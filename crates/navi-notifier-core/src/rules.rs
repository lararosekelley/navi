//! The filter stage: given a normalized [`Event`] and a [`RuleConfig`], decide
//! whether it should be delivered, and why not when it shouldn't.
//!
//! Pure and synchronous so it is trivial to unit-test. Anything time- or
//! identity-dependent is passed in via [`FilterContext`] rather than read from the
//! environment, keeping this layer deterministic.

use crate::config::{MuteField, MuteRule, RuleConfig};
use crate::model::{Event, EventKind};

/// Ambient inputs the filter needs but that don't belong to the event itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct FilterContext {
    /// Current local time as minutes since midnight (0..1440), used for quiet hours.
    /// `None` disables quiet-hours evaluation (the caller couldn't determine local time).
    pub local_minutes: Option<u16>,
}

/// Why an event was dropped. Surfaced in logs and `--dry-run` output so the user
/// can understand why something didn't ping them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    EventKindDisabled,
    RepoFiltered,
    AuthorMuted,
    Muted,
    QuietHours,
    MergeCloseScope,
}

/// Outcome of evaluating one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Deliver,
    Drop(DropReason),
}

/// A mute rule compiled once when the engine is built.
#[derive(Debug, Clone)]
struct CompiledMute {
    field: MuteField,
    matcher: Matcher,
}

#[derive(Debug, Clone)]
enum Matcher {
    /// Case-insensitive substring; the needle is stored lowercased.
    Substring(String),
    Regex(regex::Regex),
}

impl CompiledMute {
    fn matches(&self, event: &Event) -> bool {
        let hay = match self.field {
            MuteField::Author => event.actor.login.as_str(),
            MuteField::Title => event.pull_request.title.as_str(),
            MuteField::Excerpt => event.excerpt.as_deref().unwrap_or(""),
        };
        match &self.matcher {
            Matcher::Substring(needle) => hay.to_ascii_lowercase().contains(needle.as_str()),
            Matcher::Regex(re) => re.is_match(hay),
        }
    }
}

/// A mute rule that didn't compile (bad regex). Surfaced at engine-build time so
/// the user fixes their config instead of it silently never matching.
#[derive(Debug)]
pub struct InvalidMutePattern {
    pub pattern: String,
    pub source: regex::Error,
}

impl std::fmt::Display for InvalidMutePattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid mute regex `{}`: {}", self.pattern, self.source)
    }
}

impl std::error::Error for InvalidMutePattern {}

fn compile_mute(rule: &MuteRule) -> Result<CompiledMute, InvalidMutePattern> {
    let matcher = if rule.regex {
        Matcher::Regex(
            regex::Regex::new(&rule.pattern).map_err(|source| InvalidMutePattern {
                pattern: rule.pattern.clone(),
                source,
            })?,
        )
    } else {
        Matcher::Substring(rule.pattern.to_ascii_lowercase())
    };
    Ok(CompiledMute {
        field: rule.field,
        matcher,
    })
}

/// Applies [`RuleConfig`] to events.
#[derive(Debug, Clone)]
pub struct RuleEngine {
    config: RuleConfig,
    mutes: Vec<CompiledMute>,
}

impl RuleEngine {
    /// Build the engine, compiling any mute patterns once. Fails on a bad regex.
    pub fn new(config: RuleConfig) -> Result<Self, InvalidMutePattern> {
        let mutes = config
            .mute
            .iter()
            .map(compile_mute)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { config, mutes })
    }

    pub fn config(&self) -> &RuleConfig {
        &self.config
    }

    /// Decide the fate of a single event. Checks run cheapest-first.
    pub fn decide(&self, event: &Event, ctx: &FilterContext) -> Decision {
        if !self.config.events.is_enabled(event.kind.tag()) {
            return Decision::Drop(DropReason::EventKindDisabled);
        }

        if self.config.mute_authors.contains(&event.actor.login) {
            return Decision::Drop(DropReason::AuthorMuted);
        }

        if self.mutes.iter().any(|m| m.matches(event)) {
            return Decision::Drop(DropReason::Muted);
        }

        if !self
            .config
            .repos
            .permits(&event.pull_request.repo.full_name())
        {
            return Decision::Drop(DropReason::RepoFiltered);
        }

        // Merge/close are only interesting depending on your relationship to the PR.
        if matches!(event.kind, EventKind::Merged | EventKind::Closed) {
            let scope = &self.config.merge_close;
            let wanted = (scope.author && event.viewer.is_author)
                || (scope.reviewer && event.viewer.is_reviewer);
            if !wanted {
                return Decision::Drop(DropReason::MergeCloseScope);
            }
        }

        if self.in_quiet_hours(ctx) {
            return Decision::Drop(DropReason::QuietHours);
        }

        Decision::Deliver
    }

    fn in_quiet_hours(&self, ctx: &FilterContext) -> bool {
        let qh = &self.config.quiet_hours;
        if !qh.enabled {
            return false;
        }
        let (Some(now), Some(start), Some(end)) = (
            ctx.local_minutes,
            parse_hhmm(&qh.start),
            parse_hhmm(&qh.end),
        ) else {
            return false;
        };
        if start == end {
            return false;
        }
        if start < end {
            // Same-day window, e.g. 09:00 to 17:00.
            now >= start && now < end
        } else {
            // Wraps midnight, e.g. 22:00 to 08:00.
            now >= start || now < end
        }
    }
}

