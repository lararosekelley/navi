//! Minimal typed views over the GitHub REST payloads navi consumes.
//!
//! We deserialize only the fields the diff engine needs (via `#[serde(default)]`
//! throughout, so GitHub adding/removing peripheral fields never breaks parsing).
//! Keeping our own structs — rather than leaning on octocrab's models — means the
//! pure [`crate::diff`] layer has no dependency on the HTTP client and is trivially
//! testable from JSON fixtures.

// These structs mirror the GitHub payloads; some fields are kept for documentation
// and forward-compatibility even when the diff engine doesn't read them today.
#![allow(dead_code)]

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub login: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
}

/// One entry from `GET /notifications`.
#[derive(Debug, Clone, Deserialize)]
pub struct Notification {
    /// Thread id (string).
    pub id: String,
    pub reason: String,
    #[serde(default)]
    pub updated_at: Option<String>,
    pub subject: NotificationSubject,
    pub repository: NotificationRepo,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NotificationSubject {
    #[serde(default)]
    pub title: String,
    /// API URL of the subject, e.g. `.../repos/o/r/pulls/12`. Absent for some kinds.
    #[serde(default)]
    pub url: Option<String>,
    /// `"PullRequest"`, `"Issue"`, `"Commit"`, …
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NotificationRepo {
    pub name: String,
    pub owner: User,
    #[serde(default)]
    pub html_url: Option<String>,
}

/// `GET /repos/{o}/{r}/pulls/{n}`.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub html_url: String,
    /// `"open"` | `"closed"`.
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
    pub user: Option<User>,
    #[serde(default)]
    pub merged_by: Option<User>,
    #[serde(default)]
    pub requested_reviewers: Vec<User>,
}

/// `GET /repos/{o}/{r}/pulls/{n}/reviews`.
#[derive(Debug, Clone, Deserialize)]
pub struct Review {
    pub id: u64,
    pub user: Option<User>,
    /// `APPROVED` | `CHANGES_REQUESTED` | `COMMENTED` | `DISMISSED` | `PENDING`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub submitted_at: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
}

/// `GET /repos/{o}/{r}/pulls/{n}/comments` — inline (diff) review comments.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    pub user: Option<User>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub in_reply_to_id: Option<u64>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// `GET /repos/{o}/{r}/issues/{n}/comments` — top-level conversation comments.
#[derive(Debug, Clone, Deserialize)]
pub struct IssueComment {
    pub id: u64,
    pub user: Option<User>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// Everything fetched for one PR in a single poll pass — the input to the diff.
#[derive(Debug, Clone)]
pub struct PrData {
    pub pull_request: PullRequest,
    pub reviews: Vec<Review>,
    pub review_comments: Vec<ReviewComment>,
    pub issue_comments: Vec<IssueComment>,
}
