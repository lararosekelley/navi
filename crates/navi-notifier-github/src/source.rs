//! The GitHub [`Source`]: turns notifications into fetches, fetches into diffs,
//! and diffs into normalized events. All GitHub-specific I/O lives here; the
//! decision logic lives in the pure `navi-notifier-forge` diff engine.

use std::collections::HashSet;

use async_trait::async_trait;
use navi_notifier_core::model::{Event, Repo};
use navi_notifier_core::traits::{Source, StateStore};
use navi_notifier_core::SourceError;
use navi_notifier_forge::model::{IssueComment, PrData, PullRequest, Review, ReviewComment, User};
use navi_notifier_forge::{
    diff, first_sight_watermark, team_key, DiffContext, PrSnapshot, FIRST_SIGHT_LEEWAY,
};
use octocrab::Octocrab;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tokio::sync::OnceCell;
use tracing::{debug, warn};

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
}

pub struct GitHubSource {
    octo: Octocrab,
    /// Cached authenticated login, resolved lazily on first poll.
    viewer: OnceCell<String>,
    track_prs: bool,
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
        };
        let (evs, new_snapshot) = diff(&ctx, &pr_data, &old);
        let bytes = serde_json::to_vec(&new_snapshot)
            .map_err(|e| SourceError::Parse(format!("serialize snapshot {scope}: {e}")))?;
        state.put_snapshot(SOURCE_ID, &scope, &bytes).await?;
        Ok(evs)
    }

    /// Open PRs the viewer is involved in (author, reviewer, assignee, commenter,
    /// mentioned), via search - independent of notification settings, so it still
    /// finds activity on muted repos and reviews on your own PRs that GitHub never
    /// puts in your notifications. Returns `(owner, repo, number, updated_at)`.
    async fn involved_open_prs(
        &self,
        viewer: &str,
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

        let q = format!("is:open is:pr involves:{viewer}");
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let res: SearchPage = self
                .octo
                .get(
                    "/search/issues",
                    Some(&Params {
                        q: &q,
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
                    cap = MAX_PAGES as u32 * 100,
                    "involved-PR search hit the page cap; some open PRs skipped this poll"
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
        let since = state.get_cursor(SOURCE_ID, "notif_since").await?;

        let notifs = self.notifications(since.as_deref()).await?;
        debug!(count = notifs.len(), "fetched notifications");

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

            let evs = self
                .process_pr(
                    state,
                    &owner,
                    &repo,
                    number,
                    n.repository.html_url.clone(),
                    first_sight_watermark(n.updated_at.as_deref()),
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
        if self.track_prs {
            match self.involved_open_prs(&viewer).await {
                Ok(prs) => {
                    debug!(count = prs.len(), "fetched involved open PRs");
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
                        // No triggering notification here to anchor first sight, so
                        // surface only activity from the last leeway window; older
                        // history is baselined silently (no first-run backlog).
                        let evs = self
                            .process_pr(
                                state,
                                &owner,
                                &repo,
                                number,
                                Some(format!("https://github.com/{owner}/{repo}")),
                                Some(poll_start - FIRST_SIGHT_LEEWAY),
                                &viewer,
                                &viewer_teams,
                                poll_start,
                            )
                            .await?;
                        state.put_cursor(SOURCE_ID, &seen_key, &updated_at).await?;
                        events.extend(evs);
                        processed.insert(scope);
                    }
                }
                Err(e) => {
                    warn!(error = %e, "could not search your involved PRs; skipping that pass");
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
    use super::{classify_github_error, parse_pr_url, parse_repo_url};
    use navi_notifier_core::SourceError;

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
