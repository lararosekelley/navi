//! GitHub notification payloads (`GET /notifications`), used only to learn which
//! PRs have activity. The PR data the diff consumes lives in `navi-notifier-forge`.
#![allow(dead_code)]

use navi_notifier_forge::model::User;
use serde::Deserialize;

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
