//! End-to-end tests for the Slack notifier against a mock Slack Web API.
//!
//! Exercises the real reqwest path, the `ok:false` envelope handling, DM-channel
//! resolution (`conversations.open`), and that the posted message carries the
//! resolved channel + rendered blocks.

use navi_notifier_core::model::{
    Actor, Event, EventKind, PullRequest, Repo, ReviewState, ViewerRelationship,
};
use navi_notifier_core::traits::Notifier;
use navi_notifier_slack::{SlackNotifier, SlackNotifierConfig};
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
        viewer: ViewerRelationship {
            is_author: true,
            is_reviewer: false,
        },
        actor: Actor::new("reviewer"),
        occurred_at: OffsetDateTime::UNIX_EPOCH,
        target_url: None,
        excerpt: None,
        dedup_key: "k".into(),
    }
}

fn notifier(server: &MockServer, dm_to: &str) -> SlackNotifier {
    SlackNotifier::new(SlackNotifierConfig {
        token: "xoxb-test".into(),
        dm_to: dm_to.into(),
        api_base: Some(format!("{}/api", server.uri())),
    })
    .expect("build notifier")
}

async fn mount_ok(server: &MockServer, endpoint: &str, body: Value) {
    Mock::given(method("POST"))
        .and(path(format!("/api/{endpoint}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn dm_self_resolves_channel_and_posts() {
    let server = MockServer::start().await;
    mount_ok(
        &server,
        "auth.test",
        json!({ "ok": true, "user_id": "U1", "user": "me", "team": "T" }),
    )
    .await;
    mount_ok(
        &server,
        "conversations.open",
        json!({ "ok": true, "channel": { "id": "D1" } }),
    )
    .await;
    mount_ok(
        &server,
        "chat.postMessage",
        json!({ "ok": true, "ts": "123.456" }),
    )
    .await;

    let notifier = notifier(&server, "self");
    notifier.send(&sample_event()).await.expect("send");

    // Assert the posted message went to the resolved DM channel with a headline.
    let requests = server.received_requests().await.unwrap();
    let post = requests
        .iter()
        .find(|r| r.url.path() == "/api/chat.postMessage")
        .expect("a chat.postMessage request");
    let body: Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(body["channel"], "D1");
    let text = body["text"].as_str().unwrap();
    assert!(text.contains("approved"), "headline missing: {text}");
    assert!(body["blocks"].is_array());
}

#[tokio::test]
async fn concrete_channel_skips_conversations_open() {
    let server = MockServer::start().await;
    // No conversations.open mounted: if the code calls it, the request 404s and
    // send() fails, which is the assertion we want.
    mount_ok(
        &server,
        "chat.postMessage",
        json!({ "ok": true, "ts": "1" }),
    )
    .await;

    let notifier = notifier(&server, "C0123456789");
    notifier.send(&sample_event()).await.expect("send");

    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|r| r.url.path() != "/api/conversations.open"),
        "should not open a conversation for a concrete channel id"
    );
    let post = requests
        .iter()
        .find(|r| r.url.path() == "/api/chat.postMessage")
        .unwrap();
    let body: Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(body["channel"], "C0123456789");
}

#[tokio::test]
async fn verify_returns_identity() {
    let server = MockServer::start().await;
    mount_ok(
        &server,
        "auth.test",
        json!({ "ok": true, "user_id": "U1", "user": "lara", "team": "Higharc" }),
    )
    .await;

    let notifier = notifier(&server, "self");
    let who = notifier.verify().await.expect("verify");
    assert!(who.contains("lara"), "got {who}");
    assert!(who.contains("Higharc"), "got {who}");
}

#[tokio::test]
async fn ok_false_is_surfaced_as_error() {
    let server = MockServer::start().await;
    mount_ok(
        &server,
        "auth.test",
        json!({ "ok": false, "error": "invalid_auth" }),
    )
    .await;

    let notifier = notifier(&server, "self");
    let err = notifier.verify().await.unwrap_err();
    assert!(format!("{err}").contains("invalid_auth"), "got {err}");
}
