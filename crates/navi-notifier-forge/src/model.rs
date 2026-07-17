//! Shared input model for forge-style sources (GitHub, Gitea, ...).
//!
//! These are the structs the pure [`crate::diff`] engine consumes. Each source
//! deserializes or maps its provider's payloads into these, so one diff engine
//! serves every GitHub-shaped forge. Fields use GitHub's names and values (e.g.
//! review state `CHANGES_REQUESTED`); a non-GitHub source normalizes to them.

// Some fields are kept for forward-compatibility even when the diff engine doesn't
// read them today.
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

/// A pull/merge request.
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

/// A submitted review.
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

/// An inline (diff) review comment.
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

/// A top-level conversation comment.
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

/// Everything fetched for one PR in a single poll pass; the input to the diff.
#[derive(Debug, Clone)]
pub struct PrData {
    pub pull_request: PullRequest,
    pub reviews: Vec<Review>,
    pub review_comments: Vec<ReviewComment>,
    pub issue_comments: Vec<IssueComment>,
}
