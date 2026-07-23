//! Live GitLab-source end-to-end test: a real second identity drives an event at
//! you, and navi's real engine must derive and deliver it.
//!
//! Mirrors `navi-e2e-github` for GitLab. Two real identities against a fixed test
//! project: the **actor** opens an ephemeral MR with the **viewer** as a reviewer,
//! which creates a `review_requested` todo; navi (authenticated as the viewer)
//! polls the live Todos API, derives a `ReviewRequested`, and delivers it to a
//! Dockerized Mailpit sink. We assert the email lands for that specific MR, then
//! close the MR and delete its branch — even on failure.
//!
//! Mailpit is the destination so this slice needs only GitLab credentials; live
//! Slack/Discord delivery is proven by their own slices. Gated behind the `e2e`
//! feature; run by the e2e workflow's `gitlab-live` job.
//!
//! Env:
//!   E2E_GITLAB_PROJECT       fixed test project as `namespace/name` (e.g. you/navi-e2e)
//!   E2E_GITLAB_VIEWER_TOKEN  PAT navi authenticates as (read_api)
//!   E2E_GITLAB_ACTOR_TOKEN   token for a *different* member (project access token or
//!                            second-user PAT) with `api` scope + Developer role, which
//!                            opens the MR and adds the viewer as a reviewer
//!   E2E_GITLAB_API           API base (default https://gitlab.com/api/v4; self-hosted differs)
//!   MAILPIT_HTTP             Mailpit REST base (default http://localhost:8025)
//!   MAILPIT_SMTP_HOST        Mailpit SMTP host (default localhost)
//!   MAILPIT_SMTP_PORT        Mailpit SMTP port (default 1025)

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use navi_notifier_core::traits::StateStore;
use navi_notifier_core::{Engine, FilterContext, RuleConfig, RuleEngine, StateError};
use navi_notifier_email::{EmailDestination, EmailDestinationConfig, EmailTls};
use navi_notifier_gitlab::{GitLabSource, GitLabSourceConfig};
use serde_json::{json, Value};

