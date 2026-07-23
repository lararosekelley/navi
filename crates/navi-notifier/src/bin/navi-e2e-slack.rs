//! Live Slack-destination end-to-end test with read-back.
//!
//! Replaces the old send-and-hope smoke (`navi-e2e`): it posts a synthetic event
//! through navi's real Slack destination, then **reads the message back** via
//! `conversations.history` and asserts the unique marker it embedded actually
//! landed — proving delivery, not just that the API call didn't error. (The GitHub
//! auth-poll the old smoke also did is now covered far better by `navi-e2e-github`.)
//!
//! Gated behind the `e2e` feature; run by the e2e workflow's `slack-live` job.
//!
//! Env:
//!   E2E_SLACK_TOKEN   Slack bot token (chat:write + im:write + im:history to read back)
//!   E2E_SLACK_DM_TO   DM target: "self" (default), a user id (U…), or a channel (C…/#name)
//!   E2E_SLACK_API     Slack Web API base (default https://slack.com/api)

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use navi_notifier_core::model::{Actor, Event, EventKind, PullRequest, Repo, ViewerRelationship};
use navi_notifier_core::traits::{Destination, StateStore};
use navi_notifier_core::StateError;
use navi_notifier_slack::{SlackDestination, SlackDestinationConfig};
use serde_json::Value;
use time::OffsetDateTime;

