//! Live GitHub-source end-to-end test: a real second identity drives an event at
//! you, and navi's real engine must derive and deliver it.
//!
//! navi observes activity *directed at you*, so a smoke test that only polls (like
//! `navi-e2e`) can't prove the derivation path. This one uses two real GitHub
//! identities against a fixed test repo: the **actor** opens an ephemeral PR and
//! requests the **viewer**'s review; navi (authenticated as the viewer) polls the
//! live notifications API, derives a `ReviewRequested`, and delivers it to a
//! Dockerized Mailpit sink. We assert the email lands, then tear the PR + branch
//! down — even on failure — so the fixed repo stays clean for the next run.
//!
//! Mailpit is the destination so this slice needs only GitHub credentials; live
//! Slack/Discord delivery is proven by their own slices. Gated behind the `e2e`
//! feature; run by the e2e workflow's `github-live` job.
//!
//! Env:
//!   E2E_GITHUB_REPO          fixed test repo as `owner/name` (e.g. you/navi-e2e)
//!   E2E_GITHUB_VIEWER_TOKEN  PAT navi authenticates as (notifications + repo read)
//!   E2E_GITHUB_ACTOR_TOKEN   PAT for a *different* account with push access, which
//!                            opens the PR and requests the viewer's review
//!   E2E_GITHUB_API           API base (default https://api.github.com; GHE differs)
//!   MAILPIT_HTTP             Mailpit REST base (default http://localhost:8025)
//!   MAILPIT_SMTP_HOST        Mailpit SMTP host (default localhost)
//!   MAILPIT_SMTP_PORT        Mailpit SMTP port (default 1025)

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use navi_notifier_core::{Engine, FilterContext, RuleConfig, RuleEngine};
use navi_notifier_email::{EmailDestination, EmailDestinationConfig, EmailTls};
use navi_notifier_github::{GitHubSource, GitHubSourceConfig};
use serde_json::{json, Value};

#[path = "../e2e_common.rs"]
mod e2e_common;
use e2e_common::{env, env_or, json_ok, MemState};

const USER_AGENT: &str = "navi-e2e";

