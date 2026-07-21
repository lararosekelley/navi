//! Typed views over the GitLab REST payloads navi reads (todos + user).
// Some fields mirror the payload for documentation and forward-compatibility.
#![allow(dead_code)]

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub username: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SimpleUser {
    pub username: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    pub path_with_namespace: String,
    #[serde(default)]
    pub web_url: Option<String>,
}

/// The merge request a todo points at (embedded in the todo payload).
#[derive(Debug, Clone, Deserialize)]
pub struct Target {
    pub iid: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    /// Older GitLab field for draft status.
    #[serde(default)]
    pub work_in_progress: bool,
    #[serde(default)]
    pub author: Option<SimpleUser>,
}

impl Target {
    pub fn is_draft(&self) -> bool {
        self.draft || self.work_in_progress
    }
}

/// A merge request from `GET /merge_requests` (list) or the single-MR endpoint.
/// Enough of the payload to derive lifecycle (merged/closed/ready) events.
#[derive(Debug, Clone, Deserialize)]
pub struct MergeRequest {
    pub iid: u64,
    pub project_id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub web_url: Option<String>,
    /// `opened`, `closed`, `locked`, or `merged`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub work_in_progress: bool,
    #[serde(default)]
    pub author: Option<SimpleUser>,
    #[serde(default)]
    pub merged_by: Option<SimpleUser>,
    #[serde(default)]
    pub merged_at: Option<String>,
    #[serde(default)]
    pub closed_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Users requested to review this MR, used to set the viewer's reviewer relation.
    #[serde(default)]
    pub reviewers: Vec<SimpleUser>,
}

impl MergeRequest {
    pub fn is_draft(&self) -> bool {
        self.draft || self.work_in_progress
    }
    /// Whether `viewer` is among the requested reviewers (case-insensitive).
    pub fn has_reviewer(&self, viewer: &str) -> bool {
        self.reviewers
            .iter()
            .any(|u| u.username.eq_ignore_ascii_case(viewer))
    }
    pub fn is_merged(&self) -> bool {
        self.state == "merged"
    }
    pub fn is_closed(&self) -> bool {
        self.state == "closed"
    }
}

/// One thread from `GET /merge_requests/:iid/discussions`. Its `notes` share a
/// discussion id; the first non-system note is the thread's root.
#[derive(Debug, Clone, Deserialize)]
pub struct Discussion {
    pub id: String,
    #[serde(default)]
    pub notes: Vec<Note>,
}

/// A single note within a discussion.
#[derive(Debug, Clone, Deserialize)]
pub struct Note {
    pub id: u64,
    /// System notes are state-change breadcrumbs ("changed the description"), not
    /// human comments; we skip them.
    #[serde(default)]
    pub system: bool,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub author: Option<SimpleUser>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// One entry from `GET /todos`.
#[derive(Debug, Clone, Deserialize)]
pub struct Todo {
    pub id: u64,
    /// `review_requested`, `approval_required`, `assigned`, `mentioned`,
    /// `directly_addressed`, `build_failed`, `marked`, `unmergeable`, ...
    pub action_name: String,
    /// `MergeRequest`, `Issue`, ...
    pub target_type: String,
    #[serde(default)]
    pub target_url: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub author: Option<SimpleUser>,
    pub project: Project,
    #[serde(default)]
    pub target: Option<Target>,
}
