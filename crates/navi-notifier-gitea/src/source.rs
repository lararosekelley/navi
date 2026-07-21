//! The Gitea/Forgejo [`Source`]. Gitea's REST API is GitHub-shaped, so this fetches
//! the same PR/reviews/comments and reuses the shared `navi-notifier-forge` diff
//! engine; only the payload mapping (in `api`) and notification URL shape differ.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use navi_notifier_core::model::{Event, Repo};
use navi_notifier_core::traits::{Source, StateStore};
use navi_notifier_core::SourceError;
use navi_notifier_forge::model::PrData;
use navi_notifier_forge::{diff, first_sight_watermark, DiffContext, PrSnapshot};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tokio::sync::OnceCell;
use tracing::{debug, warn};

use crate::api::{GiteaIssueComment, GiteaPull, GiteaReview, GiteaUser, Notification};

const SOURCE_ID: &str = "gitea";
const DEFAULT_API_BASE: &str = "https://gitea.com/api/v1";
const MAX_PAGES: u8 = 10;
const SINCE_OVERLAP: Duration = Duration::minutes(5);

pub struct GiteaSourceConfig {
    pub token: String,
    /// API base, e.g. `https://gitea.example.com/api/v1` (or a Forgejo instance).
    pub api_base: Option<String>,
}

pub struct GiteaSource {
    client: reqwest::Client,
    token: String,
    api_base: String,
    viewer: OnceCell<String>,
    /// scope (`owner/repo#n`) -> serialized new snapshot, deferred during a poll and
    /// flushed by `commit_snapshots` only for PRs whose delivery didn't fail.
    pending_snapshots: Mutex<HashMap<String, Vec<u8>>>,
}

impl GiteaSource {
    pub fn new(config: GiteaSourceConfig) -> Result<Self, SourceError> {
        if config.token.trim().is_empty() {
            return Err(SourceError::Auth(
                "Gitea token is empty; set NAVI_GITEA_TOKEN".into(),
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
                let me: GiteaUser = self.get("/user", &[]).await?;
                Ok::<_, SourceError>(me.login)
            })
            .await
            .map(String::as_str)
    }

    async fn get<T: for<'de> serde::Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, SourceError> {
        let resp = self
            .client
            .get(format!("{}{path}", self.api_base))
            .header("Authorization", format!("token {}", self.token))
            .query(query)
            .send()
            .await
            .map_err(|e| SourceError::Request(e.to_string()))?;
        map_status(&resp)?;
        resp.json()
            .await
            .map_err(|e| SourceError::Parse(e.to_string()))
    }

    async fn get_all<T: for<'de> serde::Deserialize<'de>>(
        &self,
        path: &str,
    ) -> Result<Vec<T>, SourceError> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let batch: Vec<T> = self
                .get(path, &[("page", page.to_string()), ("limit", "50".into())])
                .await?;
            let n = batch.len();
            out.extend(batch);
            if n < 50 {
                break;
            }
        }
        Ok(out)
    }

    async fn notifications(&self, since: Option<&str>) -> Result<Vec<Notification>, SourceError> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut query = vec![
                ("all", "true".to_string()),
                ("page", page.to_string()),
                ("limit", "50".to_string()),
            ];
            if let Some(s) = since {
                query.push(("since", s.to_string()));
            }
            let batch: Vec<Notification> = self.get("/notifications", &query).await?;
            let n = batch.len();
            out.extend(batch);
            if n < 50 {
                break;
            }
        }
        Ok(out)
    }

    async fn fetch_pr(&self, owner: &str, repo: &str, index: u64) -> Result<PrData, SourceError> {
        let pull: GiteaPull = self
            .get(&format!("/repos/{owner}/{repo}/pulls/{index}"), &[])
            .await?;
        let reviews: Vec<GiteaReview> = self
            .get_all(&format!("/repos/{owner}/{repo}/pulls/{index}/reviews"))
            .await?;
        let issue_comments: Vec<GiteaIssueComment> = self
            .get_all(&format!("/repos/{owner}/{repo}/issues/{index}/comments"))
            .await?;
        Ok(PrData {
            pull_request: pull.into_forge(),
            reviews: reviews.into_iter().map(GiteaReview::into_forge).collect(),
            // Gitea inline review comments are per-review and lack reply threading;
            // conversation comments cover mentions and replies for now.
            review_comments: Vec::new(),
            issue_comments: issue_comments
                .into_iter()
                .map(GiteaIssueComment::into_forge)
                .collect(),
        })
    }
}

