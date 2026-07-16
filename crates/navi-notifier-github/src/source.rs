//! The GitHub [`Source`]: turns notifications into fetches, fetches into diffs,
//! and diffs into normalized events. All GitHub-specific I/O lives here; the
//! decision logic lives in the pure [`crate::diff`] module.

use async_trait::async_trait;
use navi_notifier_core::model::{Event, Repo};
use navi_notifier_core::traits::{Source, StateStore};
use navi_notifier_core::SourceError;
use octocrab::Octocrab;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tokio::sync::OnceCell;
use tracing::{debug, warn};

use crate::api::{IssueComment, Notification, PrData, PullRequest, Review, ReviewComment, User};
use crate::diff::{diff, DiffContext};
use crate::snapshot::PrSnapshot;

const SOURCE_ID: &str = "github";
/// Safety cap on pagination per endpoint so a pathological PR can't stall a poll.
const MAX_PAGES: u8 = 10;
/// Overlap window when advancing the `since` cursor, to tolerate clock skew.
const SINCE_OVERLAP: Duration = Duration::minutes(5);

/// Configuration for the GitHub source.
pub struct GitHubSourceConfig {
    pub token: String,
    /// API base for GitHub Enterprise Server (e.g. `https://ghe.example.com/api/v3`).
    pub api_base: Option<String>,
}

pub struct GitHubSource {
    octo: Octocrab,
    /// Cached authenticated login, resolved lazily on first poll.
    viewer: OnceCell<String>,
}

impl GitHubSource {
    pub fn new(config: GitHubSourceConfig) -> Result<Self, SourceError> {
        let mut builder = Octocrab::builder().personal_token(config.token);
        if let Some(base) = config.api_base {
            builder = builder
                .base_uri(base)
                .map_err(|e| SourceError::Request(format!("invalid api_base: {e}")))?;
        }
        let octo = builder
            .build()
            .map_err(|e| SourceError::Auth(e.to_string()))?;
        Ok(Self {
            octo,
            viewer: OnceCell::new(),
        })
    }

    /// The authenticated user's login, fetched once and cached.
    async fn viewer_login(&self) -> Result<&str, SourceError> {
        self.viewer
            .get_or_try_init(|| async {
                let me: User = self.octo.get("/user", None::<&()>).await.map_err(map_err)?;
                Ok::<_, SourceError>(me.login)
            })
            .await
            .map(String::as_str)
    }

    /// List notifications updated since `since` (RFC3339), across pages.
    async fn notifications(&self, since: Option<&str>) -> Result<Vec<Notification>, SourceError> {
        #[derive(Serialize)]
        struct Params<'a> {
            all: bool,
            per_page: u8,
            page: u8,
            #[serde(skip_serializing_if = "Option::is_none")]
            since: Option<&'a str>,
        }

        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let params = Params {
                all: true,
                per_page: 100,
                page,
                since,
            };
            let batch: Vec<Notification> = self
                .octo
                .get("/notifications", Some(&params))
                .await
                .map_err(map_err)?;
            let n = batch.len();
            out.extend(batch);
            if n < 100 {
                break;
            }
        }
        Ok(out)
    }

    /// Fetch a page-collected list from a repo sub-resource path.
    async fn get_all<T>(&self, path: &str) -> Result<Vec<T>, SourceError>
    where
        T: serde::de::DeserializeOwned,
    {
        #[derive(Serialize)]
        struct Page {
            per_page: u8,
            page: u8,
        }
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let batch: Vec<T> = self
                .octo
                .get(
                    path,
                    Some(&Page {
                        per_page: 100,
                        page,
                    }),
                )
                .await
                .map_err(map_err)?;
            let n = batch.len();
            out.extend(batch);
            if n < 100 {
                break;
            }
        }
        Ok(out)
    }

    /// Fetch everything the diff needs for one PR.
    async fn fetch_pr(&self, owner: &str, repo: &str, number: u64) -> Result<PrData, SourceError> {
        let pr: PullRequest = self
            .octo
            .get(format!("/repos/{owner}/{repo}/pulls/{number}"), None::<&()>)
            .await
            .map_err(map_err)?;
        let reviews: Vec<Review> = self
            .get_all(&format!("/repos/{owner}/{repo}/pulls/{number}/reviews"))
            .await?;
        let review_comments: Vec<ReviewComment> = self
            .get_all(&format!("/repos/{owner}/{repo}/pulls/{number}/comments"))
            .await?;
        let issue_comments: Vec<IssueComment> = self
            .get_all(&format!("/repos/{owner}/{repo}/issues/{number}/comments"))
            .await?;
        Ok(PrData {
            pull_request: pr,
            reviews,
            review_comments,
            issue_comments,
        })
    }
}

