//! Discord destination tests against a mock Discord API.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use navi_notifier_core::model::{
    Actor, Event, EventKind, PullRequest, Repo, ReviewState, ViewerRelationship,
};
use navi_notifier_core::traits::{Destination, StateStore};
use navi_notifier_core::StateError;
use navi_notifier_discord::{DiscordDestination, DiscordDestinationConfig};
use serde_json::{json, Value};
use time::OffsetDateTime;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A no-op store, for webhook-mode tests that don't touch the reply-chain cursor.
struct NoState;

#[async_trait]
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

/// In-memory store so the reply-chain cursor persists across two bot-mode sends.
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
    async fn get_cursor(&self, src: &str, k: &str) -> Result<Option<String>, StateError> {
        Ok(self
            .cursors
            .lock()
            .unwrap()
            .get(&format!("{src}:{k}"))
            .cloned())
    }
    async fn put_cursor(&self, src: &str, k: &str, v: &str) -> Result<(), StateError> {
        self.cursors
            .lock()
            .unwrap()
            .insert(format!("{src}:{k}"), v.to_string());
        Ok(())
    }
}

fn sample_event() -> Event {
    Event {
        source_id: "github".into(),
        kind: EventKind::ReviewSubmitted {
            state: ReviewState::Approved,
        },
        pull_request: PullRequest {
            repo: Repo::new("acme", "widgets"),
            number: 12,
            title: "Add gizmo".into(),
            url: "https://gh.test/acme/widgets/pull/12".into(),
            author: Actor::new("octo"),
            draft: false,
        },
        viewer: ViewerRelationship::default(),
        actor: Actor::new("reviewer"),
        occurred_at: OffsetDateTime::UNIX_EPOCH,
        target_url: None,
        excerpt: None,
        dedup_key: "k".into(),
    }
}

#[tokio::test]
async fn webhook_mode_posts_embed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhooks/1/abc"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let destination = DiscordDestination::new(DiscordDestinationConfig {
        token: None,
        dm_to: format!("{}/webhooks/1/abc", server.uri()),
        api_base: None,
    })
    .expect("build");
    destination
        .send(&sample_event(), &NoState)
        .await
        .expect("send");

    let reqs = server.received_requests().await.unwrap();
    let post = reqs
        .iter()
        .find(|r| r.url.path() == "/webhooks/1/abc")
        .unwrap();
    let body: Value = serde_json::from_slice(&post.body).unwrap();
    assert!(body["embeds"].is_array());
    assert!(body["embeds"][0]["description"]
        .as_str()
        .unwrap()
        .contains("approved"));
}

#[tokio::test]
async fn dm_mode_opens_channel_then_posts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/users/@me/channels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "D1" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/channels/D1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "m1" })))
        .mount(&server)
        .await;

    let destination = DiscordDestination::new(DiscordDestinationConfig {
        token: Some("bot-token".into()),
        dm_to: "123456789".into(),
        api_base: Some(server.uri()),
    })
    .expect("build");
    destination
        .send(&sample_event(), &NoState)
        .await
        .expect("send");

    let reqs = server.received_requests().await.unwrap();
    assert!(reqs.iter().any(|r| r.url.path() == "/channels/D1/messages"));
}

#[tokio::test]
async fn second_event_on_a_pr_replies_to_the_first() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/users/@me/channels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "D1" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/channels/D1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "m1" })))
        .mount(&server)
        .await;

    let state = MemState::default();
    let destination = DiscordDestination::new(DiscordDestinationConfig {
        token: Some("bot-token".into()),
        dm_to: "123456789".into(),
        api_base: Some(server.uri()),
    })
    .expect("build");
    destination
        .send(&sample_event(), &state)
        .await
        .expect("first");
    destination
        .send(&sample_event(), &state)
        .await
        .expect("second");

    let posts: Vec<Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/channels/D1/messages")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(posts.len(), 2);
    // First opens the chain (no reference); second replies to the first message.
    assert!(
        posts[0].get("message_reference").is_none(),
        "parent must not reference"
    );
    assert_eq!(posts[1]["message_reference"]["message_id"], "m1");
}

#[test]
fn dm_mode_requires_token() {
    let result = DiscordDestination::new(DiscordDestinationConfig {
        token: None,
        dm_to: "123456789".into(),
        api_base: None,
    });
    match result {
        Err(e) => assert!(format!("{e}").contains("bot token")),
        Ok(_) => panic!("expected an error for DM mode without a token"),
    }
}