#[async_trait]
impl Source for GiteaSource {
    fn id(&self) -> &str {
        SOURCE_ID
    }

    async fn poll(&self, state: &dyn StateStore) -> Result<Vec<Event>, SourceError> {
        let viewer = self.viewer_login().await?.to_string();
        let poll_start = OffsetDateTime::now_utc();
        // Fresh stash each pass; deferred snapshots persist via `commit_snapshots`.
        self.pending_snapshots.lock().unwrap().clear();
        let since = state.get_cursor(SOURCE_ID, "notif_since").await?;
        let notifs = self.notifications(since.as_deref()).await?;
        debug!(count = notifs.len(), "fetched gitea notifications");

        let mut events = Vec::new();
        for n in &notifs {
            if n.subject.kind != "Pull" {
                continue;
            }
            let Some((owner, repo)) = n.repository.full_name.split_once('/') else {
                continue;
            };
            let Some(index) = n.subject.url.as_deref().and_then(parse_index) else {
                warn!(url = ?n.subject.url, "could not parse index from gitea notification");
                continue;
            };
            let scope = format!("{owner}/{repo}#{index}");

            let pr_data = match self.fetch_pr(owner, repo, index).await {
                Ok(d) => d,
                Err(e) => {
                    warn!(%scope, error = %e, "failed to fetch gitea PR; skipping");
                    continue;
                }
            };

            let old: PrSnapshot = match state.get_snapshot(SOURCE_ID, &scope).await? {
                Some(bytes) => serde_json::from_slice(&bytes)
                    .map_err(|e| SourceError::Parse(format!("snapshot {scope}: {e}")))?,
                None => PrSnapshot::default(),
            };

            let ctx = DiffContext {
                source_id: SOURCE_ID.to_string(),
                viewer_login: viewer.clone(),
                repo: Repo {
                    owner: owner.to_string(),
                    name: repo.to_string(),
                    url: n.repository.html_url.clone(),
                },
                now: poll_start,
                first_sight_since: first_sight_watermark(n.updated_at.as_deref()),
                // Gitea team review requests aren't modelled yet.
                viewer_teams: std::collections::HashSet::new(),
            };
            let (evs, new_snapshot) = diff(&ctx, &pr_data, &old);

            let bytes = serde_json::to_vec(&new_snapshot)
                .map_err(|e| SourceError::Parse(format!("serialize snapshot {scope}: {e}")))?;
            // Defer persistence to `commit_snapshots` (after delivery).
            self.pending_snapshots.lock().unwrap().insert(scope, bytes);
            events.extend(evs);
        }

        let next_since = (poll_start - SINCE_OVERLAP)
            .format(&Rfc3339)
            .map_err(|e| SourceError::Other(Box::new(e)))?;
        state
            .put_cursor(SOURCE_ID, "notif_since", &next_since)
            .await?;

        Ok(events)
    }

    /// Persist the snapshots deferred during `poll`, skipping any PR whose delivery
    /// failed this pass so its events re-derive next time.
    async fn commit_snapshots(
        &self,
        state: &dyn StateStore,
        failed_scopes: &HashSet<String>,
    ) -> Result<(), SourceError> {
        let pending: Vec<(String, Vec<u8>)> =
            self.pending_snapshots.lock().unwrap().drain().collect();
        // Attempt every entry: one write failure must not drop the others (already
        // drained). A scope we fail to persist just re-derives next poll.
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

/// Trailing number of a Gitea subject URL (`.../issues/12` -> `12`).
fn parse_index(url: &str) -> Option<u64> {
    url.rsplit('/').next()?.parse().ok()
}

fn map_status(resp: &reqwest::Response) -> Result<(), SourceError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    match status.as_u16() {
        401 => Err(SourceError::Auth("invalid Gitea token".into())),
        403 => Err(SourceError::Auth(
            "Gitea returned 403; the token may lack the needed scopes".into(),
        )),
        429 => Err(SourceError::RateLimited {
            retry_after_secs: 60,
        }),
        _ => Err(SourceError::Request(format!("gitea returned {status}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_index;

    #[test]
    fn parses_index_from_subject_url() {
        assert_eq!(
            parse_index("https://gitea.test/api/v1/repos/acme/widgets/issues/12"),
            Some(12)
        );
    }
}
