//! Hermetic Gitea -> Mailpit end-to-end test.
//!
//! Drives the full navi loop with no external accounts: seeds a Dockerized Gitea
//! (two users, a repo, a PR, a review request), runs navi's real engine (Gitea
//! source -> email destination), and asserts the email lands in a Dockerized
//! Mailpit sink. Gated behind the `e2e` feature; run by the e2e-gitea workflow.
//!
//! Env:
//!   GITEA_URL          Gitea base URL (default http://localhost:3000)
//!   GITEA_ADMIN_TOKEN  admin API token (the workflow seeds an admin + token)
//!   MAILPIT_HTTP       Mailpit REST base (default http://localhost:8025)
//!   MAILPIT_SMTP_HOST  Mailpit SMTP host (default localhost)
//!   MAILPIT_SMTP_PORT  Mailpit SMTP port (default 1025)

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use navi_notifier_core::{Engine, FilterContext, RuleConfig, RuleEngine};
use navi_notifier_email::{EmailDestination, EmailDestinationConfig, EmailTls};
use navi_notifier_gitea::{GiteaSource, GiteaSourceConfig};
use serde_json::{json, Value};

#[path = "../e2e_common.rs"]
mod e2e_common;
use e2e_common::{env, env_or, json_ok, MemState};

const VIEWER: &str = "viewer";
const ACTOR: &str = "actor";
const PASSWORD: &str = "navi-e2e-pass-1";

