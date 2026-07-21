//! The filter stage: given a normalized [`Event`] and a [`RuleConfig`], decide
//! whether it should be delivered, and why not when it shouldn't.
//!
//! Pure and synchronous so it is trivial to unit-test. Anything time- or
//! identity-dependent is passed in via [`FilterContext`] rather than read from the
//! environment, keeping this layer deterministic.

use crate::config::{pattern_matches, MuteField, MuteRule, RuleConfig, RuleOverride};
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
    /// Routes are configured, but none cover this event's repo.
    NoMatchingRoute,
}

/// Outcome of evaluating one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Deliver,
    Drop(DropReason),
}

/// A mute rule compiled once when the engine is built: one or more conditions, all
/// of which must match (AND) for the rule to fire.
#[derive(Debug, Clone)]
struct CompiledMute {
    conditions: Vec<CompiledCondition>,
}

/// One field/matcher pair within a rule.
#[derive(Debug, Clone)]
struct CompiledCondition {
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
        self.conditions.iter().all(|c| c.matches(event))
    }
}

impl CompiledCondition {
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

/// A mute rule that couldn't be compiled. Surfaced at engine-build time so the user
/// fixes their config instead of it silently never (or always) matching.
#[derive(Debug)]
pub enum InvalidMutePattern {
    /// A pattern failed to compile as a regex.
    Regex {
        pattern: String,
        source: regex::Error,
    },
    /// `match` was given without `pattern` (or vice versa).
    IncompleteMatch,
    /// `regex = true` set on a rule with no `match`/`pattern` - the top-level flag
    /// only applies to that form; flat fields use their own `<field>_regex`.
    RegexWithoutMatch,
    /// A rule with no conditions - it would match everything and mute the feed.
    Empty,
    /// A per-repo override references an unknown event tag (likely a typo), which
    /// would silently do nothing.
    UnknownEventTag(String),
}

impl std::fmt::Display for InvalidMutePattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Regex { pattern, source } => {
                write!(f, "invalid mute regex `{pattern}`: {source}")
            }
            Self::IncompleteMatch => {
                write!(f, "a mute rule sets `match` without `pattern` (or vice versa)")
            }
            Self::RegexWithoutMatch => write!(
                f,
                "`regex = true` needs a `match`/`pattern`; for flat fields use author_regex/title_regex/excerpt_regex"
            ),
            Self::Empty => write!(
                f,
                "a mute rule has no conditions; set `match`/`pattern` or an author/title/excerpt field"
            ),
            Self::UnknownEventTag(tag) => {
                write!(f, "unknown event tag `{tag}` in a rules.overrides entry")
            }
        }
    }
}

impl std::error::Error for InvalidMutePattern {}

fn compile_condition(
    field: MuteField,
    pattern: &str,
    regex: bool,
) -> Result<CompiledCondition, InvalidMutePattern> {
    let matcher = if regex {
        Matcher::Regex(
            regex::Regex::new(pattern).map_err(|source| InvalidMutePattern::Regex {
                pattern: pattern.to_string(),
                source,
            })?,
        )
    } else {
        Matcher::Substring(pattern.to_ascii_lowercase())
    };
    Ok(CompiledCondition { field, matcher })
}

fn compile_mute(rule: &MuteRule) -> Result<CompiledMute, InvalidMutePattern> {
    let mut conditions = Vec::new();
    match (rule.field, rule.pattern.as_deref()) {
        (Some(field), Some(pattern)) => {
            conditions.push(compile_condition(field, pattern, rule.regex)?)
        }
        (None, None) => {
            // `regex` only wires into match/pattern; a lone `regex = true` on a
            // flat rule would silently do nothing, so reject it.
            if rule.regex {
                return Err(InvalidMutePattern::RegexWithoutMatch);
            }
        }
        _ => return Err(InvalidMutePattern::IncompleteMatch),
    }
    if let Some(pattern) = rule.author.as_deref() {
        conditions.push(compile_condition(
            MuteField::Author,
            pattern,
            rule.author_regex,
        )?);
    }
    if let Some(pattern) = rule.title.as_deref() {
        conditions.push(compile_condition(
            MuteField::Title,
            pattern,
            rule.title_regex,
        )?);
    }
    if let Some(pattern) = rule.excerpt.as_deref() {
        conditions.push(compile_condition(
            MuteField::Excerpt,
            pattern,
            rule.excerpt_regex,
        )?);
    }
    if conditions.is_empty() {
        return Err(InvalidMutePattern::Empty);
    }
    Ok(CompiledMute { conditions })
}

