//! Live Discord-destination end-to-end test with read-back.
//!
//! Mirrors `navi-e2e-slack` for Discord: posts a synthetic event through navi's real
//! Discord destination in bot-DM mode, then **reads the message back** via
//! `GET /channels/{id}/messages` and asserts the unique marker it embedded actually
//! landed — proving delivery, not just a non-erroring send.
//!
//! Bot DMs need the bot and the recipient to share a server, and the recipient's DM
//! privacy to allow it. Gated behind the `e2e` feature; run by `discord-live`.
//!
//! Env:
//!   E2E_DISCORD_TOKEN   Discord bot token (View Channel + Send Messages; shared server)
//!   E2E_DISCORD_DM_TO   Discord user id (numeric snowflake) to DM
//!   E2E_DISCORD_API     Discord API base (default https://discord.com/api/v10)

use std::time::Duration;

use navi_notifier_core::model::{Actor, Event, EventKind, PullRequest, Repo, ViewerRelationship};
use navi_notifier_core::traits::{Destination, StateStore};
use navi_notifier_core::StateError;
use navi_notifier_discord::{DiscordDestination, DiscordDestinationConfig};
use serde_json::{json, Value};
use time::OffsetDateTime;

#[tokio::main]
async fn main() {
    match run().await {
        Ok(()) => println!("e2e-discord: PASSED"),
        Err(e) => {
            eprintln!("e2e-discord: FAILED: {e}");
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<(), String> {
    let token = env("E2E_DISCORD_TOKEN")?;
    let dm_to = env("E2E_DISCORD_DM_TO")?;
    let api = env_or("E2E_DISCORD_API", "https://discord.com/api/v10");

    let http = reqwest::Client::new();

    // A per-run marker carried in the PR title, which navi renders into both the
    // message content and the embed title — so we can find *this* message.
    let marker = format!("navi-e2e-discord-{}", std::process::id());

    // Preflight: a bot can only DM a user it shares a server with. Opening the DM
    // channel succeeds regardless, so we check membership up front to turn the
    // otherwise-opaque 403-on-send into an actionable message.
    preflight(&http, &api, &token, &dm_to).await?;

    // Open the same DM channel navi will post to, so we can read its history back.
    let channel = open_dm(&http, &api, &token, &dm_to).await?;
    println!("e2e-discord: DM channel {channel}, marker {marker}");

    let destination = DiscordDestination::new(DiscordDestinationConfig {
        token: Some(token.clone()),
        dm_to,
        api_base: Some(api.clone()),
    })
    .map_err(|e| format!("build discord destination: {e}"))?;
    destination
        .send(&sample_event(&marker), &NoState)
        .await
        .map_err(|e| format!("discord send failed: {e}"))?;

    // Read the message back. Discord is near-instant but can lag a beat, so retry.
    for attempt in 1..=10 {
        if history_contains(&http, &api, &token, &channel, &marker).await? {
            println!("e2e-discord: read back the delivered message (marker {marker})");
            return Ok(());
        }
        if attempt % 3 == 0 {
            println!("e2e-discord: still waiting for read-back (attempt {attempt})…");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(format!(
        "posted, but never read back a message containing {marker} from {channel}"
    ))
}

/// Verify the bot can plausibly DM the recipient: confirm the token, list the bot's
/// servers, and check the recipient is a member of at least one. If not, fail with a
/// message that names where the bot actually is — the common cause of a 403 on send.
async fn preflight(
    http: &reqwest::Client,
    api: &str,
    token: &str,
    dm_to: &str,
) -> Result<(), String> {
    let me = get(http, &format!("{api}/users/@me"), token).await?;
    let bot = me["username"].as_str().unwrap_or("?");
    let guilds = get(http, &format!("{api}/users/@me/guilds"), token).await?;
    let guilds = guilds.as_array().cloned().unwrap_or_default();
    let names: Vec<String> = guilds
        .iter()
        .filter_map(|g| g["name"].as_str().map(str::to_string))
        .collect();
    println!(
        "e2e-discord: bot={bot} is in {} server(s): [{}]",
        guilds.len(),
        names.join(", ")
    );
    if guilds.is_empty() {
        return Err("the bot is in no servers — invite it to a server you're in".into());
    }
    for g in &guilds {
        let Some(gid) = g["id"].as_str() else {
            continue;
        };
        if get_opt(http, &format!("{api}/guilds/{gid}/members/{dm_to}"), token)
            .await?
            .is_some()
        {
            let gname = g["name"].as_str().unwrap_or(gid);
            println!("e2e-discord: recipient shares server '{gname}' with the bot");
            return Ok(());
        }
    }
    Err(format!(
        "recipient {dm_to} is not a member of any server the bot is in ([{}]); \
         invite the bot to a server you're in, and confirm E2E_DISCORD_DM_TO is your user id",
        names.join(", ")
    ))
}

/// Open (or fetch) the DM channel to a user, matching navi's own bot-DM resolution.
async fn open_dm(
    http: &reqwest::Client,
    api: &str,
    token: &str,
    user_id: &str,
) -> Result<String, String> {
    let opened = post(
        http,
        &format!("{api}/users/@me/channels"),
        token,
        &json!({ "recipient_id": user_id }),
    )
    .await?;
    opened["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "users/@me/channels returned no channel id".into())
}

/// Whether the channel's recent messages include one mentioning `marker`.
async fn history_contains(
    http: &reqwest::Client,
    api: &str,
    token: &str,
    channel: &str,
    marker: &str,
) -> Result<bool, String> {
    let value = get(
        http,
        &format!("{api}/channels/{channel}/messages?limit=30"),
        token,
    )
    .await?;
    // Match the whole serialized message so the marker is found wherever navi placed
    // it (message `content` and/or the embed's `title`/fields).
    let hit = value
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
            title: format!("navi e2e discord read-back {marker}"),
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
        dedup_key: format!("navi:e2e:discord:{marker}"),
    }
}

async fn get(http: &reqwest::Client, url: &str, token: &str) -> Result<Value, String> {
    let resp = http
        .get(url)
        .header("Authorization", format!("Bot {token}"))
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    json_ok(resp, &format!("GET {url}")).await
}

/// GET returning `None` on 404 (e.g. a member lookup for a non-member), `Some` on 2xx.
async fn get_opt(http: &reqwest::Client, url: &str, token: &str) -> Result<Option<Value>, String> {
    let resp = http
        .get(url)
        .header("Authorization", format!("Bot {token}"))
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    json_ok(resp, &format!("GET {url}")).await.map(Some)
}

async fn post(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    body: &Value,
) -> Result<Value, String> {
    let resp = http
        .post(url)
        .header("Authorization", format!("Bot {token}"))
        .json(body)
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;
    json_ok(resp, &format!("POST {url}")).await
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

/// Bot mode reads/writes a cursor to group replies, but for a single one-off message
/// a no-op store (never a parent) is fine — it just posts a fresh message.
struct NoState;

#[async_trait::async_trait]
impl StateStore for NoState {
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
    async fn get_cursor(&self, _: &str, _: &str) -> Result<Option<String>, StateError> {
        Ok(None)
    }
    async fn put_cursor(&self, _: &str, _: &str, _: &str) -> Result<(), StateError> {
        Ok(())
    }
}
