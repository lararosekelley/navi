//! Slack destination for navi.
//!
//! Delivers each event as a Block Kit DM via a bot token (`chat.postMessage`). The
//! target channel is resolved once: a `U…` user id (or the token's own identity via
//! `"self"`) is turned into a DM channel with `conversations.open`; a `C…`/`#name`
//! value is used directly.

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

const DEFAULT_API_BASE: &str = "https://slack.com/api";
const MAX_ATTEMPTS: u32 = 3;

pub struct SlackDestinationConfig {
    pub token: String,
    /// `"self"`, a user id (`U…`), a channel id (`C…`), or `#channel-name`.
    pub dm_to: String,
    /// Override the Slack Web API base URL. `None` uses `https://slack.com/api`.
    /// Primarily for pointing tests at a mock server.
    pub api_base: Option<String>,
}

pub struct SlackDestination {
    client: reqwest::Client,
    token: String,
    dm_to: String,
    api_base: String,
    channel: OnceCell<String>,
}

impl SlackDestination {
    pub fn new(config: SlackDestinationConfig) -> Result<Self, DestinationError> {
        if config.token.trim().is_empty() {
            return Err(DestinationError::Auth(
                "Slack token is empty; set NAVI_SLACK_TOKEN".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| DestinationError::Delivery(format!("building HTTP client: {e}")))?;
        Ok(Self {
            client,
            token: config.token,
            dm_to: config.dm_to,
            api_base: config
                .api_base
                .unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
            channel: OnceCell::new(),
        })
    }

    /// Verify credentials and return the authenticated identity string (for `test-slack`).
    pub async fn verify(&self) -> Result<String, DestinationError> {
        let resp: AuthTest = self.call("auth.test", &json!({})).await?;
        Ok(format!(
            "{} (team {})",
            resp.user.unwrap_or_else(|| "?".into()),
            resp.team.unwrap_or_else(|| "?".into())
        ))
    }

    /// Resolve (once) the channel id to post into.
    async fn target(&self) -> Result<&str, DestinationError> {
        self.channel
            .get_or_try_init(|| async {
                // A concrete channel id or name is used verbatim.
                if self.dm_to.starts_with('C') || self.dm_to.starts_with('#') {
                    return Ok(self.dm_to.clone());
                }
                let user_id = if self.dm_to == "self" {
                    let auth: AuthTest = self.call("auth.test", &json!({})).await?;
                    auth.user_id.ok_or_else(|| {
                        DestinationError::Auth("auth.test returned no user id".into())
                    })?
                } else {
                    self.dm_to.clone()
                };
                let opened: ConversationsOpen = self
                    .call("conversations.open", &json!({ "users": user_id }))
                    .await?;
                opened.channel.map(|c| c.id).ok_or_else(|| {
                    DestinationError::Delivery("conversations.open returned no channel".into())
                })
            })
            .await
            .map(String::as_str)
    }

    /// POST a Slack Web API method with a JSON body, checking the `ok` envelope.
    async fn call<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        body: &Value,
    ) -> Result<T, DestinationError> {
        let url = format!("{}/{method}", self.api_base);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| DestinationError::Delivery(format!("{method}: {e}")))?;

        if resp.status().as_u16() == 429 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse().ok())
                .unwrap_or(30);
            return Err(DestinationError::RateLimited {
                retry_after_secs: retry_after,
            });
        }

        let value: Value = resp
            .json()
            .await
            .map_err(|e| DestinationError::Delivery(format!("{method}: decoding response: {e}")))?;

        if value.get("ok").and_then(Value::as_bool) == Some(true) {
            serde_json::from_value(value)
                .map_err(|e| DestinationError::Delivery(format!("{method}: unexpected shape: {e}")))
        } else {
            let err = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error")
                .to_string();
            Err(classify_slack_error(&err))
        }
    }
}

#[async_trait]
impl Destination for SlackDestination {
    fn id(&self) -> &str {
        "slack"
    }

    async fn send(&self, event: &Event) -> Result<(), DestinationError> {
        self.post(&render(event), &event.dedup_key).await
    }

    async fn send_digest(&self, events: &[Event]) -> Result<(), DestinationError> {
        self.post(&render_digest(events), "digest").await
    }
}

impl SlackDestination {
    /// Post a rendered message to the resolved channel, retrying transient
    /// failures. `label` is only for the debug log.
    async fn post(&self, rendered: &Rendered, label: &str) -> Result<(), DestinationError> {
        let channel = self.target().await?.to_string();
        let body = json!({
            "channel": channel,
            "text": rendered.text,
            "blocks": rendered.blocks,
            "unfurl_links": false,
        });

        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.call::<PostMessage>("chat.postMessage", &body).await {
                Ok(_) => {
                    debug!(label, channel, "delivered to slack");
                    return Ok(());
                }
                Err(DestinationError::RateLimited { retry_after_secs })
                    if attempt < MAX_ATTEMPTS =>
                {
                    warn!(retry_after_secs, attempt, "slack rate limited; backing off");
                    tokio::time::sleep(Duration::from_secs(retry_after_secs)).await;
                }
                Err(DestinationError::Delivery(_)) if attempt < MAX_ATTEMPTS => {
                    let backoff = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

fn classify_slack_error(err: &str) -> DestinationError {
    match err {
        "invalid_auth" | "not_authed" | "account_inactive" | "token_revoked" => {
            DestinationError::Auth(err.to_string())
        }
        "ratelimited" | "rate_limited" => DestinationError::RateLimited {
            retry_after_secs: 30,
        },
        other => DestinationError::Delivery(other.to_string()),
    }
}

#[derive(Deserialize)]
struct AuthTest {
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    team: Option<String>,
}

#[derive(Deserialize)]
struct ConversationsOpen {
    #[serde(default)]
    channel: Option<Channel>,
}

#[derive(Deserialize)]
struct Channel {
    id: String,
}

#[derive(Deserialize)]
struct PostMessage {
    #[serde(default)]
    #[allow(dead_code)]
    ts: Option<String>,
}