/// Applies [`RuleConfig`] to events.
#[derive(Debug, Clone)]
pub struct RuleEngine {
    config: RuleConfig,
    mutes: Vec<CompiledMute>,
}

impl RuleEngine {
    /// Build the engine, validating rules once: compile mute patterns (failing on a
    /// bad regex) and reject per-repo overrides that reference unknown event tags.
    pub fn new(config: RuleConfig) -> Result<Self, InvalidMutePattern> {
        let mutes = config
            .mute
            .iter()
            .map(compile_mute)
            .collect::<Result<Vec<_>, _>>()?;
        for ovr in &config.overrides {
            for tag in ovr.events.keys() {
                if !crate::config::EventToggles::is_known_tag(tag) {
                    return Err(InvalidMutePattern::UnknownEventTag(tag.clone()));
                }
            }
        }
        Ok(Self { config, mutes })
    }

    pub fn config(&self) -> &RuleConfig {
        &self.config
    }

    /// Decide the fate of a single event. Checks run cheapest-first.
    pub fn decide(&self, event: &Event, ctx: &FilterContext) -> Decision {
        let repo = event.pull_request.repo.full_name();
        // The first override whose repos match wins; unset fields inherit global.
        let ovr = self
            .config
            .overrides
            .iter()
            .find(|o| o.repos.iter().any(|p| pattern_matches(p, &repo)));

        let tag = event.kind.tag();
        let kind_enabled = ovr
            .and_then(|o| o.events.get(tag).copied())
            .unwrap_or_else(|| self.config.events.is_enabled(tag));
        if !kind_enabled {
            return Decision::Drop(DropReason::EventKindDisabled);
        }

        if self.config.mute_authors.contains(&event.actor.login) {
            return Decision::Drop(DropReason::AuthorMuted);
        }

        if self.mutes.iter().any(|m| m.matches(event)) {
            return Decision::Drop(DropReason::Muted);
        }

        if !self.config.repos.permits(&repo) {
            return Decision::Drop(DropReason::RepoFiltered);
        }

        // Merge/close are only interesting depending on your relationship to the PR.
        if matches!(event.kind, EventKind::Merged | EventKind::Closed) {
            let g = &self.config.merge_close;
            let author = ovr.and_then(|o| o.merge_close.author).unwrap_or(g.author);
            let reviewer = ovr
                .and_then(|o| o.merge_close.reviewer)
                .unwrap_or(g.reviewer);
            let wanted =
                (author && event.viewer.is_author) || (reviewer && event.viewer.is_reviewer);
            if !wanted {
                return Decision::Drop(DropReason::MergeCloseScope);
            }
        }

        if self.in_quiet_hours(ovr, ctx) {
            return Decision::Drop(DropReason::QuietHours);
        }

        Decision::Deliver
    }