#[tokio::main]
async fn main() {
    match run().await {
        Ok(()) => println!("e2e-gitea: PASSED"),
        Err(e) => {
            eprintln!("e2e-gitea: FAILED: {e}");
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<(), String> {
    let gitea = env_or("GITEA_URL", "http://localhost:3000");
    let gitea_api = format!("{gitea}/api/v1");
    let admin = env("GITEA_ADMIN_TOKEN")?;
    let mailpit = env_or("MAILPIT_HTTP", "http://localhost:8025");
    let smtp_host = env_or("MAILPIT_SMTP_HOST", "localhost");
    let smtp_port: u16 = env_or("MAILPIT_SMTP_PORT", "1025").parse().unwrap_or(1025);

    let http = reqwest::Client::new();

    // 1. Seed Gitea: two users + tokens.
    println!("e2e-gitea: creating users…");
    create_user(&http, &gitea_api, &admin, VIEWER).await?;
    create_user(&http, &gitea_api, &admin, ACTOR).await?;
    let viewer_token = create_token(&http, &gitea_api, VIEWER, VIEWER_SCOPES).await?;
    let actor_token = create_token(&http, &gitea_api, ACTOR, ACTOR_SCOPES).await?;

    // 2. Actor opens a PR and requests the viewer's review.
    println!("e2e-gitea: creating repo, PR, and review request…");
    let repo = "widgets";
    post(
        &http,
        &format!("{gitea_api}/user/repos"),
        &actor_token,
        &json!({ "name": repo, "auto_init": true, "private": false, "default_branch": "main" }),
    )
    .await?;
    put(
        &http,
        &format!("{gitea_api}/repos/{ACTOR}/{repo}/collaborators/{VIEWER}"),
        &actor_token,
        &json!({ "permission": "read" }),
    )
    .await?;
    post(
        &http,
        &format!("{gitea_api}/repos/{ACTOR}/{repo}/contents/change.txt"),
        &actor_token,
        &json!({
            "content": base64::engine::general_purpose::STANDARD.encode("a change\n"),
            "message": "add change.txt",
            "branch": "main",
            "new_branch": "feature",
        }),
    )
    .await?;
    let pr = post(
        &http,
        &format!("{gitea_api}/repos/{ACTOR}/{repo}/pulls"),
        &actor_token,
        &json!({ "head": "feature", "base": "main", "title": "Add gizmo" }),
    )
    .await?;
    let index = pr["number"].as_u64().ok_or("PR response missing number")?;
    post(
        &http,
        &format!("{gitea_api}/repos/{ACTOR}/{repo}/pulls/{index}/requested_reviewers"),
        &actor_token,
        &json!({ "reviewers": [VIEWER] }),
    )
    .await?;
    println!("e2e-gitea: requested review of {VIEWER} on {ACTOR}/{repo}#{index}");

    // 3. Build navi's engine: Gitea source (viewer) -> email destination (Mailpit).
    let source = GiteaSource::new(GiteaSourceConfig {
        token: viewer_token,
        api_base: Some(gitea_api.clone()),
        comment_min_age_secs: 0,
        track_prs: false,
        backfill: Default::default(),
    })
    .map_err(|e| format!("build gitea source: {e}"))?;
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

    // 4. Poll until the review-request email lands in Mailpit (eventual consistency).
    println!("e2e-gitea: polling navi + checking Mailpit…");
    for attempt in 1..=30 {
        let report = engine.run_once(FilterContext::default(), false).await;
        for (src, err) in &report.source_errors {
            eprintln!("e2e-gitea: source {src} error: {err}");
        }
        if let Some(subject) = mailpit_review_request(&http, &mailpit).await? {
            println!("e2e-gitea: email delivered, subject: {subject}");
            return Ok(());
        }
        if attempt % 5 == 0 {
            println!("e2e-gitea: still waiting (attempt {attempt})…");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err("no review-request email arrived in Mailpit within 30 polls".into())
}

const VIEWER_SCOPES: &[&str] = &[
    "read:notification",
    "read:repository",
    "read:issue",
    "read:user",
];
const ACTOR_SCOPES: &[&str] = &["write:repository", "write:issue", "write:user", "read:user"];

async fn create_user(
    http: &reqwest::Client,
    api: &str,
    admin_token: &str,
    username: &str,
) -> Result<(), String> {
    let resp = http
        .post(format!("{api}/admin/users"))
        .header("Authorization", format!("token {admin_token}"))
        .json(&json!({
            "username": username,
            "email": format!("{username}@navi.local"),
            "password": PASSWORD,
            "must_change_password": false,
            "visibility": "public",
        }))
        .send()
        .await
        .map_err(|e| format!("create user {username}: {e}"))?;
    // 201 created, or 422 if it already exists (idempotent re-runs).
    let status = resp.status();
    if status.is_success() || status.as_u16() == 422 {
        Ok(())
    } else {
        Err(format!(
            "create user {username}: {status}: {}",
            resp.text().await.unwrap_or_default()
        ))
    }
}

async fn create_token(
    http: &reqwest::Client,
    api: &str,
    username: &str,
    scopes: &[&str],
) -> Result<String, String> {
    let resp = http
        .post(format!("{api}/users/{username}/tokens"))
        .basic_auth(username, Some(PASSWORD))
        .json(&json!({ "name": format!("navi-e2e-{username}"), "scopes": scopes }))
        .send()
        .await
        .map_err(|e| format!("create token {username}: {e}"))?;
    let value = json_ok(resp, &format!("create token {username}")).await?;
    value["sha1"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("token response for {username} missing sha1"))
}

async fn post(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    body: &Value,
) -> Result<Value, String> {
    let resp = http
        .post(url)
        .header("Authorization", format!("token {token}"))
        .json(body)
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;
    json_ok(resp, &format!("POST {url}")).await
}

async fn put(http: &reqwest::Client, url: &str, token: &str, body: &Value) -> Result<(), String> {
    let resp = http
        .put(url)
        .header("Authorization", format!("token {token}"))
        .json(body)
        .send()
        .await
        .map_err(|e| format!("PUT {url}: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "PUT {url}: {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        ))
    }
}

/// Return the subject of a "requested your review" message in Mailpit, if any.
async fn mailpit_review_request(
    http: &reqwest::Client,
    mailpit: &str,
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
        .find(|s| s.contains("requested your review"))
        .map(str::to_string);
    Ok(found)
}
