//! The Gitea/Forgejo [`Source`]. Gitea's REST API is GitHub-shaped, so this fetches
//! the same PR/reviews/comments and reuses the shared `navi-notifier-forge` diff
//! engine; only the payload mapping (in `api`) and notification URL shape differ.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use navi_notifier_core::model::{Event, Repo};
use navi_notifier_core::traits::{Source, StateStore};
use navi_notifier_core::{Backfill, SourceError};
use navi_notifier_forge::model::PrData;
use navi_notifier_forge::{
    diff, first_sight_watermark, DiffContext, FetchBackoff, PrOutcome, PrSnapshot,
    FIRST_SIGHT_LEEWAY,
};
use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tokio::sync::OnceCell;
use tracing::{debug, info, warn};

use crate::api::{GiteaIssueComment, GiteaPull, GiteaReview, GiteaUser, Notification};

const SOURCE_ID: &str = "gitea";
const DEFAULT_API_BASE: &str = "https://gitea.com/api/v1";
const MAX_PAGES: u8 = 10;
const SINCE_OVERLAP: Duration = Duration::minutes(5);

pub struct GiteaSourceConfig {
    pub token: String,
    /// API base, e.g. `https://gitea.example.com/api/v1` (or a Forgejo instance).
    pub api_base: Option<String>,
    /// Hold a comment back until it is at least this many seconds old (0 = off).
    pub comment_min_age_secs: u64,
    /// Poll your involved PRs directly (search), on top of notifications.
    pub track_prs: bool,
    /// How much pre-existing activity to surface on the very first poll.
    pub backfill: Backfill,
}

pub struct GiteaSource {
    client: reqwest::Client,
    token: String,
    api_base: String,
    viewer: OnceCell<String>,
    /// scope (`owner/repo#n`) -> serialized new snapshot, deferred during a poll and
    /// flushed by `commit_snapshots` only for PRs whose delivery didn't fail.
    pending_snapshots: Mutex<HashMap<String, Vec<u8>>>,
    /// Min comment age before notifying (`None` = off), passed through to the diff.
    comment_min_age: Option<Duration>,
    /// Whether to also sweep your involved PRs directly (catches self-merges/closes).
    track_prs: bool,
    /// First-run backfill mode, applied to the involved-PR sweep on the first poll.
    backfill: Backfill,
    /// scope (`owner/repo#n`) -> involved-sweep `pr:` cursor value, deferred until
    /// `commit_snapshots` so a failed delivery re-derives the PR instead of skipping.
    pending_pr_cursors: Mutex<HashMap<String, String>>,
    /// Scopes whose fetch keeps failing, skipped for a growing interval so a PR that
    /// can never be fetched doesn't cost a re-fetch on every poll for ever.
    backoff: FetchBackoff,
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
            comment_min_age: (config.comment_min_age_secs > 0)
                .then(|| Duration::seconds(config.comment_min_age_secs as i64)),
            track_prs: config.track_prs,
            backfill: config.backfill,
            pending_pr_cursors: Mutex::new(HashMap::new()),
            backoff: FetchBackoff::default(),
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
            if page == MAX_PAGES {
                warn!(
                    fetched = out.len(),
                    cap = MAX_PAGES,
                    "gitea list truncated at the page cap; newer items may be missed this poll"
                );
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
            if page == MAX_PAGES {
                warn!(
                    fetched = out.len(),
                    cap = MAX_PAGES,
                    "gitea list truncated at the page cap; newer items may be missed this poll"
                );
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

    /// Fetch, diff, and stash one PR against its stored snapshot; returns the events.
    /// Shared by the notification and involved-PR paths so both dedupe through the
    /// same snapshot key.
    #[allow(clippy::too_many_arguments)]
    async fn process_pr(
        &self,
        state: &dyn StateStore,
        owner: &str,
        repo: &str,
        index: u64,
        repo_url: Option<String>,
        first_sight_since: Option<OffsetDateTime>,
        first_sight_backfill: Option<Backfill>,
        viewer: &str,
        now: OffsetDateTime,
    ) -> Result<PrOutcome, SourceError> {
        let scope = format!("{owner}/{repo}#{index}");
        let pr_data = match self.fetch_pr(owner, repo, index).await {
            Ok(d) => {
                self.backoff.clear(&scope);
                d
            }
            // One unfetchable PR shouldn't abort the poll, but the caller has to know
            // whether it may record this PR as seen: past a permanently gone one,
            // yes; past one that merely blipped, no, or the PR is skipped until its
            // timestamp moves and the failure outlives the poll it happened on.
            Err(e @ SourceError::Gone(_)) => {
                // Permanently gone: the cursor advances, so there is nothing left to
                // back off from.
                self.backoff.clear(&scope);
                warn!(%scope, error = %e, "gitea PR is gone or not visible; skipping it for good");
                return Ok(PrOutcome::Gone);
            }
            Err(e) => {
                let wait = self.backoff.failed(&scope, now);
                warn!(%scope, error = %e, retry_in_secs = wait.whole_seconds(), "failed to fetch gitea PR; backing off");
                return Ok(PrOutcome::Unfetched);
            }
        };
        let old: PrSnapshot = match state.get_snapshot(SOURCE_ID, &scope).await? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| SourceError::Parse(format!("snapshot {scope}: {e}")))?,
            None => PrSnapshot::default(),
        };
        let ctx = DiffContext {
            source_id: SOURCE_ID.to_string(),
            viewer_login: viewer.to_string(),
            repo: Repo {
                owner: owner.to_string(),
                name: repo.to_string(),
                url: repo_url,
            },
            now,
            first_sight_since,
            viewer_teams: std::collections::HashSet::new(),
            comment_min_age: self.comment_min_age,
            first_sight_backfill,
        };
        let (evs, new_snapshot) = diff(&ctx, &pr_data, &old);
        let bytes = serde_json::to_vec(&new_snapshot)
            .map_err(|e| SourceError::Parse(format!("serialize snapshot {scope}: {e}")))?;
        self.pending_snapshots.lock().unwrap().insert(scope, bytes);
        Ok(PrOutcome::Diffed(evs))
    }

