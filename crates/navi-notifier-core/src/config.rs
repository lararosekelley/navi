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

/// Repository allow/deny filtering. Patterns are `owner/name` with an optional
/// trailing `/*` wildcard on the name (e.g. `acme/*` matches all repos in `acme`).
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

/// Matches `owner/name` against a pattern that may end in `/*`.
fn pattern_matches(pattern: &str, full_name: &str) -> bool {
    if let Some(owner_prefix) = pattern.strip_suffix("/*") {
        full_name
            .split_once('/')
            .is_some_and(|(owner, _)| owner == owner_prefix)
    } else {
        pattern == full_name
    }
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

/// A pattern mute: suppress events whose `field` matches `pattern`. With
/// `regex = false` it's a case-insensitive substring match; with `regex = true`
/// it's a full regex (use `(?i)` for case-insensitivity there).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MuteRule {
    #[serde(rename = "match")]
    pub field: MuteField,
    pub pattern: String,
    #[serde(default)]
    pub regex: bool,
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
