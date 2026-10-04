//! Gitea/Forgejo REST payloads, mapped into the shared `navi-notifier-forge` model.
#![allow(dead_code)]

use navi_notifier_forge::model::{IssueComment, PrData, PullRequest, Review, User};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct GiteaUser {
    /// Gitea user objects always carry `login` (and a duplicate `username`).
    pub login: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
}

impl GiteaUser {
    fn into_forge(self) -> User {
        User {
            login: self.login,
            avatar_url: self.avatar_url,
            html_url: self.html_url,
        }
    }
}

/// One entry from `GET /notifications`.
#[derive(Debug, Clone, Deserialize)]
pub struct Notification {
    #[serde(default)]
    pub updated_at: Option<String>,
    pub subject: NotificationSubject,
    pub repository: NotificationRepo,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NotificationSubject {
    #[serde(default)]
    pub title: String,
    /// Gitea points this at the issue endpoint, e.g. `.../repos/o/r/issues/12`.
    #[serde(default)]
    pub url: Option<String>,
    /// `"Pull"`, `"Issue"`, `"Commit"`, …
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NotificationRepo {
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub html_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GiteaPull {
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub merged: bool,
    #[serde(default)]
    pub merged_at: Option<String>,
    #[serde(default)]
    pub closed_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
    pub user: Option<GiteaUser>,
    #[serde(default)]
    pub merged_by: Option<GiteaUser>,
}

impl GiteaPull {
    pub fn into_forge(self) -> PullRequest {
        PullRequest {
            number: self.number,
            title: self.title,
            html_url: self.html_url,
            state: self.state,
            draft: self.draft,
            merged: self.merged,
            merged_at: self.merged_at,
            closed_at: self.closed_at,
            updated_at: self.updated_at,
            merge_commit_sha: self.merge_commit_sha,
            user: self.user.map(GiteaUser::into_forge),
            merged_by: self.merged_by.map(GiteaUser::into_forge),
            // Filled from the reviews list by `into_pr_data`.
            requested_reviewers: Vec::new(),
            // Gitea team review requests aren't modelled yet.
            requested_teams: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GiteaReview {
    pub id: u64,
    pub user: Option<GiteaUser>,
    /// `APPROVED` | `REQUEST_CHANGES` | `COMMENT` | `PENDING`, or `REQUEST_REVIEW`
    /// for a review request that hasn't been answered yet.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub dismissed: bool,
    #[serde(default)]
    pub submitted_at: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
}

impl GiteaReview {
    pub fn into_forge(self) -> Review {
        // Normalize Gitea's review states to the forge (GitHub) vocabulary, and
        // fold Gitea's `dismissed` flag into the DISMISSED state the diff expects.
        let state = if self.dismissed {
            "DISMISSED".to_string()
        } else {
            match self.state.as_str() {
                "REQUEST_CHANGES" => "CHANGES_REQUESTED".to_string(),
                "COMMENT" => "COMMENTED".to_string(),
                other => other.to_string(),
            }
        };
        Review {
            id: self.id,
            user: self.user.map(GiteaUser::into_forge),
            state,
            submitted_at: self.submitted_at,
            html_url: self.html_url,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GiteaIssueComment {
    pub id: u64,
    pub user: Option<GiteaUser>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

impl GiteaIssueComment {
    pub fn into_forge(self) -> IssueComment {
        IssueComment {
            id: self.id,
            user: self.user.map(GiteaUser::into_forge),
            body: self.body,
            html_url: self.html_url,
            created_at: self.created_at,
        }
    }
}

/// Assemble one PR's fetched payloads into the forge model. Gitea's
/// `requested_reviewers` keeps listing a reviewer after they review (and is `null`
/// when empty), so pending requests come from the `REQUEST_REVIEW` entries in the
/// reviews list instead, and those entries are not passed on as reviews.
pub fn into_pr_data(
    pull: GiteaPull,
    reviews: Vec<GiteaReview>,
    issue_comments: Vec<GiteaIssueComment>,
) -> PrData {
    let (requests, reviews): (Vec<_>, Vec<_>) = reviews
        .into_iter()
        .partition(|r| r.state == "REQUEST_REVIEW");
    let mut pull_request = pull.into_forge();
    pull_request.requested_reviewers = requests
        .into_iter()
        .filter_map(|r| r.user.map(GiteaUser::into_forge))
        .collect();
    PrData {
        pull_request,
        reviews: reviews.into_iter().map(GiteaReview::into_forge).collect(),
        // Gitea inline review comments are per-review and lack reply threading;
        // conversation comments cover mentions and replies for now.
        review_comments: Vec::new(),
        issue_comments: issue_comments
            .into_iter()
            .map(GiteaIssueComment::into_forge)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn review(state: &str, dismissed: bool) -> GiteaReview {
        GiteaReview {
            id: 1,
            user: None,
            state: state.into(),
            dismissed,
            submitted_at: None,
            html_url: None,
        }
    }

    #[test]
    fn into_forge_normalizes_review_state() {
        assert_eq!(
            review("REQUEST_CHANGES", false).into_forge().state,
            "CHANGES_REQUESTED"
        );
        assert_eq!(review("COMMENT", false).into_forge().state, "COMMENTED");
        assert_eq!(review("APPROVED", false).into_forge().state, "APPROVED");
        // The dismissed flag wins over whatever state Gitea reports.
        assert_eq!(review("APPROVED", true).into_forge().state, "DISMISSED");
    }

    fn reviews(entries: serde_json::Value) -> Vec<GiteaReview> {
        serde_json::from_value(entries).expect("reviews")
    }

    fn pull() -> GiteaPull {
        // Gitea sends `null` rather than `[]` for an empty reviewer list.
        serde_json::from_value(json!({
            "number": 5,
            "user": { "login": "octo" },
            "requested_reviewers": null
        }))
        .expect("pull")
    }

    #[test]
    fn pending_requests_come_from_request_review_entries() {
        let data = into_pr_data(
            pull(),
            reviews(json!([
                { "id": 1, "user": { "login": "me" }, "state": "REQUEST_REVIEW" },
                { "id": 2, "user": { "login": "sam" }, "state": "APPROVED" }
            ])),
            Vec::new(),
        );
        let requested: Vec<_> = data
            .pull_request
            .requested_reviewers
            .iter()
            .map(|u| u.login.as_str())
            .collect();
        assert_eq!(requested, ["me"]);
        // The request itself is not a review, or it would count as you reviewing.
        let ids: Vec<_> = data.reviews.iter().map(|r| r.id).collect();
        assert_eq!(ids, [2]);
    }

    #[test]
    fn a_reviewer_who_already_reviewed_is_not_pending() {
        let data = into_pr_data(
            pull(),
            reviews(json!([
                { "id": 3, "user": { "login": "me" }, "state": "REQUEST_CHANGES" }
            ])),
            Vec::new(),
        );
        assert!(data.pull_request.requested_reviewers.is_empty());
    }
}
