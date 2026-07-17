//! GitHub source for navi.
//!
//! Polls the Notifications API to learn which PRs have activity, then fetches each
//! PR's reviews/comments and diffs them (via `navi-notifier-forge`) against a
//! persisted snapshot to derive normalized events at a granularity
//! (reply-to-your-comment, re-review, dismissal) the raw notification `reason`
//! can't provide.

mod notification;
mod source;

pub use source::{GitHubSource, GitHubSourceConfig};