/// Parse `"HH:MM"` into minutes since midnight. Returns `None` on malformed input.
fn parse_hhmm(s: &str) -> Option<u16> {
    let (h, m) = s.split_once(':')?;
    let h: u16 = h.parse().ok()?;
    let m: u16 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        EventToggles, MergeCloseScope, MuteField, MuteRule, QuietHours, RepoFilter,
    };
    use crate::model::{Actor, PullRequest, Repo, ReviewState, ViewerRelationship};
    use time::OffsetDateTime;

    fn event(kind: EventKind) -> Event {
        Event {
            source_id: "github".into(),
            kind,
            pull_request: PullRequest {
                repo: Repo::new("acme", "widgets"),
                number: 12,
                title: "Add gizmo".into(),
                url: "https://example.test/pr/12".into(),
                author: Actor::new("octocat"),
                draft: false,
            },
            viewer: ViewerRelationship::default(),
            actor: Actor::new("reviewer1"),
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            target_url: None,
            excerpt: None,
            dedup_key: "github:acme/widgets#12:test".into(),
        }
    }

    #[test]
    fn disabled_event_kind_is_dropped() {
        let mut cfg = RuleConfig::default();
        cfg.events.review_submitted = false;
        let engine = RuleEngine::new(cfg).unwrap();
        let e = event(EventKind::ReviewSubmitted {
            state: ReviewState::Approved,
        });
        assert_eq!(
            engine.decide(&e, &FilterContext::default()),
            Decision::Drop(DropReason::EventKindDisabled)
        );
    }

    #[test]
    fn muted_author_is_dropped() {
        let mut cfg = RuleConfig::default();
        cfg.mute_authors.insert("reviewer1".into());
        let engine = RuleEngine::new(cfg).unwrap();
        assert_eq!(
            engine.decide(&event(EventKind::Mentioned), &FilterContext::default()),
            Decision::Drop(DropReason::AuthorMuted)
        );
    }

    fn mute(field: MuteField, pattern: &str, regex: bool) -> RuleConfig {
        RuleConfig {
            mute: vec![MuteRule {
                field,
                pattern: pattern.into(),
                regex,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn mute_pattern_by_author_substring() {
        let engine = RuleEngine::new(mute(MuteField::Author, "[bot]", false)).unwrap();
        let mut bot = event(EventKind::Mentioned);
        bot.actor = Actor::new("dependabot[bot]");
        assert_eq!(
            engine.decide(&bot, &FilterContext::default()),
            Decision::Drop(DropReason::Muted)
        );
        // A human actor is unaffected.
        assert_eq!(
            engine.decide(&event(EventKind::Mentioned), &FilterContext::default()),
            Decision::Deliver
        );
    }

    #[test]
    fn mute_pattern_by_author_regex() {
        let engine = RuleEngine::new(mute(MuteField::Author, r"(?i)\[bot\]$", true)).unwrap();
        let mut bot = event(EventKind::Mentioned);
        bot.actor = Actor::new("dependabot[bot]");
        assert_eq!(
            engine.decide(&bot, &FilterContext::default()),
            Decision::Drop(DropReason::Muted)
        );
        // A login that merely contains "bot" mid-string doesn't match the anchor.
        let mut human = event(EventKind::Mentioned);
        human.actor = Actor::new("botanist");
        assert_eq!(
            engine.decide(&human, &FilterContext::default()),
            Decision::Deliver
        );
    }

    #[test]
    fn mute_pattern_by_title_regex() {
        let engine = RuleEngine::new(mute(MuteField::Title, r"^Bump ", true)).unwrap();
        let mut e = event(EventKind::ReviewRequested);
        e.pull_request.title = "Bump serde to 1.2".into();
        assert_eq!(
            engine.decide(&e, &FilterContext::default()),
            Decision::Drop(DropReason::Muted)
        );
        // "Add gizmo" doesn't match.
        assert_eq!(
            engine.decide(
                &event(EventKind::ReviewRequested),
                &FilterContext::default()
            ),
            Decision::Deliver
        );
    }

    #[test]
    fn mute_pattern_by_excerpt() {
        let engine = RuleEngine::new(mute(MuteField::Excerpt, "coverage", false)).unwrap();
        let mut e = event(EventKind::Mentioned);
        e.excerpt = Some("Coverage decreased by 0.1%".into());
        assert_eq!(
            engine.decide(&e, &FilterContext::default()),
            Decision::Drop(DropReason::Muted)
        );
    }

    #[test]
    fn bad_mute_regex_is_a_config_error() {
        assert!(RuleEngine::new(mute(MuteField::Title, "(unclosed", true)).is_err());
    }

    #[test]
    fn repo_allowlist_filters() {
        let cfg = RuleConfig {
            repos: RepoFilter {
                allow: vec!["other/*".into()],
                deny: vec![],
            },
            ..Default::default()
        };
        let engine = RuleEngine::new(cfg).unwrap();
        assert_eq!(
            engine.decide(&event(EventKind::Mentioned), &FilterContext::default()),
            Decision::Drop(DropReason::RepoFiltered)
        );
    }

    #[test]
    fn repo_wildcard_allows() {
        let cfg = RuleConfig {
            repos: RepoFilter {
                allow: vec!["acme/*".into()],
                deny: vec![],
            },
            ..Default::default()
        };
        let engine = RuleEngine::new(cfg).unwrap();
        assert_eq!(
            engine.decide(&event(EventKind::Mentioned), &FilterContext::default()),
            Decision::Deliver
        );
    }

    #[test]
    fn merge_dropped_when_not_related() {
        let cfg = RuleConfig {
            merge_close: MergeCloseScope {
                author: true,
                reviewer: true,
            },
            ..Default::default()
        };
        let engine = RuleEngine::new(cfg).unwrap();
        // Viewer neither authored nor reviewed → dropped.
        assert_eq!(
            engine.decide(&event(EventKind::Merged), &FilterContext::default()),
            Decision::Drop(DropReason::MergeCloseScope)
        );
    }

    #[test]
    fn merge_delivered_when_author() {
        let engine = RuleEngine::new(RuleConfig::default()).unwrap();
        let mut e = event(EventKind::Merged);
        e.viewer.is_author = true;
        assert_eq!(
            engine.decide(&e, &FilterContext::default()),
            Decision::Deliver
        );
    }

    #[test]
    fn quiet_hours_wrapping_midnight() {
        let cfg = RuleConfig {
            quiet_hours: QuietHours {
                enabled: true,
                start: "22:00".into(),
                end: "08:00".into(),
            },
            ..Default::default()
        };
        let engine = RuleEngine::new(cfg).unwrap();
        let e = event(EventKind::Mentioned);
        // 23:00 is inside the quiet window.
        let ctx = FilterContext {
            local_minutes: Some(23 * 60),
        };
        assert_eq!(
            engine.decide(&e, &ctx),
            Decision::Drop(DropReason::QuietHours)
        );
        // 12:00 is outside it.
        let ctx = FilterContext {
            local_minutes: Some(12 * 60),
        };
        assert_eq!(engine.decide(&e, &ctx), Decision::Deliver);
    }

    #[test]
    fn default_toggles_allow_common_events() {
        let engine = RuleEngine::new(RuleConfig::default()).unwrap();
        assert!(EventToggles::default().review_requested);
        assert_eq!(
            engine.decide(
                &event(EventKind::ReviewRequested),
                &FilterContext::default()
            ),
            Decision::Deliver
        );
    }
}
