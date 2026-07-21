//! Discord destination for navi.
//!
//! Two delivery modes, chosen by `dm_to`:
//! - a webhook URL (`https://discord.com/api/webhooks/...`) posts an embed to that
//!   channel with no token, the simplest setup;
//! - a user id (snowflake) opens a DM with the bot token and posts there.

mod render;

use std::time::Duration;

use async_trait::async_trait;
use navi_notifier_core::traits::Destination;
use navi_notifier_core::{DestinationError, Event};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::OnceCell;
use tracing::{debug, warn};

pub use render::{render, render_digest, Rendered};

const DEFAULT_API_BASE: &str = "https://discord.com/api/v10";
const MAX_ATTEMPTS: u32 = 3;

pub struct DiscordDestinationConfig {
    /// Bot token. Required for user-DM mode; ignored in webhook mode.
    pub token: Option<String>,
    /// A webhook URL (`https://...`) or a Discord user id to DM.
    pub dm_to: String,
    /// Override the API base (bot mode only). Primarily for tests.
    pub api_base: Option<String>,
}

enum Mode {
    /// Post directly to this webhook URL, no auth.
    Webhook(String),
    /// Open a DM with this user id using the bot token.
    Dm(String),
}

pub struct DiscordDestination {
    client: reqwest::Client,
    token: Option<String>,
    api_base: String,
    mode: Mode,
    /// Resolved DM channel id (bot mode only).
    channel: OnceCell<String>,
}

impl DiscordDestination {
    pub fn new(config: DiscordDestinationConfig) -> Result<Self, DestinationError> {
        // A user id is a numeric snowflake; anything with a URL scheme is a webhook.
        let mode = if config.dm_to.contains("://") {
            Mode::Webhook(config.dm_to.clone())
        } else {
            match config.token.as_deref() {
                Some(t) if !t.trim().is_empty() => Mode::Dm(config.dm_to.clone()),
                _ => {
                    return Err(DestinationError::Auth(
                        "Discord DM mode needs a bot token; set NAVI_DISCORD_TOKEN \
                         (or use a webhook URL as dm_to)"
                            .into(),
                    ))
                }
            }
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| DestinationError::Delivery(format!("building HTTP client: {e}")))?;
        Ok(Self {
            client,
            token: config.token,
            api_base: config
                .api_base
                .unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
            mode,
            channel: OnceCell::new(),
        })
    }

    /// Confirm the bot token (or report webhook mode).
    pub async fn verify(&self) -> Result<String, DestinationError> {
        match &self.mode {
            Mode::Webhook(_) => Ok("webhook".to_string()),
            Mode::Dm(_) => {
                let me: DiscordUser = self.get(&format!("{}/users/@me", self.api_base)).await?;
                Ok(me.username.unwrap_or_else(|| "bot".into()))
            }
        }
    }

    /// The message endpoint to POST to, resolving a DM channel once if needed.
    async fn endpoint(&self) -> Result<String, DestinationError> {
        match &self.mode {
            Mode::Webhook(url) => Ok(url.clone()),
            Mode::Dm(user_id) => {
                let channel = self
                    .channel
                    .get_or_try_init(|| async {
                        let opened: Channel = self
                            .post_json(
                                &format!("{}/users/@me/channels", self.api_base),
                                &json!({ "recipient_id": user_id }),
                            )
                            .await?;
                        Ok::<_, DestinationError>(opened.id)
                    })
                    .await?;
                Ok(format!("{}/channels/{}/messages", self.api_base, channel))
            }
        }
    }

    fn bot_auth(&self) -> Option<String> {
        match &self.mode {
            Mode::Dm(_) => self.token.as_ref().map(|t| format!("Bot {t}")),
            Mode::Webhook(_) => None,
        }
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T, DestinationError> {
        let mut req = self.client.get(url);
        if let Some(auth) = self.bot_auth() {
            req = req.header("Authorization", auth);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| DestinationError::Delivery(e.to_string()))?;
        self.decode(resp).await
    }

    async fn post_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        body: &Value,
    ) -> Result<T, DestinationError> {
        let resp = self.post_raw(url, body).await?;
        self.decode(resp).await
    }

    async fn post_raw(
        &self,
        url: &str,
        body: &Value,
    ) -> Result<reqwest::Response, DestinationError> {
        let mut req = self.client.post(url).json(body);
        if let Some(auth) = self.bot_auth() {
            req = req.header("Authorization", auth);
        }
        req.send()
            .await
            .map_err(|e| DestinationError::Delivery(e.to_string()))
    }

    /// Turn a response into `T`, mapping 429 and error statuses.
    async fn decode<T: for<'de> Deserialize<'de>>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, DestinationError> {
        check_status(&resp)?;
        // Discord returns 204 (empty) for webhooks; treat empty as unit-like.
        let text = resp
            .text()
            .await
            .map_err(|e| DestinationError::Delivery(e.to_string()))?;
        if text.trim().is_empty() {
            return serde_json::from_str("null")
                .map_err(|e| DestinationError::Delivery(format!("empty body decode: {e}")));
        }
        serde_json::from_str(&text)
            .map_err(|e| DestinationError::Delivery(format!("unexpected response: {e}")))
    }
}

#[async_trait]
impl Destination for DiscordDestination {
    fn id(&self) -> &str {
        "discord"
    }

    async fn send(&self, event: &Event) -> Result<(), DestinationError> {
        self.post(&render(event), &event.dedup_key).await
    }

    async fn send_digest(&self, events: &[Event]) -> Result<(), DestinationError> {
        self.post(&render_digest(events), "digest").await
    }
}

impl DiscordDestination {
    /// Post a rendered message to the resolved endpoint, retrying transient
    /// failures. `label` is only for the debug log.
    async fn post(&self, rendered: &Rendered, label: &str) -> Result<(), DestinationError> {
        let endpoint = self.endpoint().await?;
        let body = json!({ "content": rendered.content, "embeds": [rendered.embed] });

        let mut attempt = 0;
        loop {
            attempt += 1;
            let resp = self.post_raw(&endpoint, &body).await?;
            match check_status(&resp) {
                Ok(()) => {
                    debug!(label, "delivered to discord");
                    return Ok(());
                }
                Err(DestinationError::RateLimited { retry_after_secs })
                    if attempt < MAX_ATTEMPTS =>
                {
                    warn!(
                        retry_after_secs,
                        attempt, "discord rate limited; backing off"
                    );
                    tokio::time::sleep(Duration::from_secs(retry_after_secs.max(1))).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Map a response status to an error, recognising Discord's 429.
fn check_status(resp: &reqwest::Response) -> Result<(), DestinationError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    if status.as_u16() == 429 {
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<f64>().ok())
            .map(|s| s.ceil() as u64)
            .unwrap_or(1);
        return Err(DestinationError::RateLimited {
            retry_after_secs: retry_after,
        });
    }
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(DestinationError::Auth(format!(
            "discord rejected the request: {status}"
        )));
    }
    Err(DestinationError::Delivery(format!(
        "discord returned {status}"
    )))
}

#[derive(Deserialize)]
struct Channel {
    id: String,
}

#[derive(Deserialize)]
struct DiscordUser {
    #[serde(default)]
    username: Option<String>,
}
