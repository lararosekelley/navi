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
