//! Gitea/Forgejo REST payloads, mapped into the shared `navi-notifier-forge` model.
#![allow(dead_code)]

use navi_notifier_forge::model::{IssueComment, PullRequest, Review, User};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct GiteaUser {
    /// Gitea uses `username`; `login` is the alias other forges use.
    #[serde(alias = "username")]
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
    #[serde(default)]
    pub requested_reviewers: Vec<GiteaUser>,
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
            requested_reviewers: self
                .requested_reviewers
                .into_iter()
                .map(GiteaUser::into_forge)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GiteaReview {
    pub id: u64,
    pub user: Option<GiteaUser>,
    /// `APPROVED` | `REQUEST_CHANGES` | `COMMENT` | `PENDING`.
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