#[tokio::main]
async fn main() {
    match run().await {
        Ok(()) => println!("e2e-slack: PASSED"),
        Err(e) => {
            eprintln!("e2e-slack: FAILED: {e}");
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<(), String> {
    let token = env("E2E_SLACK_TOKEN")?;
    let dm_to = env_or("E2E_SLACK_DM_TO", "self");
    let api = env_or("E2E_SLACK_API", "https://slack.com/api");

    let http = reqwest::Client::new();

    // A per-run marker carried in the PR title, which navi renders into the message's
    // fallback `text` — so we can find *this* message in the channel history.
    let marker = format!("navi-e2e-slack-{}", std::process::id());

    // Resolve the same channel navi will post to, so we can read history from it.
    let channel = resolve_channel(&http, &api, &token, &dm_to).await?;
    println!("e2e-slack: posting to channel {channel} with marker {marker}");

    let destination = SlackDestination::new(SlackDestinationConfig {
        token: token.clone(),
        dm_to,
        api_base: Some(api.clone()),
        broadcast: Vec::new(),
    })
    .map_err(|e| format!("build slack destination: {e}"))?;
    destination
        .send(&sample_event(&marker), &MemState::default())
        .await
        .map_err(|e| format!("slack send failed: {e}"))?;

    // Read the message back: conversations.history is near-instant but occasionally
    // lags a beat, so retry briefly.
    for attempt in 1..=10 {
        if history_contains(&http, &api, &token, &channel, &marker).await? {
            println!("e2e-slack: read back the delivered message (marker {marker})");
            return Ok(());
        }
        if attempt % 3 == 0 {
            println!("e2e-slack: still waiting for read-back (attempt {attempt})…");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(format!(
        "posted, but never read back a message containing {marker} from {channel}"
    ))
}

/// Resolve the channel id navi's destination will target, mirroring its own logic:
/// a `C…`/`#name` is used directly; `self` resolves via `auth.test`; a `U…` user id
/// opens a DM channel.
async fn resolve_channel(
    http: &reqwest::Client,
    api: &str,
    token: &str,
    dm_to: &str,
) -> Result<String, String> {
    if dm_to.starts_with('C') || dm_to.starts_with('#') {
        return Ok(dm_to.to_string());
    }
    let user_id = if dm_to == "self" {
        slack_get(http, api, token, "auth.test").await?["user_id"]
            .as_str()
            .ok_or("auth.test returned no user_id")?
            .to_string()
    } else {
        dm_to.to_string()
    };
    let opened = slack_post(
        http,
        api,
        token,
        "conversations.open",
        &[("users", &user_id)],
    )
    .await?;
    opened["channel"]["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "conversations.open returned no channel id".into())
}

/// Whether the channel's recent history contains a message mentioning `marker`.
async fn history_contains(
    http: &reqwest::Client,
    api: &str,
    token: &str,
    channel: &str,
    marker: &str,
) -> Result<bool, String> {
    let value = slack_post(
        http,
        api,
        token,
        "conversations.history",
        &[("channel", channel), ("limit", "30")],
    )
    .await?;
    // Match against the whole serialized message so the marker is found wherever navi
    // placed it (fallback `text` and/or Block Kit `blocks`).
    let hit = value["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|m| m.to_string().contains(marker));
    Ok(hit)
}

/// A synthetic review-request event whose PR title carries the unique `marker`.
fn sample_event(marker: &str) -> Event {
    Event {
        source_id: "github".into(),
        kind: EventKind::ReviewRequested,
        pull_request: PullRequest {
            repo: Repo::new("navi", "e2e"),
            number: 1,
            title: format!("navi e2e slack read-back {marker}"),
            url: "https://github.com/navi/e2e/pull/1".into(),
            author: Actor::new("navi-e2e"),
            draft: false,
        },
        viewer: ViewerRelationship {
            is_author: false,
            is_reviewer: true,
            actor_is_viewer: false,
        },
        actor: Actor::new("navi-e2e"),
        occurred_at: OffsetDateTime::now_utc(),
        target_url: Some("https://github.com/navi/e2e/pull/1".into()),
        excerpt: Some(format!("read-back probe {marker}")),
        dedup_key: format!("navi:e2e:slack:{marker}"),
    }
}

async fn slack_get(
    http: &reqwest::Client,
    api: &str,
    token: &str,
    method: &str,
) -> Result<Value, String> {
    let resp = http
        .get(format!("{api}/{method}"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("{method}: {e}"))?;
    slack_ok(resp, method).await
}

async fn slack_post(
    http: &reqwest::Client,
    api: &str,
    token: &str,
    method: &str,
    form: &[(&str, &str)],
) -> Result<Value, String> {
    let resp = http
        .post(format!("{api}/{method}"))
        .bearer_auth(token)
        .form(form)
        .send()
        .await
        .map_err(|e| format!("{method}: {e}"))?;
    slack_ok(resp, method).await
}

/// Parse a Slack response and surface the `ok:false` error envelope.
async fn slack_ok(resp: reqwest::Response, method: &str) -> Result<Value, String> {
    let value: Value = resp
        .json()
        .await
        .map_err(|e| format!("{method}: decode: {e}"))?;
    if value["ok"].as_bool() == Some(true) {
        Ok(value)
    } else {
        Err(format!(
            "{method}: {}",
            value["error"].as_str().unwrap_or("unknown_error")
        ))
    }
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

/// In-memory state so the destination has somewhere to keep its thread cursor.
#[derive(Default)]
struct MemState {
    cursors: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl StateStore for MemState {
    async fn get_snapshot(&self, _: &str, _: &str) -> Result<Option<Vec<u8>>, StateError> {
        Ok(None)
    }
    async fn put_snapshot(&self, _: &str, _: &str, _: &[u8]) -> Result<(), StateError> {
        Ok(())
    }
    async fn was_delivered(&self, _: &str) -> Result<bool, StateError> {
        Ok(false)
    }
    async fn mark_delivered(&self, _: &str) -> Result<(), StateError> {
        Ok(())
    }
    async fn get_cursor(&self, s: &str, key: &str) -> Result<Option<String>, StateError> {
        Ok(self
            .cursors
            .lock()
            .unwrap()
            .get(&format!("{s}:{key}"))
            .cloned())
    }
    async fn put_cursor(&self, s: &str, key: &str, v: &str) -> Result<(), StateError> {
        self.cursors
            .lock()
            .unwrap()
            .insert(format!("{s}:{key}"), v.to_string());
        Ok(())
    }
}
