//! Discord notifier tests against a mock Discord API.

use navi_notifier_core::model::{
    Actor, Event, EventKind, PullRequest, Repo, ReviewState, ViewerRelationship,
};
use navi_notifier_core::traits::Notifier;
use navi_notifier_discord::{DiscordNotifier, DiscordNotifierConfig};
use serde_json::{json, Value};
use time::OffsetDateTime;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

    let notifier = DiscordNotifier::new(DiscordNotifierConfig {
        token: None,
        dm_to: format!("{}/webhooks/1/abc", server.uri()),
        api_base: None,
    })
    .expect("build");
    notifier.send(&sample_event()).await.expect("send");

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

    let notifier = DiscordNotifier::new(DiscordNotifierConfig {
        token: Some("bot-token".into()),
        dm_to: "123456789".into(),
        api_base: Some(server.uri()),
    })
    .expect("build");
    notifier.send(&sample_event()).await.expect("send");

    let reqs = server.received_requests().await.unwrap();
    assert!(reqs.iter().any(|r| r.url.path() == "/channels/D1/messages"));
}

#[test]
fn dm_mode_requires_token() {
    let result = DiscordNotifier::new(DiscordNotifierConfig {
        token: None,
        dm_to: "123456789".into(),
        api_base: None,
    });
    match result {
        Err(e) => assert!(format!("{e}").contains("bot token")),
        Ok(_) => panic!("expected an error for DM mode without a token"),
    }
}
