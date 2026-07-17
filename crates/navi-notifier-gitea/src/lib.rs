//! Gitea/Forgejo source for navi.
//!
//! Forgejo is a Gitea fork with the same API, so one crate serves both. The API is
//! GitHub-shaped (notifications, PRs, reviews, comments), so this maps Gitea's
//! payloads into `navi-notifier-forge` and reuses its diff engine.

mod api;
mod source;

pub use source::{GiteaSource, GiteaSourceConfig};
