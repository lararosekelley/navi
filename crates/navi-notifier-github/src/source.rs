//! The GitHub [`Source`]: turns notifications into fetches, fetches into diffs,
//! and diffs into normalized events. All GitHub-specific I/O lives here; the
//! decision logic lives in the pure `navi-notifier-forge` diff engine.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use navi_notifier_core::model::{
    Actor, Event, EventKind, MergeQueueRemoval, Repo, ViewerRelationship,
};
use navi_notifier_core::traits::{Source, StateStore};
use navi_notifier_core::{Backfill, SourceError};
use navi_notifier_forge::model::{IssueComment, PrData, PullRequest, Review, ReviewComment, User};
use navi_notifier_forge::{
    diff, first_sight_watermark, team_key, DiffContext, PrSnapshot, FIRST_SIGHT_LEEWAY,
};
use octocrab::Octocrab;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tokio::sync::OnceCell;
use tracing::{info, warn};

/// A team from `GET /user/teams`, reduced to what we need to match team requests.
#[derive(Deserialize)]
struct GithubTeam {
    slug: String,
    organization: GithubOrg,
}

#[derive(Deserialize)]
struct GithubOrg {
    login: String,
}

/// What the GitHub token can see, for `navi doctor`.
pub struct GitHubDoctor {
    pub login: String,
    /// Orgs the token can see, or `None` if the request failed (token likely
    /// lacks `read:org` or needs SAML re-authorization) - distinct from an
    /// empty list, which genuinely means "no orgs".
    pub orgs: Option<Vec<String>>,
    /// Whether `read:org` is present (team review requests can be detected).
    pub team_detection: bool,
}

use crate::notification::Notification;

const SOURCE_ID: &str = "github";
/// Sentinel stored in the merge-queue cursor to mean "not in the queue", so a
/// missing cursor can be told apart from a known not-queued state (first sight).
const MQ_ABSENT: &str = "absent";
/// How long a "this repo has no merge queue" verdict is trusted before re-checking,
/// so enabling a queue later is picked up without querying every PR every poll.
const MQ_CONFIG_TTL: Duration = Duration::hours(24);
/// Safety cap on pagination per endpoint so a pathological PR can't stall a poll.
const MAX_PAGES: u8 = 10;
/// Notifications page deeper than per-PR sub-resources: a single poll after a long
/// gap (or the very first, before a `since` cursor exists) can span many pages.
const NOTIF_MAX_PAGES: u8 = 30;
/// Overlap window when advancing the `since` cursor, to tolerate clock skew.
const SINCE_OVERLAP: Duration = Duration::minutes(5);

/// Configuration for the GitHub source.
pub struct GitHubSourceConfig {
    pub token: String,
    /// API base for GitHub Enterprise Server (e.g. `https://ghe.example.com/api/v3`).
    pub api_base: Option<String>,
    /// Poll your involved open PRs directly (search), on top of notifications.
    pub track_prs: bool,
    /// Mark a notification thread read once its event has been delivered.
    pub mark_read: bool,
    /// Hold a comment back until it is at least this many seconds old (0 = off), so
    /// edit-in-place bots resolve to their final text before we notify.
    pub comment_min_age_secs: u64,
    /// How much pre-existing activity to surface on the very first poll.
    pub backfill: Backfill,
}

pub struct GitHubSource {
    octo: Octocrab,
    /// Cached authenticated login, resolved lazily on first poll.
    viewer: OnceCell<String>,
    track_prs: bool,
    mark_read: bool,
    /// scope (`owner/repo#n`) -> notification thread id, for the mark-read commit
    /// hook. Populated during a poll, only when `mark_read` is on.
    threads: Mutex<HashMap<String, String>>,
    /// scope (`owner/repo#n`) -> serialized new snapshot, deferred during a poll and
    /// flushed by `commit_snapshots` only for PRs whose delivery didn't fail, so a
    /// failed send can't advance state past an undelivered event.
    pending_snapshots: Mutex<HashMap<String, Vec<u8>>>,
    /// Min comment age before notifying (`None` = off), passed through to the diff.
    comment_min_age: Option<Duration>,
    /// First-run backfill mode, applied to the involved-PR sweep on the first poll.
    backfill: Backfill,
    /// scope (`owner/repo#n`) -> merge-queue state to persist, deferred like the
    /// snapshots and flushed by `commit_snapshots` only for scopes whose delivery
    /// didn't fail, so a failed send re-derives the queue transition next poll.
    pending_mq: Mutex<HashMap<String, String>>,
    /// scope (`owner/repo#n`) -> involved-sweep `pr:` cursor value, deferred the same
    /// way: advancing it before delivery would skip a swept PR whose event never
    /// sent, losing it until the PR next changes.
    pending_pr_cursors: Mutex<HashMap<String, String>>,
}