#[async_trait]
impl Source for GitHubSource {
    fn id(&self) -> &str {
        SOURCE_ID
    }

    async fn poll(&self, state: &dyn StateStore) -> Result<Vec<Event>, SourceError> {
        let viewer = self.viewer_login().await?.to_string();
        let poll_start = OffsetDateTime::now_utc();
        let since = state.get_cursor(SOURCE_ID, "notif_since").await?;

        let notifs = self.notifications(since.as_deref()).await?;
        debug!(count = notifs.len(), "fetched notifications");

        let mut events = Vec::new();
        for n in &notifs {
            if n.subject.kind != "PullRequest" {
                continue;
            }
            let Some((owner, repo, number)) = n.subject.url.as_deref().and_then(parse_pr_url)
            else {
                warn!(url = ?n.subject.url, "could not parse PR url from notification");
                continue;
            };
            let scope = format!("{owner}/{repo}#{number}");

            // Skip threads whose activity we've already processed (pure optimisation —
            // the snapshot would suppress duplicates anyway).
            let seen_key = format!("thread:{scope}");
            let last_seen = state.get_cursor(SOURCE_ID, &seen_key).await?;
            if let (Some(seen), Some(updated)) = (&last_seen, &n.updated_at) {
                if updated.as_str() <= seen.as_str() {
                    continue;
                }
            }

            let pr_data = match self.fetch_pr(&owner, &repo, number).await {
                Ok(d) => d,
                Err(e) => {
                    // One inaccessible PR (deleted, perms) shouldn't abort the whole poll.
                    warn!(%scope, error = %e, "failed to fetch PR; skipping");
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
                    owner: owner.clone(),
                    name: repo.clone(),
                    url: n.repository.html_url.clone(),
                },
                now: poll_start,
            };
            let (evs, new_snapshot) = diff(&ctx, &pr_data, &old);

            let bytes = serde_json::to_vec(&new_snapshot)
                .map_err(|e| SourceError::Parse(format!("serialize snapshot {scope}: {e}")))?;
            state.put_snapshot(SOURCE_ID, &scope, &bytes).await?;
            if let Some(updated) = &n.updated_at {
                state.put_cursor(SOURCE_ID, &seen_key, updated).await?;
            }

            events.extend(evs);
        }

        // Advance the list cursor with a small overlap so nothing straddling the
        // boundary is missed on the next poll.
        let next_since = (poll_start - SINCE_OVERLAP)
            .format(&Rfc3339)
            .map_err(|e| SourceError::Other(Box::new(e)))?;
        state
            .put_cursor(SOURCE_ID, "notif_since", &next_since)
            .await?;

        Ok(events)
    }
}

/// Parse `https://api.github.com/repos/{owner}/{repo}/pulls/{number}` into parts.
fn parse_pr_url(url: &str) -> Option<(String, String, u64)> {
    let after = url.split("/repos/").nth(1)?;
    let mut parts = after.split('/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    let kind = parts.next()?; // "pulls"
    if kind != "pulls" {
        return None;
    }
    let number: u64 = parts.next()?.parse().ok()?;
    Some((owner, repo, number))
}

/// Map an octocrab error into a [`SourceError`], recognising rate limiting.
fn map_err(err: octocrab::Error) -> SourceError {
    let msg = err.to_string();
    if msg.contains("rate limit") || msg.contains("403") {
        return SourceError::RateLimited {
            retry_after_secs: 60,
        };
    }
    if msg.contains("401") || msg.contains("Bad credentials") {
        return SourceError::Auth(msg);
    }
    SourceError::Request(msg)
}

#[cfg(test)]
mod tests {
    use super::parse_pr_url;

    #[test]
    fn parses_pr_url() {
        assert_eq!(
            parse_pr_url("https://api.github.com/repos/acme/widgets/pulls/12"),
            Some(("acme".into(), "widgets".into(), 12))
        );
    }

    #[test]
    fn rejects_non_pull_urls() {
        assert_eq!(
            parse_pr_url("https://api.github.com/repos/acme/widgets/issues/12"),
            None
        );
    }
}