#[tokio::main]
async fn main() {
    match run().await {
        Ok(()) => println!("e2e-gitlab: PASSED"),
        Err(e) => {
            eprintln!("e2e-gitlab: FAILED: {e}");
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<(), String> {
    let project = env("E2E_GITLAB_PROJECT")?;
    let viewer_token = env("E2E_GITLAB_VIEWER_TOKEN")?;
    let actor_token = env("E2E_GITLAB_ACTOR_TOKEN")?;
    let api = env_or("E2E_GITLAB_API", "https://gitlab.com/api/v4");
    let mailpit = env_or("MAILPIT_HTTP", "http://localhost:8025");
    let smtp_host = env_or("MAILPIT_SMTP_HOST", "localhost");
    let smtp_port: u16 = env_or("MAILPIT_SMTP_PORT", "1025").parse().unwrap_or(1025);

    let http = reqwest::Client::new();
    let enc_project = project.replace('/', "%2F");

    // The reviewer (viewer) and author (actor) must be distinct: navi never notifies
    // you of your own actions, so the todo only appears for a different reviewer.
    let viewer = whoami(&http, &api, &viewer_token).await?;
    let actor = whoami(&http, &api, &actor_token).await?;
    if viewer.username.eq_ignore_ascii_case(&actor.username) {
        return Err(format!(
            "viewer and actor are the same account (`{}`); the actor must differ so \
             navi sees the review request as directed at the viewer",
            viewer.username
        ));
    }
    println!(
        "e2e-gitlab: viewer={} actor={} project={project}",
        viewer.username, actor.username
    );

    let seeded = seed(&http, &api, &actor_token, &enc_project, viewer.id).await?;
    // Match the *specific* ephemeral MR, not just any "requested your review": a live
    // viewer's real outstanding reviews also surface, and matching loosely would pass
    // off that noise. navi's subject is `[namespace/name#iid] actor requested…`.
    let expect = format!("[{}#{}]", seeded.project_path, seeded.iid);
    let verdict = verify(
        &http,
        &viewer_token,
        &api,
        &mailpit,
        smtp_host,
        smtp_port,
        &expect,
    )
    .await;
    teardown(&http, &api, &actor_token, &enc_project, &seeded).await;
    verdict
}

/// An MR opened for this run, tracked so teardown can close it and delete its branch.
struct Seeded {
    branch: String,
    iid: u64,
    /// The project's canonical `path_with_namespace`, used to build the subject marker.
    project_path: String,
}

/// As the actor: create a branch off the default, commit a file, open an MR with the
/// viewer as a reviewer. Branch name is per-process so leaked runs don't collide; a
/// stale branch of the same name is deleted first.
async fn seed(
    http: &reqwest::Client,
    api: &str,
    actor_token: &str,
    enc_project: &str,
    viewer_id: u64,
) -> Result<Seeded, String> {
    let branch = format!("navi-e2e-{}", std::process::id());
    let base = format!("{api}/projects/{enc_project}");

    let project = get(http, &base, actor_token).await?;
    let project_path = project["path_with_namespace"]
        .as_str()
        .ok_or("project response missing path_with_namespace")?
        .to_string();
    let default_branch = project["default_branch"]
        .as_str()
        .ok_or("project has no default_branch (is it empty?)")?
        .to_string();

    // Best-effort delete of a same-named branch a prior run may have leaked.
    let _ = delete(
        http,
        &format!("{base}/repository/branches/{branch}"),
        actor_token,
    )
    .await;
    post(
        http,
        &format!("{base}/repository/branches?branch={branch}&ref={default_branch}"),
        actor_token,
        &json!({}),
    )
    .await?;
    post(
        http,
        &format!("{base}/repository/files/navi-e2e.txt"),
        actor_token,
        &json!({
            "branch": branch,
            "content": "navi e2e change\n",
            "commit_message": "navi e2e: review request",
        }),
    )
    .await?;
    let mr = post(
        http,
        &format!("{base}/merge_requests"),
        actor_token,
        &json!({
            "source_branch": branch,
            "target_branch": default_branch,
            "title": "navi e2e: review request",
            "description": "Ephemeral MR from the navi e2e suite. Auto-closed after the run.",
            "reviewer_ids": [viewer_id],
        }),
    )
    .await?;
    let iid = mr["iid"].as_u64().ok_or("MR response missing iid")?;
    println!("e2e-gitlab: opened {project_path}!{iid}, requested review of viewer {viewer_id}");
    Ok(Seeded {
        branch,
        iid,
        project_path,
    })
}

/// Run navi's engine (GitLab source as the viewer -> email to Mailpit) and poll
/// until an email whose subject contains `expect` (the ephemeral MR marker) lands.
/// GitLab todos are eventually consistent, so retry.
#[allow(clippy::too_many_arguments)]
async fn verify(
    http: &reqwest::Client,
    viewer_token: &str,
    api: &str,
    mailpit: &str,
    smtp_host: String,
    smtp_port: u16,
    expect: &str,
) -> Result<(), String> {
    let source = GitLabSource::new(GitLabSourceConfig {
        token: viewer_token.to_string(),
        api_base: Some(api.to_string()),
        comment_min_age_secs: 0,
        backfill: Default::default(),
    })
    .map_err(|e| format!("build gitlab source: {e}"))?;
    let email = EmailDestination::new(EmailDestinationConfig {
        smtp_host,
        smtp_port,
        tls: EmailTls::None,
        username: None,
        password: None,
        from: "navi <navi@navi.local>".into(),
        to: "you <you@navi.local>".into(),
    })
    .map_err(|e| format!("build email destination: {e}"))?;
    let engine = Engine::new(
        vec![Arc::new(source)],
        vec![Arc::new(email)],
        vec![],
        RuleEngine::new(RuleConfig::default()).expect("default rules"),
        Arc::new(MemState::default()),
    );

    println!("e2e-gitlab: polling navi + checking Mailpit for {expect}…");
    for attempt in 1..=45 {
        let report = engine.run_once(FilterContext::default(), false).await;
        for (src, err) in &report.source_errors {
            eprintln!("e2e-gitlab: source {src} error: {err}");
        }
        if let Some(subject) = mailpit_review_request(http, mailpit, expect).await? {
            println!("e2e-gitlab: email delivered, subject: {subject}");
            return Ok(());
        }
        if attempt % 5 == 0 {
            println!("e2e-gitlab: still waiting (attempt {attempt})…");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(format!(
        "no review-request email for {expect} arrived in Mailpit within 45 polls"
    ))
}

/// Close the MR and delete its branch. Best-effort: cleanup failures are logged but
/// never mask the test verdict.
async fn teardown(
    http: &reqwest::Client,
    api: &str,
    actor_token: &str,
    enc_project: &str,
    seeded: &Seeded,
) {
    let base = format!("{api}/projects/{enc_project}");
    if let Err(e) = put(
        http,
        &format!("{base}/merge_requests/{}", seeded.iid),
        actor_token,
        &json!({ "state_event": "close" }),
    )
    .await
    {
        eprintln!("e2e-gitlab: teardown: closing MR failed: {e}");
    }
    if let Err(e) = delete(
        http,
        &format!("{base}/repository/branches/{}", seeded.branch),
        actor_token,
    )
    .await
    {
        eprintln!("e2e-gitlab: teardown: deleting branch failed: {e}");
    }
    println!(
        "e2e-gitlab: torn down {}!{}",
        seeded.project_path, seeded.iid
    );
}

/// A GitLab user identity from `GET /user`.
struct Who {
    id: u64,
    username: String,
}

async fn whoami(http: &reqwest::Client, api: &str, token: &str) -> Result<Who, String> {
    let user = get(http, &format!("{api}/user"), token).await?;
    let id = user["id"].as_u64().ok_or("GET /user missing id")?;
    let username = user["username"]
        .as_str()
        .ok_or("GET /user missing username")?
        .to_string();
    Ok(Who { id, username })
}

/// The subject of a Mailpit message that is a review request for the specific MR
/// identified by `expect` (e.g. `[you/navi-e2e#7]`), if any. Scoping to the MR marker
/// keeps a live viewer's unrelated real review requests from passing the test.
async fn mailpit_review_request(
    http: &reqwest::Client,
    mailpit: &str,
    expect: &str,
) -> Result<Option<String>, String> {
    let resp = http
        .get(format!("{mailpit}/api/v1/messages"))
        .send()
        .await
        .map_err(|e| format!("mailpit query: {e}"))?;
    let value = json_ok(resp, "mailpit query").await?;
    let found = value["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["Subject"].as_str())
        .find(|s| s.contains(expect) && s.contains("requested your review"))
        .map(str::to_string);
    Ok(found)
}

async fn get(http: &reqwest::Client, url: &str, token: &str) -> Result<Value, String> {
    let resp = http
        .get(url)
        .header("PRIVATE-TOKEN", token)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    json_ok(resp, &format!("GET {url}")).await
}

async fn post(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    body: &Value,
) -> Result<Value, String> {
    let resp = http
        .post(url)
        .header("PRIVATE-TOKEN", token)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;
    json_ok(resp, &format!("POST {url}")).await
}

async fn put(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    body: &Value,
) -> Result<Value, String> {
    let resp = http
        .put(url)
        .header("PRIVATE-TOKEN", token)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("PUT {url}: {e}"))?;
    json_ok(resp, &format!("PUT {url}")).await
}

async fn delete(http: &reqwest::Client, url: &str, token: &str) -> Result<(), String> {
    let resp = http
        .delete(url)
        .header("PRIVATE-TOKEN", token)
        .send()
        .await
        .map_err(|e| format!("DELETE {url}: {e}"))?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 404 {
        Ok(())
    } else {
        Err(format!(
            "DELETE {url}: {status}: {}",
            resp.text().await.unwrap_or_default()
        ))
    }
}

async fn json_ok(resp: reqwest::Response, what: &str) -> Result<Value, String> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("{what}: read body: {e}"))?;
    if !status.is_success() {
        return Err(format!("{what}: {status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("{what}: parse: {e}"))
}

fn env(key: &str) -> Result<String, String> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("missing env var {key}"))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// In-memory state so the poll has somewhere to keep snapshots/cursors.
#[derive(Default)]
struct MemState {
    snapshots: Mutex<HashMap<String, Vec<u8>>>,
    delivered: Mutex<HashMap<String, ()>>,
    cursors: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl StateStore for MemState {
    async fn get_snapshot(&self, s: &str, scope: &str) -> Result<Option<Vec<u8>>, StateError> {
        Ok(self.snapshots.lock().unwrap().get(&k(s, scope)).cloned())
    }
    async fn put_snapshot(&self, s: &str, scope: &str, b: &[u8]) -> Result<(), StateError> {
        self.snapshots
            .lock()
            .unwrap()
            .insert(k(s, scope), b.to_vec());
        Ok(())
    }
    async fn was_delivered(&self, key: &str) -> Result<bool, StateError> {
        Ok(self.delivered.lock().unwrap().contains_key(key))
    }
    async fn mark_delivered(&self, key: &str) -> Result<(), StateError> {
        self.delivered.lock().unwrap().insert(key.to_string(), ());
        Ok(())
    }
    async fn get_cursor(&self, s: &str, key: &str) -> Result<Option<String>, StateError> {
        Ok(self.cursors.lock().unwrap().get(&k(s, key)).cloned())
    }
    async fn put_cursor(&self, s: &str, key: &str, v: &str) -> Result<(), StateError> {
        self.cursors
            .lock()
            .unwrap()
            .insert(k(s, key), v.to_string());
        Ok(())
    }
}

fn k(a: &str, b: &str) -> String {
    format!("{a}\u{0}{b}")
}
