//! Provider-agnostic domain model.
//!
//! Every source (GitHub, GitLab, Gitea, ...) normalizes its native payloads
//! into these types so that the engine, rule layer, and destinations never need to
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
    /// Avatar URL, used for richer destination rendering.
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

/// Why a PR left the merge queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeQueueRemoval {
    /// Removed while still mergeable (manually dequeued, or the queue was cleared).
    Dequeued,
    /// Kicked out because it could no longer merge (failed checks or conflicts).
    Unmergeable,
}

/// How much pre-existing activity to surface the first time navi polls (before it
/// has any stored state). Later polls always diff against stored snapshots, so this
/// only governs the initial catch-up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backfill {
    /// Baseline everything silently: not even outstanding review asks alert. The
    /// config value stays `"none"`; the variant is `Silent` to avoid colliding with
    /// `Option::None` in the diff engine's match.
    #[serde(rename = "none")]
    Silent,
    /// Surface only PRs currently awaiting your review (the default; the useful
    /// minimum, and what navi has always done on first run).
    #[default]
    ReviewRequests,
    /// Surface all derivable activity on every open PR you're involved in. Noisy on
    /// a busy account; relies on the involved-PR sweep (`track_prs`).
    AllOpen,
}

/// The kind of thing that happened. This is the taxonomy the rule layer filters on
/// and the destination renders. Most variants are lightweight; the richer payload
/// (excerpt, urls, actor) lives on [`Event`].
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
    /// The PR entered the merge queue.
    EnteredMergeQueue,
    /// The PR was removed from the merge queue (dequeued or became unmergeable).
    RemovedFromMergeQueue { reason: MergeQueueRemoval },
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
            EventKind::EnteredMergeQueue => "entered_merge_queue",
            EventKind::RemovedFromMergeQueue { .. } => "removed_merge_queue",
        }
    }

    /// All tags identifying this event for delivery config that needs finer grain
    /// than `tag()` (e.g. a destination's broadcast set). Most events match only
    /// their `tag()`. A review submission also matches a per-state tag
    /// (`review_approved` / `review_changes_requested` / `review_commented`) so a
    /// config can single out approvals and change requests without also matching
    /// the noisier plain review comments. Listing the umbrella `review_submitted`
    /// still matches every state (backward compatible).
    pub fn match_tags(&self) -> Vec<&'static str> {
        match self {
            EventKind::ReviewSubmitted { state } => {
                let state_tag = match state {
                    ReviewState::Approved => "review_approved",
                    ReviewState::ChangesRequested => "review_changes_requested",
                    ReviewState::Commented => "review_commented",
                };
                vec![self.tag(), state_tag]
            }
            _ => vec![self.tag()],
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
    /// The actor of this event is the viewer themselves (self-action).
    pub actor_is_viewer: bool,
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
    /// Display name for the actor, collapsing self-actions to "you" so a
    /// notification about your own action doesn't read as a third party.
    pub fn actor_label(&self) -> &str {
        if self.viewer.actor_is_viewer {
            "you"
        } else {
            self.actor.label()
        }
    }

    /// How to refer to the PR in a headline, from the viewer's angle: "your PR"
    /// when you authored it, "their own PR" when the actor is the PR's author
    /// (so it doesn't repeat the name, e.g. "octo merged octo's PR"), otherwise
    /// "<author>'s PR" - so activity on a PR you only review isn't mislabeled.
    pub fn pr_phrase(&self) -> String {
        if self.viewer.is_author {
            "your PR".to_string()
        } else if self
            .actor
            .login
            .eq_ignore_ascii_case(&self.pull_request.author.login)
        {
            "their own PR".to_string()
        } else {
            format!("{}'s PR", self.pull_request.author.label())
        }
    }

    /// Like [`Event::pr_phrase`], but for headlines with no preceding actor mention
    /// (e.g. "… entered the merge queue"): names the PR's author directly instead of
    /// collapsing to "their own PR", which reads oddly with nothing for "their" to
    /// refer back to. Still "your PR" when you authored it.
    pub fn pr_owner_phrase(&self) -> String {
        if self.viewer.is_author {
            "your PR".to_string()
        } else {
            format!("{}'s PR", self.pull_request.author.label())
        }
    }

    /// Provider-stable per-PR key (`owner/repo#number`). Groups an event with the
    /// pull request it came from, so the engine can advance per-PR state as a unit.
    pub fn scope(&self) -> String {
        format!(
            "{}#{}",
            self.pull_request.repo.full_name(),
            self.pull_request.number
        )
    }

    /// Stable per-PR key a destination uses to group its messages into one thread.
    /// Includes the source so a GitHub and a GitLab PR that share an `owner/repo#n`
    /// don't collapse together.
    pub fn thread_key(&self) -> String {
        format!("thread:{}:{}", self.source_id, self.scope())
    }

    /// The plain-English notification sentence, shared by every destination so the
    /// wording lives in one place. `actor` is the caller's already-formatted actor
    /// token (bold for Slack/Discord, plain for email); `escape` is applied to the PR
    /// phrase so a markup-sensitive destination (Slack) can escape it while others
    /// pass it through. Emoji and colour stay destination-specific in each renderer.
    pub fn headline(&self, actor: &str, escape: impl Fn(&str) -> String) -> String {
        match &self.kind {
            EventKind::ReviewRequested => format!("{actor} requested your review"),
            EventKind::ReReviewRequested => format!("{actor} requested a re-review"),
            EventKind::ReviewSubmitted { state } => match state {
                ReviewState::Approved => format!("{actor} approved {}", escape(&self.pr_phrase())),
                ReviewState::ChangesRequested => format!("{actor} requested changes"),
                ReviewState::Commented => format!("{actor} left a review comment"),
            },
            EventKind::ReviewDismissed => format!("{actor} dismissed your review"),
            EventKind::CommentReply { on_your_comment } => {
                if *on_your_comment {
                    format!("{actor} replied to your comment")
                } else {
                    format!("{actor} replied in a thread you're in")
                }
            }
            EventKind::Mentioned => format!("{actor} mentioned you"),
            EventKind::Merged => format!("{actor} merged {}", escape(&self.pr_phrase())),
            EventKind::Closed => format!("{} was closed", escape(&self.pr_phrase())),
            EventKind::ReadyForReview => format!("{actor} marked a PR ready for review"),
            EventKind::EnteredMergeQueue => {
                format!(
                    "{} entered the merge queue",
                    escape(&self.pr_owner_phrase())
                )
            }
            EventKind::RemovedFromMergeQueue { reason } => match reason {
                MergeQueueRemoval::Dequeued => {
                    format!("{} left the merge queue", escape(&self.pr_owner_phrase()))
                }
                MergeQueueRemoval::Unmergeable => format!(
                    "{} was kicked from the merge queue (can't merge)",
                    escape(&self.pr_owner_phrase())
                ),
            },
        }
    }

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

/// Escape the three characters that HTML and Slack mrkdwn both treat specially, so
/// user-supplied text (titles, comment excerpts) can't inject markup. Shared by the
/// email (HTML) and Slack renderers, which need the same `&`/`<`/`>` entities.
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn event(kind: EventKind) -> Event {
        Event {
            source_id: "github".into(),
            kind,
            pull_request: PullRequest {
                repo: Repo::new("acme", "widgets"),
                number: 12,
                title: "Add gizmo".into(),
                url: "https://gh.test/acme/widgets/pull/12".into(),
                author: Actor::new("octo"),
                draft: false,
            },
            viewer: ViewerRelationship::default(),
            actor: Actor::new("reviewer"),
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            target_url: None,
            excerpt: None,
            dedup_key: "k".into(),
        }
    }

    #[test]
    fn match_tags_split_review_submissions_by_state() {
        let approved = EventKind::ReviewSubmitted {
            state: ReviewState::Approved,
        };
        assert_eq!(
            approved.match_tags(),
            vec!["review_submitted", "review_approved"]
        );
        let changes = EventKind::ReviewSubmitted {
            state: ReviewState::ChangesRequested,
        };
        assert_eq!(
            changes.match_tags(),
            vec!["review_submitted", "review_changes_requested"]
        );
        // Non-review kinds match only their single tag.
        assert_eq!(EventKind::Merged.match_tags(), vec!["merged"]);
    }

    #[test]
    fn pr_phrase_and_owner_phrase_reflect_authorship() {
        let mut e = event(EventKind::Merged); // viewer not author; actor=reviewer, author=octo
        assert_eq!(e.pr_phrase(), "octo's PR");
        assert_eq!(e.pr_owner_phrase(), "octo's PR");
        // The author acted on their own PR: pr_phrase collapses, pr_owner_phrase names them.
        e.actor = Actor::new("OCTO"); // case-insensitive match
        assert_eq!(e.pr_phrase(), "their own PR");
        assert_eq!(e.pr_owner_phrase(), "octo's PR");
        // The viewer authored it.
        e.viewer.is_author = true;
        assert_eq!(e.pr_phrase(), "your PR");
        assert_eq!(e.pr_owner_phrase(), "your PR");
    }

    #[test]
    fn headline_substitutes_actor_and_escapes_the_phrase() {
        let e = event(EventKind::ReviewRequested);
        assert_eq!(
            e.headline("*bob*", |s| s.to_string()),
            "*bob* requested your review"
        );
        // The escaper only touches the PR phrase (here a markup-bearing author name).
        let mut merged = event(EventKind::Merged);
        merged.pull_request.author = Actor::new("a<b>");
        assert_eq!(
            merged.headline("bob", html_escape),
            "bob merged a&lt;b&gt;'s PR"
        );
    }

    #[test]
    fn thread_key_includes_source_and_scope() {
        assert_eq!(
            event(EventKind::Merged).thread_key(),
            "thread:github:acme/widgets#12"
        );
    }

    #[test]
    fn html_escape_covers_amp_lt_gt() {
        assert_eq!(html_escape("a & b < c > d"), "a &amp; b &lt; c &gt; d");
    }
}
