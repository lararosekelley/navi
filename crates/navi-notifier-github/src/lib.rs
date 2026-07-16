//! GitHub source for navi.
//!
//! Polls the Notifications API as a *trigger* to learn which PRs have activity, then
//! fetches each PR's reviews/comments and diffs them against a persisted snapshot to
//! derive precise, normalized [`Event`](navi_notifier_core::model::Event)s — the level of
//! granularity (reply-to-*your*-comment, re-review, dismissal) the raw notification
//! `reason` can't provide.

mod api;
mod diff;
mod snapshot;
mod source;

pub use source::{GitHubSource, GitHubSourceConfig};
