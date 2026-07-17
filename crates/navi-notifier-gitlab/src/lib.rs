//! GitLab source for navi.
//!
//! GitLab's Todos API (`GET /todos`) is already a per-user action feed with a
//! specific `action_name`, so this maps pending todos straight to normalized
//! events, no per-MR timeline diff needed. It covers review requests, approval
//! requests, and mentions on merge requests. Merge/close and reply-to-your-comment
//! events need MR-note diffing and are a follow-up (see SCRATCHPAD).

mod api;

use async_trait::async_trait;
use navi_notifier_core::model::{Actor, Event, EventKind, PullRequest, Repo, ViewerRelationship};
use navi_notifier_core::traits::{Source, StateStore};
use navi_notifier_core::SourceError;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::OnceCell;
use tracing::debug;

use api::{Todo, User};

const SOURCE_ID: &str = "gitlab";
const DEFAULT_API_BASE: &str = "https://gitlab.com/api/v4";
const MAX_PAGES: u8 = 10;

pub struct GitLabSourceConfig {
    pub token: String,
    /// API base, e.g. `https://gitlab.example.com/api/v4` for self-hosted.
    pub api_base: Option<String>,
}

pub struct GitLabSource {
    client: reqwest::Client,
    token: String,
    api_base: String,
    viewer: OnceCell<String>,
}

impl GitLabSource {
    pub fn new(config: GitLabSourceConfig) -> Result<Self, SourceError> {
        if config.token.trim().is_empty() {
            return Err(SourceError::Auth(
                "GitLab token is empty; set NAVI_GITLAB_TOKEN".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| SourceError::Request(format!("building HTTP client: {e}")))?;
        Ok(Self {
            client,
            token: config.token,
            api_base: config
                .api_base
                .unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
            viewer: OnceCell::new(),
        })
    }

    async fn viewer_login(&self) -> Result<&str, SourceError> {
        self.viewer
            .get_or_try_init(|| async {
                let me: User = self.get("/user", 1).await?;
                Ok::<_, SourceError>(me.username)
            })
            .await
            .map(String::as_str)
    }

    async fn get<T: for<'de> serde::Deserialize<'de>>(
        &self,
        path: &str,
        page: u8,
    ) -> Result<T, SourceError> {
        #[derive(Serialize)]
        struct Params {
            per_page: u8,
            page: u8,
        }
        let resp = self
            .client
            .get(format!("{}{path}", self.api_base))
            .header("PRIVATE-TOKEN", &self.token)
            .query(&Params {
                per_page: 100,
                page,
            })
            .send()
            .await
            .map_err(|e| SourceError::Request(e.to_string()))?;
        map_status(&resp)?;
        resp.json()
            .await
            .map_err(|e| SourceError::Parse(e.to_string()))
    }

    /// Fetch pending todos across pages.
    async fn todos(&self) -> Result<Vec<Todo>, SourceError> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let batch: Vec<Todo> = self.get("/todos?state=pending", page).await?;
            let n = batch.len();
            out.extend(batch);
            if n < 100 {
                break;
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl Source for GitLabSource {
    fn id(&self) -> &str {
        SOURCE_ID
    }

    async fn poll(&self, _state: &dyn StateStore) -> Result<Vec<Event>, SourceError> {
        let viewer = self.viewer_login().await?.to_string();
        let now = OffsetDateTime::now_utc();
        let todos = self.todos().await?;
        debug!(count = todos.len(), "fetched gitlab todos");

        let events = todos
            .iter()
            .filter_map(|todo| todo_to_event(todo, &viewer, now))
            .collect();
        Ok(events)
    }
}

/// Map a pending todo to a normalized event, or `None` if it isn't one we surface.
/// Pure and unit-tested; `now` is the fallback timestamp.
fn todo_to_event(todo: &Todo, viewer: &str, now: OffsetDateTime) -> Option<Event> {
    if todo.target_type != "MergeRequest" {
        return None;
    }
    let target = todo.target.as_ref()?;

    let kind = match todo.action_name.as_str() {
        "review_requested" | "approval_required" | "assigned" => EventKind::ReviewRequested,
        "mentioned" | "directly_addressed" => EventKind::Mentioned,
        _ => return None,
    };

    let (owner, name) = match todo.project.path_with_namespace.rsplit_once('/') {
        Some((o, n)) => (o.to_string(), n.to_string()),
        None => (String::new(), todo.project.path_with_namespace.clone()),
    };
    let repo = Repo {
        owner,
        name,
        url: todo.project.web_url.clone(),
    };

    let author = target
        .author
        .as_ref()
        .map(|u| Actor::new(u.username.as_str()))
        .unwrap_or_else(|| Actor::new("unknown"));
    let is_author = author.login.eq_ignore_ascii_case(viewer);

    let occurred = todo
        .created_at
        .as_deref()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .unwrap_or(now);

    let url = todo
        .target_url
        .clone()
        .or_else(|| target.web_url.clone())
        .unwrap_or_default();

    Some(Event {
        source_id: SOURCE_ID.to_string(),
        kind,
        pull_request: PullRequest {
            repo: repo.clone(),
            number: target.iid,
            title: target.title.clone(),
            url,
            author,
            draft: target.is_draft(),
        },
        viewer: ViewerRelationship {
            is_author,
            is_reviewer: true,
        },
        actor: todo
            .author
            .as_ref()
            .map(|u| Actor::new(u.username.as_str()))
            .unwrap_or_else(|| Actor::new("unknown")),
        occurred_at: occurred,
        target_url: Some(target.web_url.clone().unwrap_or_default()),
        excerpt: todo.body.clone().filter(|b| !b.is_empty()),
        dedup_key: Event::make_dedup_key(
            SOURCE_ID,
            &repo,
            target.iid,
            &format!("todo:{}", todo.id),
        ),
    })
}

fn map_status(resp: &reqwest::Response) -> Result<(), SourceError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    match status.as_u16() {
        401 => Err(SourceError::Auth("invalid GitLab token".into())),
        403 => Err(SourceError::Auth(
            "GitLab returned 403; token likely lacks read_api scope".into(),
        )),
        429 => Err(SourceError::RateLimited {
            retry_after_secs: 60,
        }),
        _ => Err(SourceError::Request(format!("gitlab returned {status}"))),
    }
}

#[cfg(test)]
mod tests {
    include!("todo_tests.rs");
}