    /// Open PRs the viewer is involved in (author, assignee, mentioned, or a
    /// requested reviewer), via `/repos/issues/search`.
    async fn involved_open_prs(
        &self,
    ) -> Result<Vec<(String, String, u64, String, Option<String>)>, SourceError> {
        self.search_prs("open", None).await
    }

    /// Involved PRs closed/merged since `since` (RFC3339). Catches self-merges and
    /// self-closes, which don't notify you and have left the open sweep.
    async fn recently_closed_prs(
        &self,
        since: &str,
    ) -> Result<Vec<(String, String, u64, String, Option<String>)>, SourceError> {
        self.search_prs("closed", Some(since)).await
    }

    /// Search involved PRs by state, returning `(owner, repo, index, updated_at,
    /// repo_url)`. The `created`/`assigned`/`mentioned`/`review_requested` flags
    /// scope results to the authenticated user.
    async fn search_prs(
        &self,
        state_filter: &str,
        since: Option<&str>,
    ) -> Result<Vec<(String, String, u64, String, Option<String>)>, SourceError> {
        #[derive(Deserialize)]
        struct SearchIssue {
            number: u64,
            #[serde(default)]
            updated_at: Option<String>,
            repository: SearchRepo,
        }
        #[derive(Deserialize)]
        struct SearchRepo {
            full_name: String,
            #[serde(default)]
            html_url: Option<String>,
        }
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut query = vec![
                ("type", "pulls".to_string()),
                ("state", state_filter.to_string()),
                ("created", "true".to_string()),
                ("assigned", "true".to_string()),
                ("mentioned", "true".to_string()),
                ("review_requested", "true".to_string()),
                ("page", page.to_string()),
                ("limit", "50".to_string()),
            ];
            if let Some(s) = since {
                query.push(("since", s.to_string()));
            }
            let batch: Vec<SearchIssue> = self.get("/repos/issues/search", &query).await?;
            let n = batch.len();
            for it in batch {
                if let Some((owner, repo)) = it.repository.full_name.split_once('/') {
                    out.push((
                        owner.to_string(),
                        repo.to_string(),
                        it.number,
                        it.updated_at.unwrap_or_default(),
                        it.repository.html_url,
                    ));
                }
            }
            if n < 50 {
                break;
            }
            if page == MAX_PAGES {
                warn!(
                    fetched = out.len(),
                    cap = MAX_PAGES,
                    "gitea list truncated at the page cap; newer items may be missed this poll"
                );
            }
        }
        Ok(out)
    }

    /// Diff a batch of swept PRs, extending `events` and marking each processed.
    /// Per-PR gated by the `pr:` cursor so an unchanged PR is skipped.
    #[allow(clippy::too_many_arguments)]
    async fn diff_swept_prs(
        &self,
        state: &dyn StateStore,
        prs: Vec<(String, String, u64, String, Option<String>)>,
        processed: &mut HashSet<String>,
        events: &mut Vec<Event>,
        viewer: &str,
        poll_start: OffsetDateTime,
        first_sight_backfill: Option<Backfill>,
        // Whether the listing this batch came from will keep returning a PR
        // indefinitely. Only the open-PR search will. The closed sweep and the
        // notifications inbox are bounded by watermarks that advance every poll
        // regardless, so deferring a retry on those risks the listing moving past
        // the PR first, turning a delayed event into a lost one.
        date_unbounded: bool,
    ) -> Result<(), SourceError> {
        for (owner, repo, index, updated_at, repo_url) in prs {
            let scope = format!("{owner}/{repo}#{index}");
            if processed.contains(&scope) {
                continue;
            }
            if date_unbounded && !self.backoff.ready(&scope, poll_start) {
                // Defer only a PR there is already a snapshot for. The diff applies
                // its age watermark solely on first sight (`!old.initialized`), so
                // for a snapshot-backed PR a deferred fetch is late and never lossy:
                // events accumulate and keep their original `occurred_at`. For a PR
                // navi has never seen, the watermark is computed at the poll that
                // succeeds, so any delay risks baselining the very activity that
                // triggered the sighting. Cheap because the state read only happens
                // for a scope already known to be failing.
                if state.get_snapshot(SOURCE_ID, &scope).await?.is_some() {
                    debug!(%scope, "skipping a gitea PR that is backed off after repeated fetch failures");
                    continue;
                }
            }
            let seen_key = format!("pr:{scope}");
            if let Some(seen) = state.get_cursor(SOURCE_ID, &seen_key).await? {
                if updated_at.as_str() <= seen.as_str() {
                    continue;
                }
            }
            let outcome = self
                .process_pr(
                    state,
                    &owner,
                    &repo,
                    index,
                    repo_url,
                    Some(poll_start - FIRST_SIGHT_LEEWAY),
                    first_sight_backfill,
                    viewer,
                    poll_start,
                )
                .await?;
            // Defer the cursor advance to `commit_snapshots` (after delivery).
            // Skipped entirely when the PR wasn't fetched: advancing then would leave
            // the cursor ahead of the snapshot and hide the PR until it changes again.
            if outcome.may_advance_cursor() {
                self.pending_pr_cursors
                    .lock()
                    .unwrap()
                    .insert(scope.clone(), updated_at);
            }
            events.extend(outcome.into_events());
            processed.insert(scope);
        }
        Ok(())
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
        self.pending_pr_cursors.lock().unwrap().clear();
        let since = state.get_cursor(SOURCE_ID, "notif_since").await?;
        let notifs = self.notifications(since.as_deref()).await?;

        // On the first poll ever, the involved-PR sweep applies the configured
        // backfill mode instead of the normal recent-only first-sight.
        // `review_requests` is the established behaviour, so only `none`/`all_open`
        // need the override.
        let first_run = state.get_cursor(SOURCE_ID, "backfilled").await?.is_none();
        let sweep_backfill =
            (first_run && self.backfill != Backfill::ReviewRequests).then_some(self.backfill);

        let mut events = Vec::new();
        // Scopes handled this poll, so the involved-PR sweep doesn't re-process one.
        let mut processed: HashSet<String> = HashSet::new();
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
            let outcome = self
                .process_pr(
                    state,
                    owner,
                    repo,
                    index,
                    n.repository.html_url.clone(),
                    first_sight_watermark(n.updated_at.as_deref()),
                    // Notifications are always "just happened", never backfill.
                    None,
                    &viewer,
                    poll_start,
                )
                .await?;
            // No cursor to guard here: gitea's notification pass is bounded by the
            // global `notif_since` watermark, not by a per-thread one.
            events.extend(outcome.into_events());
            processed.insert(scope);
        }

        // Involved-PR sweep: catches self-merges/closes and activity on your own PRs
        // that Gitea doesn't notify you about. Mirrors the GitHub source.
        let mut open_swept = 0usize;
        let mut closed_swept = 0usize;
        if self.track_prs {
            match self.involved_open_prs().await {
                Ok(prs) => {
                    open_swept = prs.len();
                    self.diff_swept_prs(
                        state,
                        prs,
                        &mut processed,
                        &mut events,
                        &viewer,
                        poll_start,
                        sweep_backfill,
                        // The open search has no date bound, so a deferred retry is
                        // guaranteed another chance.
                        true,
                    )
                    .await?;
                }
                Err(e) => warn!(error = %e, "could not search your involved gitea PRs; skipping"),
            }
            // Recently closed/merged sweep, skipped on the first poll (no cursor) so
            // it baselines forward instead of replaying history.
            if let Some(closed_since) = state.get_cursor(SOURCE_ID, "pr_closed_since").await? {
                match self.recently_closed_prs(&closed_since).await {
                    Ok(prs) => {
                        closed_swept = prs.len();
                        self.diff_swept_prs(
                            state,
                            prs,
                            &mut processed,
                            &mut events,
                            &viewer,
                            poll_start,
                            None,
                            // Bounded by `pr_closed_since`, which advances every poll
                            // whether or not this PR was fetched, so deferring a
                            // retry here could lose a self-merge rather than delay it.
                            false,
                        )
                        .await?;
                    }
                    Err(e) => {
                        warn!(error = %e, "could not search your recently-closed gitea PRs; skipping")
                    }
                }
            }
        }

        let next_since = (poll_start - SINCE_OVERLAP)
            .format(&Rfc3339)
            .map_err(|e| SourceError::Other(Box::new(e)))?;
        state
            .put_cursor(SOURCE_ID, "notif_since", &next_since)
            .await?;
        // Advance (and on first run, initialize) the closed-sweep window. Second
        // precision: some search backends reject subsecond `since` values.
        let closed_since =
            OffsetDateTime::from_unix_timestamp((poll_start - SINCE_OVERLAP).unix_timestamp())
                .map_err(|e| SourceError::Other(Box::new(e)))?
                .format(&Rfc3339)
                .map_err(|e| SourceError::Other(Box::new(e)))?;
        state
            .put_cursor(SOURCE_ID, "pr_closed_since", &closed_since)
            .await?;
        // Mark the initial catch-up done so later polls use normal first-sight.
        if first_run {
            state.put_cursor(SOURCE_ID, "backfilled", "1").await?;
        }

        // One INFO summary of what this poll examined (see the GitHub source).
        info!(
            notifications = notifs.len(),
            open_found = open_swept,
            closed_found = closed_swept,
            derived = events.len(),
            "gitea poll"
        );
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

        // Flush deferred involved-sweep `pr:` cursors, skipping failed scopes so a
        // dropped delivery re-derives the PR next poll.
        let pending_cursors: Vec<(String, String)> =
            self.pending_pr_cursors.lock().unwrap().drain().collect();
        for (scope, value) in pending_cursors {
            if failed_scopes.contains(&scope) {
                continue;
            }
            if let Err(e) = state
                .put_cursor(SOURCE_ID, &format!("pr:{scope}"), &value)
                .await
                .map_err(SourceError::from)
            {
                warn!(%scope, error = %e, "failed to persist involved-PR cursor; it will re-derive next poll");
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
        // Permanent: the PR is deleted, or in a repo this token can no longer see.
        // Kept distinct from a 5xx so callers can stop asking instead of retrying it
        // on every poll for ever. 410 is the deleted-resource response.
        404 | 410 => Err(SourceError::Gone(format!("gitea returned {status}"))),
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