impl GitHubSource {
    pub fn new(config: GitHubSourceConfig) -> Result<Self, SourceError> {
        if config.token.trim().is_empty() {
            return Err(SourceError::Auth(
                "GitHub token is empty; set NAVI_GITHUB_TOKEN".into(),
            ));
        }
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
            track_prs: config.track_prs,
            mark_read: config.mark_read,
            threads: Mutex::new(HashMap::new()),
            pending_snapshots: Mutex::new(HashMap::new()),
            comment_min_age: (config.comment_min_age_secs > 0)
                .then(|| Duration::seconds(config.comment_min_age_secs as i64)),
            backfill: config.backfill,
            pending_mq: Mutex::new(HashMap::new()),
            pending_pr_cursors: Mutex::new(HashMap::new()),
        })
    }

    /// The viewer's team memberships as `"org/slug"` keys, for matching team review
    /// requests. Fetched every poll (not cached like the login) so joining or
    /// leaving a team is picked up without a restart; it's one cheap request.
    /// Best effort: if the token can't list teams (needs `read:org`), team requests
    /// just won't be detected.
    async fn viewer_team_keys(&self) -> HashSet<String> {
        match self.get_all::<GithubTeam>("/user/teams").await {
            Ok(teams) => teams
                .into_iter()
                .map(|t| team_key(&t.organization.login, &t.slug))
                .collect(),
            Err(e) => {
                warn!(error = %e, "could not list your teams; team review requests won't be detected");
                HashSet::new()
            }
        }
    }

    /// Report the authenticated identity and what the token can actually see - the
    /// orgs it's authorized for and whether team detection (`read:org`) works. This
    /// surfaces the silent org-blindness (SAML SSO, muted repos) that otherwise
    /// looks like navi being broken.
    pub async fn doctor(&self) -> Result<GitHubDoctor, SourceError> {
        let login = self.viewer_login().await?.to_string();
        let orgs = self
            .get_all::<GithubOrg>("/user/orgs")
            .await
            .map(|v| v.into_iter().map(|o| o.login).collect())
            .ok();
        let team_detection = self.get_all::<GithubTeam>("/user/teams").await.is_ok();
        Ok(GitHubDoctor {
            login,
            orgs,
            team_detection,
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
        for page in 1..=NOTIF_MAX_PAGES {
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
            let full = batch.len() == 100;
            out.extend(batch);
            if !full {
                return Ok(out);
            }
            // Last page still full at the cap: more remain that we won't fetch
            // this pass. Surface it rather than dropping silently.
            if page == NOTIF_MAX_PAGES {
                warn!(
                    fetched = out.len(),
                    cap_pages = NOTIF_MAX_PAGES,
                    "notifications truncated at the page cap; some may be missed this poll \
                     (a shorter poll interval keeps each batch smaller)"
                );
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
            let full = batch.len() == 100;
            out.extend(batch);
            if !full {
                return Ok(out);
            }
            // Last page still full at the cap: a PR with a huge review/comment
            // history is truncated. Rare, but surface it rather than drop silently.
            if page == MAX_PAGES {
                warn!(%path, fetched = out.len(), "list truncated at the page cap");
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

    /// Fetch, diff, and persist one PR against its stored snapshot; returns the
    /// events. Shared by the notifications and involved-PR paths so both dedupe
    /// through the same snapshot key and `dedup_key`s.
    #[allow(clippy::too_many_arguments)]
    async fn process_pr(
        &self,
        state: &dyn StateStore,
        owner: &str,
        repo: &str,
        number: u64,
        repo_url: Option<String>,
        first_sight_since: Option<OffsetDateTime>,
        first_sight_backfill: Option<Backfill>,
        viewer: &str,
        viewer_teams: &HashSet<String>,
        now: OffsetDateTime,
    ) -> Result<Vec<Event>, SourceError> {
        let scope = format!("{owner}/{repo}#{number}");
        let pr_data = match self.fetch_pr(owner, repo, number).await {
            Ok(d) => d,
            Err(e) => {
                // One inaccessible PR (deleted, perms) shouldn't abort the poll.
                warn!(%scope, error = %e, "failed to fetch PR; skipping");
                return Ok(Vec::new());
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
            viewer_teams: viewer_teams.clone(),
            comment_min_age: self.comment_min_age,
            first_sight_backfill,
        };
        let (mut evs, new_snapshot) = diff(&ctx, &pr_data, &old);
        let bytes = serde_json::to_vec(&new_snapshot)
            .map_err(|e| SourceError::Parse(format!("serialize snapshot {scope}: {e}")))?;
        // Defer persistence to `commit_snapshots` (after delivery), so a send failure
        // leaves the prior snapshot in place and this PR's events re-derive next poll.
        self.pending_snapshots
            .lock()
            .unwrap()
            .insert(scope.clone(), bytes);

        // Merge-queue transitions. Self-gating: a cheap per-repo cache skips repos
        // known not to use a queue, and the query itself confirms via
        // `isMergeQueueEnabled`. Best-effort: a failed query skips this pass.
        match self
            .merge_queue_event(state, &ctx.repo, &pr_data, viewer, now)
            .await
        {
            Ok(Some(ev)) => evs.push(ev),
            Ok(None) => {}
            Err(e) => warn!(%scope, error = %e, "merge-queue check failed; skipping"),
        }
        Ok(evs)
    }

    /// Diff a batch of swept PRs (from the open or closed involved-PR search),
    /// extending `events` and marking each `scope` processed. Per-PR gated by the
    /// `pr:` cursor so an unchanged PR is skipped. There's no triggering
    /// notification to anchor first sight, so activity from the last leeway window
    /// is surfaced and older history baselined; `first_sight_backfill` overrides
    /// that on the first poll (open sweep only).
    #[allow(clippy::too_many_arguments)]
    async fn diff_swept_prs(
        &self,
        state: &dyn StateStore,
        prs: Vec<(String, String, u64, String)>,
        processed: &mut HashSet<String>,
        events: &mut Vec<Event>,
        viewer: &str,
        viewer_teams: &HashSet<String>,
        poll_start: OffsetDateTime,
        first_sight_backfill: Option<Backfill>,
    ) -> Result<(), SourceError> {
        for (owner, repo, number, updated_at) in prs {
            let scope = format!("{owner}/{repo}#{number}");
            if processed.contains(&scope) {
                continue;
            }
            let seen_key = format!("pr:{scope}");
            if let Some(seen) = state.get_cursor(SOURCE_ID, &seen_key).await? {
                if updated_at.as_str() <= seen.as_str() {
                    continue;
                }
            }
            let evs = self
                .process_pr(
                    state,
                    &owner,
                    &repo,
                    number,
                    Some(format!("https://github.com/{owner}/{repo}")),
                    Some(poll_start - FIRST_SIGHT_LEEWAY),
                    first_sight_backfill,
                    viewer,
                    viewer_teams,
                    poll_start,
                )
                .await?;
            // Defer the cursor advance to `commit_snapshots` (after delivery), so a
            // failed send leaves the cursor in place and this PR re-derives next poll.
            self.pending_pr_cursors
                .lock()
                .unwrap()
                .insert(scope.clone(), updated_at);
            events.extend(evs);
            processed.insert(scope);
        }
        Ok(())
    }

    /// Detect whether the PR's repo uses a merge queue, and if so diff its queue
    /// state against the last-seen state to build an entered/removed event. Skips
    /// (cheaply, via a cached per-repo verdict) repos without a queue, baselines
    /// silently on first sight, and suppresses a "removed" that is really a merge.
    async fn merge_queue_event(
        &self,
        state: &dyn StateStore,
        repo: &Repo,
        pr_data: &PrData,
        viewer: &str,
        now: OffsetDateTime,
    ) -> Result<Option<Event>, SourceError> {
        let pr = &pr_data.pull_request;

        // Fast path: skip repos we recently confirmed have no merge queue.
        let cfg_key = format!("mqcfg:{}", repo.full_name());
        if let Some(cached) = state.get_cursor(SOURCE_ID, &cfg_key).await? {
            if is_fresh_no_queue(&cached, now) {
                return Ok(None);
            }
        }

        let (enabled, current, enqueued_at) = self
            .merge_queue_status(&repo.owner, &repo.name, pr.number)
            .await?;
        let verdict = if enabled { "yes" } else { "no" };
        let stamp = now.format(&Rfc3339).unwrap_or_default();
        state
            .put_cursor(SOURCE_ID, &cfg_key, &format!("{verdict}|{stamp}"))
            .await?;
        if !enabled {
            return Ok(None);
        }

        let scope = format!("{}#{}", repo.full_name(), pr.number);
        let prev = state.get_cursor(SOURCE_ID, &format!("mq:{scope}")).await?;
        let stored = current.as_deref().unwrap_or(MQ_ABSENT);
        // Defer the state advance to `commit_snapshots` (after delivery), so a failed
        // send re-derives the transition instead of skipping it.
        self.pending_mq
            .lock()
            .unwrap()
            .insert(scope, stored.to_string());

        // No prior state means first sight: baseline, don't back-fill a transition.
        let kind = match prev {
            Some(prev) => merge_queue_change(Some(prev.as_str()), current.as_deref()),
            None => None,
        };
        let Some(kind) = kind else { return Ok(None) };

        // A PR that leaves the queue because it merged already produces a Merged
        // event; don't also report it as removed from the queue.
        if matches!(kind, EventKind::RemovedFromMergeQueue { .. }) && pr.merged {
            return Ok(None);
        }

        let author_login = pr
            .user
            .as_ref()
            .map(|u| u.login.as_str())
            .unwrap_or("ghost");
        // Prefer the entry's enqueue time as a stable discriminator (GitHub doesn't
        // always bump the PR's updated_at on a queue change); fall back to it.
        let disc = match &kind {
            EventKind::EnteredMergeQueue => format!(
                "merge_queue_entered:{}",
                enqueued_at.unwrap_or_else(|| ts_key(pr.updated_at.as_deref()))
            ),
            _ => format!("merge_queue_removed:{}", ts_key(pr.updated_at.as_deref())),
        };
        Ok(Some(Event {
            source_id: SOURCE_ID.to_string(),
            kind,
            pull_request: navi_notifier_core::model::PullRequest {
                repo: repo.clone(),
                number: pr.number,
                title: pr.title.clone(),
                url: pr.html_url.clone(),
                author: Actor::new(author_login),
                draft: pr.draft,
            },
            viewer: ViewerRelationship {
                is_author: author_login.eq_ignore_ascii_case(viewer),
                is_reviewer: false,
                actor_is_viewer: false,
            },
            actor: Actor::new(author_login),
            occurred_at: pr
                .updated_at
                .as_deref()
                .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                .unwrap_or(now),
            target_url: Some(pr.html_url.clone()),
            excerpt: None,
            dedup_key: Event::make_dedup_key(SOURCE_ID, repo, pr.number, &disc),
        }))
    }

    /// One GraphQL call returning `(is a merge queue enabled on this PR's base,
    /// current queue-entry state or None, the entry's enqueue timestamp or None)`.
    /// REST (octocrab's default) exposes none of these. Values are passed as GraphQL
    /// variables rather than interpolated into the query string.
    async fn merge_queue_status(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<(bool, Option<String>, Option<String>), SourceError> {
        let query = "query($owner: String!, $name: String!, $number: Int!) { \
             repository(owner: $owner, name: $name) { \
             pullRequest(number: $number) { \
             isMergeQueueEnabled mergeQueueEntry { state enqueuedAt } } } }";
        let resp: serde_json::Value = self
            .octo
            .graphql(&serde_json::json!({
                "query": query,
                "variables": { "owner": owner, "name": repo, "number": number },
            }))
            .await
            .map_err(map_err)?;
        if let Some(errors) = resp.get("errors") {
            return Err(SourceError::Request(format!("graphql: {errors}")));
        }
        let pr = resp.pointer("/data/repository/pullRequest");
        let enabled = pr
            .and_then(|p| p.get("isMergeQueueEnabled"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let str_at = |field: &str| {
            pr.and_then(|p| p.pointer(&format!("/mergeQueueEntry/{field}")))
                .and_then(|s| s.as_str())
                .map(str::to_string)
        };
        Ok((enabled, str_at("state"), str_at("enqueuedAt")))
    }

    /// Open PRs the viewer is involved in (author, reviewer, assignee, commenter,
    /// mentioned), via search - independent of notification settings, so it still
    /// finds activity on muted repos and reviews on your own PRs that GitHub never
    /// puts in your notifications. Returns `(owner, repo, number, updated_at)`.
    async fn involved_open_prs(
        &self,
        viewer: &str,
    ) -> Result<Vec<(String, String, u64, String)>, SourceError> {
        self.search_prs(&format!("is:open is:pr involves:{viewer}"), "open")
            .await
    }

    /// Involved PRs closed or merged since `since` (RFC3339). This is what catches a
    /// merge or close you performed yourself: GitHub never notifies you about your
    /// own actions, and a closed PR has already left the open sweep, so without this
    /// the transition is invisible. Bounded by `updated:>=` so it doesn't rescan
    /// history. Returns `(owner, repo, number, updated_at)`.
    async fn recently_closed_prs(
        &self,
        viewer: &str,
        since: &str,
    ) -> Result<Vec<(String, String, u64, String)>, SourceError> {
        let q = format!("is:closed is:pr involves:{viewer} updated:>={since}");
        self.search_prs(&q, "closed").await
    }

    /// Run a `/search/issues` query across pages, returning `(owner, repo, number,
    /// updated_at)` for each hit. `kind` is only for the page-cap warning.
    async fn search_prs(
        &self,
        q: &str,
        kind: &str,
    ) -> Result<Vec<(String, String, u64, String)>, SourceError> {
        #[derive(Serialize)]
        struct Params<'a> {
            q: &'a str,
            per_page: u8,
            page: u8,
        }
        #[derive(Deserialize)]
        struct SearchPage {
            items: Vec<SearchItem>,
        }
        #[derive(Deserialize)]
        struct SearchItem {
            repository_url: String,
            number: u64,
            updated_at: String,
        }

        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let res: SearchPage = self
                .octo
                .get(
                    "/search/issues",
                    Some(&Params {
                        q,
                        per_page: 100,
                        page,
                    }),
                )
                .await
                .map_err(map_err)?;
            let n = res.items.len();
            for item in res.items {
                if let Some((owner, repo)) = parse_repo_url(&item.repository_url) {
                    out.push((owner, repo, item.number, item.updated_at));
                }
            }
            if n < 100 {
                break;
            }
            if page == MAX_PAGES {
                warn!(
                    kind,
                    cap = MAX_PAGES as u32 * 100,
                    "involved-PR search hit the page cap; some PRs skipped this poll"
                );
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl Source for GitHubSource {
    fn id(&self) -> &str {
        SOURCE_ID
    }

    async fn poll(&self, state: &dyn StateStore) -> Result<Vec<Event>, SourceError> {
        let viewer = self.viewer_login().await?.to_string();
        let viewer_teams = self.viewer_team_keys().await;
        let poll_start = OffsetDateTime::now_utc();
        // Fresh stash each pass; a prior pass's deferred snapshots were either
        // flushed by `commit_snapshots` or (on a dry run) are intentionally dropped.
        self.pending_snapshots.lock().unwrap().clear();
        self.pending_mq.lock().unwrap().clear();
        self.pending_pr_cursors.lock().unwrap().clear();
        if self.mark_read {
            self.threads.lock().unwrap().clear();
        }
        let since = state.get_cursor(SOURCE_ID, "notif_since").await?;

        // On the first poll ever (no cursor yet), the involved-PR sweep applies the
        // configured backfill mode instead of the normal recent-only first-sight.
        // `review_requests` is exactly the established behaviour, so only `none` and
        // `all_open` need the override.
        let first_run = state.get_cursor(SOURCE_ID, "backfilled").await?.is_none();
        let sweep_backfill =
            (first_run && self.backfill != Backfill::ReviewRequests).then_some(self.backfill);
        // Backfill runs on the involved-PR sweep, which only happens with track_prs.
        // Warn rather than silently no-op when the two are configured at odds.
        if first_run && self.backfill == Backfill::AllOpen && !self.track_prs {
            warn!("backfill = \"all_open\" needs github.track_prs = true; skipping first-run backfill");
        }

        let notifs = self.notifications(since.as_deref()).await?;

        let mut events = Vec::new();
        // Scopes handled this poll, so the involved-PR pass doesn't re-process one.
        let mut processed: HashSet<String> = HashSet::new();

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

            // Skip threads whose notification hasn't advanced. An optimisation
            // only; the snapshot would suppress duplicates anyway. Do NOT mark the
            // scope processed here: a notification thread doesn't always advance
            // when the PR itself changes (bare approvals, activity on your own
            // PRs), and blocking the involved-PR pass would reintroduce that miss.
            let seen_key = format!("thread:{scope}");
            let last_seen = state.get_cursor(SOURCE_ID, &seen_key).await?;
            if let (Some(seen), Some(updated)) = (&last_seen, &n.updated_at) {
                if updated.as_str() <= seen.as_str() {
                    continue;
                }
            }

            if self.mark_read {
                self.threads
                    .lock()
                    .unwrap()
                    .insert(scope.clone(), n.id.clone());
            }

            let evs = self
                .process_pr(
                    state,
                    &owner,
                    &repo,
                    number,
                    n.repository.html_url.clone(),
                    first_sight_watermark(n.updated_at.as_deref()),
                    // Notifications are always "just happened", never backfill.
                    None,
                    &viewer,
                    &viewer_teams,
                    poll_start,
                )
                .await?;
            if let Some(updated) = &n.updated_at {
                state.put_cursor(SOURCE_ID, &seen_key, updated).await?;
            }
            events.extend(evs);
            processed.insert(scope);
        }

        // Involved open PRs, independent of the notifications inbox: catches
        // reviews on your own PRs and activity in muted repos that GitHub never
        // surfaces as a notification. A per-PR cursor keeps it cheap (only PRs
        // whose `updated_at` advanced get fetched).
        let mut open_swept = 0usize;
        let mut closed_swept = 0usize;
        if self.track_prs {
            // Open involved PRs: activity on PRs GitHub may not have notified about.
            match self.involved_open_prs(&viewer).await {
                Ok(prs) => {
                    open_swept = prs.len();
                    self.diff_swept_prs(
                        state,
                        prs,
                        &mut processed,
                        &mut events,
                        &viewer,
                        &viewer_teams,
                        poll_start,
                        sweep_backfill,
                    )
                    .await?;
                }
                Err(e) => {
                    warn!(error = %e, "could not search your involved PRs; skipping that pass");
                }
            }
            // Recently closed/merged involved PRs. GitHub never notifies you about a
            // merge or close you did yourself, and a closed PR has left the open
            // sweep, so this is the only way navi sees your own self-merge/close.
            // Skipped on the very first poll (no cursor) so it baselines forward
            // instead of replaying history.
            if let Some(since) = state.get_cursor(SOURCE_ID, "pr_closed_since").await? {
                match self.recently_closed_prs(&viewer, &since).await {
                    Ok(prs) => {
                        closed_swept = prs.len();
                        self.diff_swept_prs(
                            state,
                            prs,
                            &mut processed,
                            &mut events,
                            &viewer,
                            &viewer_teams,
                            poll_start,
                            None,
                        )
                        .await?;
                    }
                    Err(e) => {
                        warn!(error = %e, "could not search your recently-closed PRs; skipping that pass");
                    }
                }
            }
        }

        // Advance the list cursor with a small overlap so nothing straddling the
        // boundary is missed on the next poll.
        let next_since = (poll_start - SINCE_OVERLAP)
            .format(&Rfc3339)
            .map_err(|e| SourceError::Other(Box::new(e)))?;
        state
            .put_cursor(SOURCE_ID, "notif_since", &next_since)
            .await?;
        // Advance (and on first run, initialize) the closed-sweep window. Second
        // precision: GitHub search's `updated:` qualifier rejects subseconds.
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

        // One INFO summary of what this poll examined, so `navi logs` shows whether
        // navi saw the activity at all - not just how much it delivered.
        // `*_found` are search-result counts (before the per-PR cursor skip);
        // `derived` is the events actually produced.
        info!(
            notifications = notifs.len(),
            open_found = open_swept,
            closed_found = closed_swept,
            derived = events.len(),
            "github poll"
        );
        Ok(events)
    }

    /// Mark the notification thread read once an event has been delivered, when
    /// enabled. Only notification-derived events map to a thread; PRs found via the
    /// involved-PR search have none, and are left alone.
    async fn commit(&self, _state: &dyn StateStore, event: &Event) -> Result<(), SourceError> {
        if !self.mark_read {
            return Ok(());
        }
        let scope = event.scope();
        // Take, not clone: a PR that emits several events this pass should PATCH the
        // thread once, not once per event. Next poll repopulates the map anyway.
        let thread_id = self.threads.lock().unwrap().remove(&scope);
        // No thread mapped for this scope (e.g. found via search, not a notification).
        let Some(raw) = thread_id else {
            return Ok(());
        };
        let Ok(id) = raw.parse::<u64>() else {
            warn!(raw = %raw, scope = %scope, "mark-read: thread id is not numeric, skipping");
            return Ok(());
        };
        self.octo
            .activity()
            .notifications()
            .mark_as_read(id.into())
            .await
            .map_err(map_err)?;
        Ok(())
    }

    /// Persist the snapshots deferred during `poll`, skipping any PR whose delivery
    /// failed this pass so its events re-derive next time. Draining the stash makes
    /// this idempotent if called twice.
    async fn commit_snapshots(
        &self,
        state: &dyn StateStore,
        failed_scopes: &HashSet<String>,
    ) -> Result<(), SourceError> {
        let pending: Vec<(String, Vec<u8>)> =
            self.pending_snapshots.lock().unwrap().drain().collect();
        // Attempt every entry: a single write failure must not drop the others
        // (they were already drained). A scope we fail to persist just re-derives
        // next poll. Surface the first error after trying them all.
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

        // Flush deferred merge-queue state the same way: skip failed scopes so the
        // transition re-derives, persist the rest.
        let pending_mq: Vec<(String, String)> = self.pending_mq.lock().unwrap().drain().collect();
        for (scope, value) in pending_mq {
            if failed_scopes.contains(&scope) {
                continue;
            }
            if let Err(e) = state
                .put_cursor(SOURCE_ID, &format!("mq:{scope}"), &value)
                .await
                .map_err(SourceError::from)
            {
                warn!(%scope, error = %e, "failed to persist merge-queue state; it will re-derive next poll");
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

/// Parse `https://api.github.com/repos/{owner}/{repo}/pulls/{number}` into parts.
/// Stable-enough discriminator fragment from a timestamp string.
fn ts_key(raw: Option<&str>) -> String {
    raw.unwrap_or("0").to_string()
}

/// Whether a cached merge-queue config verdict says "no queue" and is still within
/// [`MQ_CONFIG_TTL`]. Format is `"yes|<rfc3339>"` / `"no|<rfc3339>"`; anything
/// unparseable falls through to re-checking.
fn is_fresh_no_queue(cached: &str, now: OffsetDateTime) -> bool {
    let Some((verdict, stamp)) = cached.split_once('|') else {
        return false;
    };
    if verdict != "no" {
        return false;
    }
    OffsetDateTime::parse(stamp, &Rfc3339)
        .map(|checked| now - checked < MQ_CONFIG_TTL)
        .unwrap_or(false)
}

/// Derive a merge-queue transition from the previously-stored state and the current
/// one. A PR counts as *actively queued* when it has an entry in any state other
/// than `UNMERGEABLE`; `MQ_ABSENT`, `None`, and `UNMERGEABLE` all count as not
/// queued. Entering fires on inactive -> active; removal on active -> inactive, with
/// the reason distinguishing a clean dequeue from being kicked as unmergeable.
fn merge_queue_change(prev: Option<&str>, current: Option<&str>) -> Option<EventKind> {
    let active = |s: Option<&str>| matches!(s, Some(st) if st != MQ_ABSENT && st != "UNMERGEABLE");
    let was = active(prev);
    let now = active(current);
    match (was, now) {
        (false, true) => Some(EventKind::EnteredMergeQueue),
        (true, false) => {
            let reason = if current == Some("UNMERGEABLE") {
                MergeQueueRemoval::Unmergeable
            } else {
                MergeQueueRemoval::Dequeued
            };
            Some(EventKind::RemovedFromMergeQueue { reason })
        }
        _ => None,
    }
}

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

/// Parse `https://api.github.com/repos/{owner}/{repo}` (a search item's
/// `repository_url`) into `(owner, repo)`.
fn parse_repo_url(url: &str) -> Option<(String, String)> {
    let after = url.split("/repos/").nth(1)?;
    let mut parts = after.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?.to_string();
    let repo = parts.next().filter(|s| !s.is_empty())?.to_string();
    Some((owner, repo))
}

fn map_err(err: octocrab::Error) -> SourceError {
    classify_github_error(&err.to_string())
}

/// Classify a GitHub error message. A 403 is only a rate limit when the message
/// says so (an unauthenticated or over-quota call); a plain 403 is a permission
/// problem, not something to silently retry.
fn classify_github_error(msg: &str) -> SourceError {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("rate limit") {
        SourceError::RateLimited {
            retry_after_secs: 60,
        }
    } else if lower.contains("bad credentials")
        || lower.contains("unauthorized")
        || lower.contains("401")
    {
        SourceError::Auth(format!("invalid GitHub token: {msg}"))
    } else if lower.contains("forbidden")
        || lower.contains("resource not accessible")
        || lower.contains("403")
    {
        SourceError::Auth(format!(
            "GitHub returned 403 (forbidden); the token likely lacks required scopes \
             (notifications + repo/PR read): {msg}"
        ))
    } else {
        SourceError::Request(msg.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_github_error, merge_queue_change, parse_pr_url, parse_repo_url, MQ_ABSENT,
    };
    use navi_notifier_core::model::{EventKind, MergeQueueRemoval};
    use navi_notifier_core::SourceError;

    #[test]
    fn merge_queue_transitions() {
        // Not queued -> queued: entered.
        assert_eq!(
            merge_queue_change(Some(MQ_ABSENT), Some("QUEUED")),
            Some(EventKind::EnteredMergeQueue)
        );
        // Queued -> gone: a clean dequeue.
        assert_eq!(
            merge_queue_change(Some("AWAITING_CHECKS"), None),
            Some(EventKind::RemovedFromMergeQueue {
                reason: MergeQueueRemoval::Dequeued
            })
        );
        // Queued -> unmergeable: kicked out.
        assert_eq!(
            merge_queue_change(Some("MERGEABLE"), Some("UNMERGEABLE")),
            Some(EventKind::RemovedFromMergeQueue {
                reason: MergeQueueRemoval::Unmergeable
            })
        );
        // No change either way.
        assert_eq!(merge_queue_change(Some(MQ_ABSENT), None), None);
        assert_eq!(
            merge_queue_change(Some("QUEUED"), Some("AWAITING_CHECKS")),
            None
        );
        // Unmergeable is treated as not-queued, so it never counts as "entered".
        assert_eq!(
            merge_queue_change(Some(MQ_ABSENT), Some("UNMERGEABLE")),
            None
        );
    }

    #[test]
    fn parse_repo_url_extracts_owner_and_repo() {
        assert_eq!(
            parse_repo_url("https://api.github.com/repos/acme/widgets"),
            Some(("acme".into(), "widgets".into()))
        );
        assert_eq!(parse_repo_url("https://api.github.com/user"), None);
        assert_eq!(parse_repo_url("nonsense"), None);
    }

    #[test]
    fn rate_limit_messages_are_rate_limited() {
        assert!(matches!(
            classify_github_error("API rate limit exceeded for 1.2.3.4"),
            SourceError::RateLimited { .. }
        ));
        assert!(matches!(
            classify_github_error("You have exceeded a secondary rate limit"),
            SourceError::RateLimited { .. }
        ));
    }

    #[test]
    fn bad_credentials_is_auth() {
        assert!(matches!(
            classify_github_error("Bad credentials"),
            SourceError::Auth(_)
        ));
    }

    #[test]
    fn forbidden_is_auth_not_rate_limited() {
        match classify_github_error("Resource not accessible by personal access token") {
            SourceError::Auth(m) => assert!(m.contains("403")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn other_errors_are_request() {
        assert!(matches!(
            classify_github_error("connection reset by peer"),
            SourceError::Request(_)
        ));
    }

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
