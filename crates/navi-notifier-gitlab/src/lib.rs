//! GitLab source for navi.
//!
//! Two paths feed the normalized event stream. The Todos API (`GET /todos`) is a
//! per-user action feed keyed by `action_name`, so pending todos map straight to
//! review-request and mention events with no diffing. Everything the todo feed
//! can't express - a merge, a close, a draft going ready, a reply in a thread you
//! took part in - comes from diffing each involved MR and its discussion notes
//! against a stored snapshot (see [`mr_diff`]), the same shape the GitHub source
//! uses. The two paths cover disjoint event kinds, so nothing double-fires.

mod api;
mod mr_diff;

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use navi_notifier_core::model::{Actor, Event, EventKind, PullRequest, Repo, ViewerRelationship};
use navi_notifier_core::traits::{Source, StateStore};
use navi_notifier_core::SourceError;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tokio::sync::OnceCell;
use tracing::{debug, warn};

use api::{Discussion, MergeRequest, Todo, User};
use mr_diff::{diff_mr, MrContext, MrSnapshot};

const SOURCE_ID: &str = "gitlab";
const DEFAULT_API_BASE: &str = "https://gitlab.com/api/v4";
const MAX_PAGES: u8 = 10;
/// Overlap when advancing the MR cursor, to tolerate clock skew between poll passes.
const SINCE_OVERLAP: Duration = Duration::minutes(5);

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
    /// scope (`owner/name#iid`) -> serialized MR snapshot, deferred during a poll
    /// and flushed by `commit_snapshots` only for MRs whose delivery didn't fail.
    pending_snapshots: Mutex<HashMap<String, Vec<u8>>>,
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
            pending_snapshots: Mutex::new(HashMap::new()),
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

    /// Merge requests the viewer is involved in (author, reviewer, or assignee),
    /// across all states, updated since `since`, deduped by project + iid. This is
    /// the discovery step for the note-diff path: unlike todos it also surfaces MRs
    /// you authored, so merges and closes on your own MRs reach you.
    async fn involved_mrs(
        &self,
        viewer: &str,
        since: Option<&str>,
    ) -> Result<Vec<MergeRequest>, SourceError> {
        let updated = since
            .map(|s| format!("&updated_after={s}"))
            .unwrap_or_default();
        let mut seen: HashSet<(u64, u64)> = HashSet::new();
        let mut out = Vec::new();
        for role in ["author_username", "reviewer_username", "assignee_username"] {
            for page in 1..=MAX_PAGES {
                let path = format!(
                    "/merge_requests?scope=all&state=all&order_by=updated_at&{role}={viewer}{updated}"
                );
                let batch: Vec<MergeRequest> = self.get(&path, page).await?;
                let n = batch.len();
                for mr in batch {
                    if seen.insert((mr.project_id, mr.iid)) {
                        out.push(mr);
                    }
                }
                if n < 100 {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// All discussion threads on one MR, across pages.
    async fn discussions(&self, project_id: u64, iid: u64) -> Result<Vec<Discussion>, SourceError> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let path = format!("/projects/{project_id}/merge_requests/{iid}/discussions");
            let batch: Vec<Discussion> = self.get(&path, page).await?;
            let n = batch.len();
            out.extend(batch);
            if n < 100 {
                break;
            }
        }
        Ok(out)
    }
}

/// Derive an `owner/name` repo from an MR's `web_url`
/// (`https://host/owner/name/-/merge_requests/7`). Nested groups collapse into the
/// owner (`group/subgroup`), keeping the project as the name.
fn repo_from_mr(mr: &MergeRequest) -> Repo {
    let base = mr
        .web_url
        .as_deref()
        .and_then(|u| u.split("/-/merge_requests").next());
    let path = base
        .and_then(|b| b.splitn(4, '/').nth(3))
        .unwrap_or_default();
    match path.rsplit_once('/') {
        Some((owner, name)) => Repo {
            owner: owner.to_string(),
            name: name.to_string(),
            url: base.map(str::to_string),
        },
        None => Repo {
            owner: String::new(),
            name: path.to_string(),
            url: base.map(str::to_string),
        },
    }
}

#[async_trait]
impl Source for GitLabSource {
    fn id(&self) -> &str {
        SOURCE_ID
    }

    async fn poll(&self, state: &dyn StateStore) -> Result<Vec<Event>, SourceError> {
        let viewer = self.viewer_login().await?.to_string();
        let now = OffsetDateTime::now_utc();
        // Fresh stash each pass; the prior pass's snapshots were flushed by
        // `commit_snapshots` (or dropped on a dry run).
        self.pending_snapshots.lock().unwrap().clear();

        // Todos path: review requests and mentions.
        let todos = self.todos().await?;
        debug!(count = todos.len(), "fetched gitlab todos");
        let mut events: Vec<Event> = todos
            .iter()
            .filter_map(|todo| todo_to_event(todo, &viewer, now))
            .collect();

        // Note-diff path: merges, closes, ready, and replies on involved MRs.
        let since = state.get_cursor(SOURCE_ID, "mr_since").await?;
        match self.involved_mrs(&viewer, since.as_deref()).await {
            Ok(mrs) => {
                debug!(count = mrs.len(), "fetched involved gitlab MRs");
                for mr in mrs {
                    let repo = repo_from_mr(&mr);
                    let scope = format!("{}#{}", repo.full_name(), mr.iid);
                    let old: MrSnapshot = match state.get_snapshot(SOURCE_ID, &scope).await? {
                        Some(bytes) => serde_json::from_slice(&bytes)
                            .map_err(|e| SourceError::Parse(format!("snapshot {scope}: {e}")))?,
                        None => MrSnapshot::default(),
                    };
                    let discussions = match self.discussions(mr.project_id, mr.iid).await {
                        Ok(d) => d,
                        Err(e) => {
                            warn!(%scope, error = %e, "failed to fetch MR discussions; skipping");
                            continue;
                        }
                    };
                    let ctx = MrContext {
                        viewer: viewer.clone(),
                        repo,
                        now,
                    };
                    let (evs, snapshot) = diff_mr(&ctx, &mr, &discussions, &old);
                    let bytes = serde_json::to_vec(&snapshot)
                        .map_err(|e| SourceError::Parse(format!("serialize {scope}: {e}")))?;
                    // Defer persistence to `commit_snapshots` (after delivery), so a
                    // failed send re-derives this MR's events next poll.
                    self.pending_snapshots.lock().unwrap().insert(scope, bytes);
                    events.extend(evs);
                }
            }
            Err(e) => {
                warn!(error = %e, "could not list your involved MRs; skipping that pass");
            }
        }

        let next_since = (now - SINCE_OVERLAP)
            .format(&Rfc3339)
            .map_err(|e| SourceError::Other(Box::new(e)))?;
        state.put_cursor(SOURCE_ID, "mr_since", &next_since).await?;

        Ok(events)
    }

    /// Persist the MR snapshots deferred during `poll`, skipping any whose delivery
    /// failed this pass so its events re-derive next time.
    async fn commit_snapshots(
        &self,
        state: &dyn StateStore,
        failed_scopes: &HashSet<String>,
    ) -> Result<(), SourceError> {
        let pending: Vec<(String, Vec<u8>)> =
            self.pending_snapshots.lock().unwrap().drain().collect();
        let mut first_err = None;
        for (scope, bytes) in pending {
            if failed_scopes.contains(&scope) {
                continue;
            }
            if let Err(e) = state
                .put_snapshot(SOURCE_ID, &scope, &bytes)
                .await
                .map_err(SourceError::from)
            {
                warn!(%scope, error = %e, "failed to persist snapshot; it will re-derive next poll");
                first_err.get_or_insert(e);
            }
        }
        first_err.map_or(Ok(()), Err)
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

    let actor = todo
        .author
        .as_ref()
        .map(|u| Actor::new(u.username.as_str()))
        .unwrap_or_else(|| Actor::new("unknown"));
    let actor_is_viewer = actor.login.eq_ignore_ascii_case(viewer);

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
            actor_is_viewer,
        },
        actor,
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
