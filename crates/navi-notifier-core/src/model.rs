//! Provider-agnostic domain model.
//!
//! Every source (GitHub today; GitLab, etc. later) normalizes its native payloads
//! into these types so that the engine, rule layer, and notifiers never need to
//! know which provider an event came from.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A person who performed an action (opened a PR, left a review, replied, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    /// Stable handle, e.g. a GitHub login.
    pub login: String,
    /// Human display name when the provider supplies one.
    pub display_name: Option<String>,
    /// Avatar URL, used for richer notifier rendering.
    pub avatar_url: Option<String>,
}

impl Actor {
    pub fn new(login: impl Into<String>) -> Self {
        Self {
            login: login.into(),
            display_name: None,
            avatar_url: None,
        }
    }

    /// Best label to show a human: display name if present, else the login.
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.login)
    }
}

/// A repository the pull request lives in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub owner: String,
    pub name: String,
    /// The provider's canonical web URL for the repo.
    pub url: Option<String>,
}

impl Repo {
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            name: name.into(),
            url: None,
        }
    }

    /// `owner/name`, the form used in config filters and dedup keys.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// A pull request (or merge request) the event concerns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub repo: Repo,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: Actor,
    pub draft: bool,
}

/// The outcome a reviewer submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
}

/// The kind of thing that happened. This is the taxonomy the rule layer filters on
/// and the notifier renders. Discriminant-only variants keep matching cheap; payload
/// detail lives on [`Event`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    /// Someone requested your review on a PR you had not been asked to review.
    ReviewRequested,
    /// Your review was requested again after you had already reviewed.
    ReReviewRequested,
    /// A reviewer submitted a review.
    ReviewSubmitted { state: ReviewState },
    /// A review you submitted was dismissed.
    ReviewDismissed,
    /// Someone replied in a review/comment thread you participated in.
    CommentReply {
        /// True when the reply lands directly on a comment you authored (vs. merely
        /// a thread you're subscribed to). Lets rules prioritise direct replies.
        on_your_comment: bool,
    },
    /// You were @-mentioned.
    Mentioned,
    /// The PR was merged.
    Merged,
    /// The PR was closed without merging.
    Closed,
    /// A draft PR was marked ready for review.
    ReadyForReview,
}

impl EventKind {
    /// Stable machine tag used for config toggles and dedup keys.
    /// Kept in sync with the serde `snake_case` tag.
    pub fn tag(&self) -> &'static str {
        match self {
            EventKind::ReviewRequested => "review_requested",
            EventKind::ReReviewRequested => "re_review_requested",
            EventKind::ReviewSubmitted { .. } => "review_submitted",
            EventKind::ReviewDismissed => "review_dismissed",
            EventKind::CommentReply { .. } => "comment_reply",
            EventKind::Mentioned => "mentioned",
            EventKind::Merged => "merged",
            EventKind::Closed => "closed",
            EventKind::ReadyForReview => "ready_for_review",
        }
    }
}

/// How the person running navi ("the viewer") relates to the PR. The source sets
/// this since it alone knows the authenticated identity; rules (e.g. merge/close
/// scope) read it without needing to know the viewer's login.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerRelationship {
    /// The viewer authored the PR.
    pub is_author: bool,
    /// The viewer is (or was) a requested reviewer or has reviewed.
    pub is_reviewer: bool,
}

/// A fully normalized event ready for filtering and delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Id of the source that produced this event (e.g. `"github"`).
    pub source_id: String,
    pub kind: EventKind,
    pub pull_request: PullRequest,
    /// The viewer's relationship to the PR this event concerns.
    pub viewer: ViewerRelationship,
    /// Who performed the action.
    pub actor: Actor,
    /// When it happened, per the provider.
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
    /// Deep link to the specific artifact (comment, review, …) when narrower than the PR URL.
    pub target_url: Option<String>,
    /// Short human-facing excerpt (e.g. the first line of a comment).
    pub excerpt: Option<String>,
    /// Stable key for idempotent delivery. Two runs that observe the same underlying
    /// action must produce the same key so the state store can suppress duplicates.
    pub dedup_key: String,
}

impl Event {
    /// Convenience for building a dedup key from provider-stable parts.
    /// Callers should feed identifiers that never change for a given action
    /// (e.g. `github:owner/repo#12:review:456789`).
    pub fn make_dedup_key(
        source_id: &str,
        repo: &Repo,
        pr_number: u64,
        discriminator: &str,
    ) -> String {
        format!(
            "{}:{}#{}:{}",
            source_id,
            repo.full_name(),
            pr_number,
            discriminator
        )
    }
}
