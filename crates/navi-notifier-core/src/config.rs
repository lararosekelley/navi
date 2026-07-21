//! Configuration for the rule/filter layer.
//!
//! Provider auth (tokens, API bases) is not here; that belongs to each provider
//! crate's own config. This module only describes how events are filtered and
//! prioritised once normalized.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Per-`EventKind` on/off switches, keyed by [`crate::model::EventKind::tag`].
/// Everything defaults to enabled; a user opts *out* of noise.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EventToggles {
    pub review_requested: bool,
    pub re_review_requested: bool,
    pub review_submitted: bool,
    pub review_dismissed: bool,
    pub comment_reply: bool,
    pub mentioned: bool,
    pub merged: bool,
    pub closed: bool,
    pub ready_for_review: bool,
}

impl Default for EventToggles {
    fn default() -> Self {
        Self {
            review_requested: true,
            re_review_requested: true,
            review_submitted: true,
            review_dismissed: true,
            comment_reply: true,
            mentioned: true,
            merged: true,
            closed: true,
            ready_for_review: true,
        }
    }
}

impl EventToggles {
    /// Whether an event tag is enabled.
    pub fn is_enabled(&self, tag: &str) -> bool {
        match tag {
            "review_requested" => self.review_requested,
            "re_review_requested" => self.re_review_requested,
            "review_submitted" => self.review_submitted,
            "review_dismissed" => self.review_dismissed,
            "comment_reply" => self.comment_reply,
            "mentioned" => self.mentioned,
            "merged" => self.merged,
            "closed" => self.closed,
            "ready_for_review" => self.ready_for_review,
            // Unknown tags are allowed through so new event kinds aren't silently dropped.
            _ => true,
        }
    }
}

/// Repository allow/deny filtering. Patterns are `owner/name`, where either side
/// may use `*`: a whole owner (`acme/*`), a name prefix (`acme/tmp-*` matches
/// `acme/tmp-123`), or any owner (`*/tmp-*`). GitHub names can't contain `*`, so a
/// `*` is always a wildcard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoFilter {
    /// If non-empty, only repos matching one of these patterns are allowed.
    pub allow: Vec<String>,
    /// Repos matching any of these are always dropped (takes precedence over allow).
    pub deny: Vec<String>,
}

impl RepoFilter {
    pub fn permits(&self, full_name: &str) -> bool {
        if self.deny.iter().any(|p| pattern_matches(p, full_name)) {
            return false;
        }
        if self.allow.is_empty() {
            return true;
        }
        self.allow.iter().any(|p| pattern_matches(p, full_name))
    }
}

/// Matches an `owner/name` repo against a pattern. Each side is matched
/// independently: `*` on the owner matches any owner; a name ending in `*` is a
/// prefix match (`*` alone matches any name), otherwise both sides are exact. A
/// pattern without a `/` can never match a repo. Shared with the engine's route
/// matching so repo globs behave identically in filters and routing.
pub(crate) fn pattern_matches(pattern: &str, full_name: &str) -> bool {
    let (Some((owner_pat, name_pat)), Some((owner, name))) =
        (pattern.split_once('/'), full_name.split_once('/'))
    else {
        return false;
    };
    let owner_ok = owner_pat == "*" || owner_pat == owner;
    let name_ok = match name_pat.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name_pat == name,
    };
    owner_ok && name_ok
}

/// Quiet hours during which non-urgent events are suppressed. Times are `HH:MM` in
/// the machine's local time. A window that wraps midnight (start > end) is honored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuietHours {
    pub enabled: bool,
    /// `"HH:MM"`, inclusive start of the quiet window.
    pub start: String,
    /// `"HH:MM"`, exclusive end of the quiet window.
    pub end: String,
}

/// Which merge/close events to surface, based on your relationship to the PR.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MergeCloseScope {
    /// Notify when a PR you authored is merged/closed.
    pub author: bool,
    /// Notify when a PR you reviewed (or were asked to review) is merged/closed.
    pub reviewer: bool,
}

impl Default for MergeCloseScope {
    fn default() -> Self {
        Self {
            author: true,
            reviewer: true,
        }
    }
}

/// Which field of an event a [`MuteRule`] matches against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MuteField {
    Author,
    Title,
    Excerpt,
}

/// A pattern mute: suppress events matching its condition(s). Two forms, and a
/// rule may combine them - all present conditions must match (AND):
///
/// - **Single** (legacy): `match = "author|title|excerpt"` + `pattern = "…"`.
/// - **Flat**: any of `author = "…"`, `title = "…"`, `excerpt = "…"`, each with an
///   optional `<field>_regex = true`. Use this to scope, e.g. mute a bot's noise:
///   `author = "github-actions[bot]"` **and** `excerpt = "CircleCI…"`.
///
/// Each pattern is a case-insensitive substring by default, or a full regex when
/// its `regex` flag is set (use `(?i)` for case-insensitivity there). A rule with
/// no conditions at all is a config error.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MuteRule {
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    pub field: Option<MuteField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    pub regex: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub author_regex: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub title_regex: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    pub excerpt_regex: bool,
}

/// The complete rule configuration consumed by the engine's filter stage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleConfig {
    pub events: EventToggles,
    pub repos: RepoFilter,
    /// Logins whose actions never generate notifications (e.g. bots).
    pub mute_authors: BTreeSet<String>,
    /// Pattern mutes for noisier filtering than exact logins.
    pub mute: Vec<MuteRule>,
    pub quiet_hours: QuietHours,
    pub merge_close: MergeCloseScope,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_pattern_exact_and_whole_owner() {
        assert!(pattern_matches("acme/widgets", "acme/widgets"));
        assert!(!pattern_matches("acme/widgets", "acme/gadgets"));
        // Whole-owner wildcard (backward-compatible with the old `owner/*`).
        assert!(pattern_matches("acme/*", "acme/anything"));
        assert!(!pattern_matches("acme/*", "other/anything"));
    }

    #[test]
    fn repo_pattern_name_prefix() {
        // The git-stk-e2e-* case: a name prefix under a specific owner.
        assert!(pattern_matches("acme/tmp-*", "acme/tmp-123"));
        assert!(pattern_matches("acme/tmp-*", "acme/tmp-")); // prefix boundary
        assert!(!pattern_matches("acme/tmp-*", "acme/temp-1")); // must start with the prefix
        assert!(!pattern_matches("acme/tmp-*", "other/tmp-1")); // owner still exact
    }

    #[test]
    fn repo_pattern_any_owner() {
        assert!(pattern_matches("*/tmp-*", "acme/tmp-1"));
        assert!(pattern_matches("*/tmp-*", "other/tmp-9"));
        assert!(!pattern_matches("*/tmp-*", "acme/prod-1"));
        // Any owner, exact name.
        assert!(pattern_matches("*/widgets", "acme/widgets"));
        assert!(!pattern_matches("*/widgets", "acme/gadgets"));
    }

    #[test]
    fn repo_pattern_without_slash_never_matches() {
        assert!(!pattern_matches("acme", "acme/widgets"));
    }

    #[test]
    fn repo_filter_deny_wins_and_prefix_denies() {
        let filter = RepoFilter {
            allow: Vec::new(),
            deny: vec!["me/git-stk-e2e-*".into()],
        };
        assert!(!filter.permits("me/git-stk-e2e-abc123"));
        assert!(filter.permits("me/real-project"));
    }
}