    fn in_quiet_hours(&self, ovr: Option<&RuleOverride>, ctx: &FilterContext) -> bool {
        let qh = &self.config.quiet_hours;
        let enabled = ovr
            .and_then(|o| o.quiet_hours.enabled)
            .unwrap_or(qh.enabled);
        if !enabled {
            return false;
        }
        let start_s = ovr
            .and_then(|o| o.quiet_hours.start.as_deref())
            .unwrap_or(&qh.start);
        let end_s = ovr
            .and_then(|o| o.quiet_hours.end.as_deref())
            .unwrap_or(&qh.end);
        let (Some(now), Some(start), Some(end)) =
            (ctx.local_minutes, parse_hhmm(start_s), parse_hhmm(end_s))
        else {
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
        EventToggles, MergeCloseOverride, MergeCloseScope, MuteField, MuteRule, QuietHours,
        QuietHoursOverride, RepoFilter,
    };
    use crate::model::{Actor, PullRequest, Repo, ReviewState, ViewerRelationship};
    use std::collections::BTreeMap;
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
                field: Some(field),
                pattern: Some(pattern.into()),
                regex,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn mute_rules(rules: Vec<MuteRule>) -> RuleConfig {
        RuleConfig {
            mute: rules,
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
    fn multi_condition_rule_requires_all_to_match() {
        // Mute the bot's CI chatter only: author AND excerpt must both match.
        let engine = RuleEngine::new(mute_rules(vec![MuteRule {
            author: Some("github-actions[bot]".into()),
            excerpt: Some("CircleCI pipeline triggered".into()),
            ..Default::default()
        }]))
        .unwrap();

        let mut bot_ci = event(EventKind::Mentioned);
        bot_ci.actor = Actor::new("github-actions[bot]");
        bot_ci.excerpt = Some("CircleCI pipeline triggered for abc123".into());
        assert_eq!(
            engine.decide(&bot_ci, &FilterContext::default()),
            Decision::Drop(DropReason::Muted)
        );

        // Same author, different text → not muted (the AND fails).
        let mut bot_review = event(EventKind::Mentioned);
        bot_review.actor = Actor::new("github-actions[bot]");
        bot_review.excerpt = Some("Claude finished the review".into());
        assert_eq!(
            engine.decide(&bot_review, &FilterContext::default()),
            Decision::Deliver
        );

        // Matching text from a human → not muted (author condition fails).
        let mut human = event(EventKind::Mentioned);
        human.actor = Actor::new("lara");
        human.excerpt = Some("CircleCI pipeline triggered, fyi".into());
        assert_eq!(
            engine.decide(&human, &FilterContext::default()),
            Decision::Deliver
        );
    }

    #[test]
    fn per_field_regex_flag_applies() {
        let engine = RuleEngine::new(mute_rules(vec![MuteRule {
            author: Some(r"\[bot\]$".into()),
            author_regex: true,
            ..Default::default()
        }]))
        .unwrap();
        let mut bot = event(EventKind::Mentioned);
        bot.actor = Actor::new("dependabot[bot]");
        assert_eq!(
            engine.decide(&bot, &FilterContext::default()),
            Decision::Drop(DropReason::Muted)
        );
    }

    #[test]
    fn empty_mute_rule_is_a_config_error() {
        // A rule with no conditions would match everything; reject it at build time.
        assert!(RuleEngine::new(mute_rules(vec![MuteRule::default()])).is_err());
    }

    #[test]
    fn match_without_pattern_is_a_config_error() {
        assert!(RuleEngine::new(mute_rules(vec![MuteRule {
            field: Some(MuteField::Title),
            ..Default::default()
        }]))
        .is_err());
    }

    #[test]
    fn top_level_regex_on_a_flat_rule_is_a_config_error() {
        // `regex = true` only applies to match/pattern; on a flat rule it would
        // silently do nothing (flat fields use `<field>_regex`), so reject it.
        assert!(RuleEngine::new(mute_rules(vec![MuteRule {
            author: Some(".*bot.*".into()),
            regex: true,
            ..Default::default()
        }]))
        .is_err());
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
        assert!(EventToggles::default().ready_for_review);
        assert_eq!(
            engine.decide(
                &event(EventKind::ReviewRequested),
                &FilterContext::default()
            ),
            Decision::Deliver
        );
    }

    /// Build an event for a specific repo (helper for override tests).
    fn event_in(kind: EventKind, owner: &str, name: &str) -> Event {
        let mut e = event(kind);
        e.pull_request.repo = Repo::new(owner, name);
        e
    }

    #[test]
    fn per_repo_override_flips_an_event_toggle() {
        let cfg = RuleConfig {
            overrides: vec![RuleOverride {
                repos: vec!["acme/*".into()],
                events: BTreeMap::from([("mentioned".to_string(), false)]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let engine = RuleEngine::new(cfg).unwrap();
        // acme repo: the override turns `mentioned` off.
        assert_eq!(
            engine.decide(
                &event_in(EventKind::Mentioned, "acme", "widgets"),
                &FilterContext::default()
            ),
            Decision::Drop(DropReason::EventKindDisabled)
        );
        // A different repo inherits the global default (mentioned on).
        assert_eq!(
            engine.decide(
                &event_in(EventKind::Mentioned, "other", "thing"),
                &FilterContext::default()
            ),
            Decision::Deliver
        );
    }

    #[test]
    fn per_repo_override_narrows_merge_close_scope() {
        let cfg = RuleConfig {
            overrides: vec![RuleOverride {
                repos: vec!["acme/*".into()],
                merge_close: MergeCloseOverride {
                    reviewer: Some(false),
                    author: None,
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let engine = RuleEngine::new(cfg).unwrap();
        // A merged PR you only reviewed, in acme: reviewer scope off here → dropped.
        let mut acme = event_in(EventKind::Merged, "acme", "widgets");
        acme.viewer.is_reviewer = true;
        assert_eq!(
            engine.decide(&acme, &FilterContext::default()),
            Decision::Drop(DropReason::MergeCloseScope)
        );
        // Elsewhere the global reviewer scope (on) still delivers it.
        let mut other = event_in(EventKind::Merged, "other", "thing");
        other.viewer.is_reviewer = true;
        assert_eq!(
            engine.decide(&other, &FilterContext::default()),
            Decision::Deliver
        );
    }

    #[test]
    fn per_repo_override_can_re_enable_a_globally_disabled_event() {
        // The primary use case: a kind that's off globally, back on for some repos.
        let mut cfg = RuleConfig::default();
        cfg.events.ready_for_review = false;
        cfg.overrides = vec![RuleOverride {
            repos: vec!["acme/*".into()],
            events: BTreeMap::from([("ready_for_review".to_string(), true)]),
            ..Default::default()
        }];
        let engine = RuleEngine::new(cfg).unwrap();
        // acme: the override turns it back on.
        assert_eq!(
            engine.decide(
                &event_in(EventKind::ReadyForReview, "acme", "widgets"),
                &FilterContext::default()
            ),
            Decision::Deliver
        );
        // Elsewhere it stays off.
        assert_eq!(
            engine.decide(
                &event_in(EventKind::ReadyForReview, "other", "thing"),
                &FilterContext::default()
            ),
            Decision::Drop(DropReason::EventKindDisabled)
        );
    }

    #[test]
    fn override_with_unknown_event_tag_is_a_config_error() {
        let cfg = RuleConfig {
            overrides: vec![RuleOverride {
                repos: vec!["acme/*".into()],
                events: BTreeMap::from([("reivew_submitted".to_string(), false)]), // typo
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(RuleEngine::new(cfg).is_err());
    }

    #[test]
    fn per_repo_override_can_disable_quiet_hours() {
        let cfg = RuleConfig {
            quiet_hours: QuietHours {
                enabled: true,
                start: "09:00".into(),
                end: "17:00".into(),
            },
            overrides: vec![RuleOverride {
                repos: vec!["acme/*".into()],
                quiet_hours: QuietHoursOverride {
                    enabled: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let engine = RuleEngine::new(cfg).unwrap();
        let ctx = FilterContext {
            local_minutes: Some(600), // 10:00, inside the global quiet window
        };
        // acme: quiet hours disabled by the override → delivered.
        assert_eq!(
            engine.decide(&event_in(EventKind::Mentioned, "acme", "widgets"), &ctx),
            Decision::Deliver
        );
        // Elsewhere the global quiet window suppresses it.
        assert_eq!(
            engine.decide(&event_in(EventKind::Mentioned, "other", "thing"), &ctx),
            Decision::Drop(DropReason::QuietHours)
        );
    }
}