#[tokio::main]
async fn main() {
    match run().await {
        Ok(()) => println!("e2e-github: PASSED"),
        Err(e) => {
            eprintln!("e2e-github: FAILED: {e}");
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<(), String> {
    let repo = env("E2E_GITHUB_REPO")?;
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("E2E_GITHUB_REPO must be `owner/name`, got `{repo}`"))?;
    let viewer_token = env("E2E_GITHUB_VIEWER_TOKEN")?;
    let actor_token = env("E2E_GITHUB_ACTOR_TOKEN")?;
    let api = env_or("E2E_GITHUB_API", "https://api.github.com");
    let mailpit = env_or("MAILPIT_HTTP", "http://localhost:8025");
    let smtp_host = env_or("MAILPIT_SMTP_HOST", "localhost");
    let smtp_port: u16 = env_or("MAILPIT_SMTP_PORT", "1025").parse().unwrap_or(1025);

    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("build http client: {e}"))?;

    // Confirm the two identities are distinct: navi never notifies you of your own
    // actions, so an actor equal to the viewer would derive nothing.
    let viewer = whoami(&http, &api, &viewer_token).await?;
    let actor = whoami(&http, &api, &actor_token).await?;
    if viewer.eq_ignore_ascii_case(&actor) {
        return Err(format!(
            "viewer and actor are the same account (`{viewer}`); the actor must differ so \
             navi sees the review request as directed at the viewer"
        ));
    }
    println!("e2e-github: viewer={viewer} actor={actor} repo={owner}/{name}");

    // Seed the event as the actor, then always tear it down so the fixed repo stays
    // clean — even when the assertion below fails.
    let seeded = seed(&http, &api, &actor_token, owner, name, &viewer).await?;
    // Match the *specific* ephemeral PR, not just any "requested your review": the
    // viewer is a live account whose real outstanding reviews also surface on the
    // first poll, and matching loosely would pass off that noise instead of the
    // event this run seeded. navi's subject is `[owner/name#N] actor requested…`.
    let expect = format!("[{owner}/{name}#{}]", seeded.pr_number);
    let verdict = verify(
        &http,
        &viewer_token,
        &mailpit,
        smtp_host,
        smtp_port,
        &expect,
    )
    .await;
    teardown(&http, &api, &actor_token, owner, name, &seeded).await;
    verdict
}

/// A PR opened for this run, tracked so teardown can close it and delete its branch.
struct Seeded {
    branch: String,
    pr_number: u64,
}

/// As the actor: create a branch off the default, commit a file, open a PR, and
/// request the viewer's review. Branch name is per-process so parallel/leaked runs
/// don't collide; a stale branch of the same name is deleted first.
async fn seed(
    http: &reqwest::Client,
    api: &str,
    actor_token: &str,
    owner: &str,
    name: &str,
    viewer: &str,
) -> Result<Seeded, String> {
    let branch = format!("navi-e2e-{}", std::process::id());
    let repo = format!("{api}/repos/{owner}/{name}");

    let default_branch = get(http, &repo, actor_token).await?["default_branch"]
        .as_str()
        .ok_or("repo response missing default_branch")?
        .to_string();
    let base_sha = get(
        http,
        &format!("{repo}/git/ref/heads/{default_branch}"),
        actor_token,
    )
    .await?["object"]["sha"]
        .as_str()
        .ok_or("base ref missing sha")?
        .to_string();

    // Best-effort delete of a same-named branch a prior run may have leaked.
    let _ = delete(
        http,
        &format!("{repo}/git/refs/heads/{branch}"),
        actor_token,
    )
    .await;
    post(
        http,
        &format!("{repo}/git/refs"),
        actor_token,
        &json!({ "ref": format!("refs/heads/{branch}"), "sha": base_sha }),
    )
    .await?;
    put(
        http,
        &format!("{repo}/contents/navi-e2e.txt"),
        actor_token,
        &json!({
            "message": "navi e2e: review request",
            "content": base64::engine::general_purpose::STANDARD.encode("navi e2e change\n"),
            "branch": branch,
        }),
    )
    .await?;
    let pr = post(
        http,
        &format!("{repo}/pulls"),
        actor_token,
        &json!({
            "title": "navi e2e: review request",
            "head": branch,
            "base": default_branch,
            "body": "Ephemeral PR from the navi e2e suite. Auto-closed after the run.",
        }),
    )
    .await?;
    let pr_number = pr["number"].as_u64().ok_or("PR response missing number")?;
    post(
        http,
        &format!("{repo}/pulls/{pr_number}/requested_reviewers"),
        actor_token,
        &json!({ "reviewers": [viewer] }),
    )
    .await?;
    println!("e2e-github: opened {owner}/{name}#{pr_number}, requested review of {viewer}");
    Ok(Seeded { branch, pr_number })
}

/// Run navi's engine (GitHub source as the viewer -> email to Mailpit) and poll
/// until an email whose subject contains `expect` (the ephemeral PR marker) lands.
/// GitHub notifications are eventually consistent, so retry.
async fn verify(
    http: &reqwest::Client,
    viewer_token: &str,
    mailpit: &str,
    smtp_host: String,
    smtp_port: u16,
    expect: &str,
) -> Result<(), String> {
    let source = GitHubSource::new(GitHubSourceConfig {
        token: viewer_token.to_string(),
        api_base: None,
        track_prs: false,
        mark_read: false,
        comment_min_age_secs: 0,
        backfill: Default::default(),
    })
    .map_err(|e| format!("build github source: {e}"))?;
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

    println!("e2e-github: polling navi + checking Mailpit for {expect}…");
    for attempt in 1..=45 {
        let report = engine.run_once(FilterContext::default(), false).await;
        for (src, err) in &report.source_errors {
            eprintln!("e2e-github: source {src} error: {err}");
        }
        if let Some(subject) = mailpit_review_request(http, mailpit, expect).await? {
            println!("e2e-github: email delivered, subject: {subject}");
            return Ok(());
        }
        if attempt % 5 == 0 {
            println!("e2e-github: still waiting (attempt {attempt})…");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(format!(
        "no review-request email for {expect} arrived in Mailpit within 45 polls"
    ))
}

/// Close the PR and delete its branch. Best-effort: cleanup failures are logged but
/// never mask the test verdict.
async fn teardown(
    http: &reqwest::Client,
    api: &str,
    actor_token: &str,
    owner: &str,
    name: &str,
    seeded: &Seeded,
) {
    let repo = format!("{api}/repos/{owner}/{name}");
    if let Err(e) = patch(
        http,
        &format!("{repo}/pulls/{}", seeded.pr_number),
        actor_token,
        &json!({ "state": "closed" }),
    )
    .await
    {
        eprintln!("e2e-github: teardown: closing PR failed: {e}");
    }
    if let Err(e) = delete(
        http,
        &format!("{repo}/git/refs/heads/{}", seeded.branch),
        actor_token,
    )
    .await
    {
        eprintln!("e2e-github: teardown: deleting branch failed: {e}");
    }
    println!("e2e-github: torn down {owner}/{name}#{}", seeded.pr_number);
}

/// The `login` of whoever a token authenticates as.
async fn whoami(http: &reqwest::Client, api: &str, token: &str) -> Result<String, String> {
    get(http, &format!("{api}/user"), token).await?["login"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "GET /user response missing login".into())
}

/// The subject of a Mailpit message that is a review request for the specific PR
/// identified by `expect` (e.g. `[owner/name#7]`), if any. Scoping to the PR marker
/// keeps a live viewer's unrelated real review requests from passing the test.
async fn mailpit_review_request(
    http: &reqwest::Client,
    mailpit: &str,
    expect: &str,
) -> Result<Option<String>, String> {
    let value = get_unauthed(http, &format!("{mailpit}/api/v1/messages")).await?;
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
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    json_ok(resp, &format!("GET {url}")).await
}

async fn get_unauthed(http: &reqwest::Client, url: &str) -> Result<Value, String> {
    let resp = http
        .get(url)
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
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
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
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("PUT {url}: {e}"))?;
    json_ok(resp, &format!("PUT {url}")).await
}

async fn patch(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    body: &Value,
) -> Result<Value, String> {
    let resp = http
        .patch(url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .json(body)
        .send()
        .await
        .map_err(|e| format!("PATCH {url}: {e}"))?;
    json_ok(resp, &format!("PATCH {url}")).await
}

async fn delete(http: &reqwest::Client, url: &str, token: &str) -> Result<(), String> {
    let resp = http
        .delete(url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
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
