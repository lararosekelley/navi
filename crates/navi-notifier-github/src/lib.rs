//! GitHub source for navi.
//!
//! Polls the Notifications API to learn which PRs have activity, then fetches each
//! PR's reviews/comments and diffs them against a persisted snapshot to derive
//! normalized [`Event`](navi_notifier_core::model::Event)s at a granularity
//! (reply-to-your-comment, re-review, dismissal) the raw notification `reason`
//! can't provide.

mod api;
mod diff;
mod snapshot;
mod source;

pub use source::{GitHubSource, GitHubSourceConfig};
