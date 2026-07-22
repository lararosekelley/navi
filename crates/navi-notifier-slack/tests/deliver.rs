//! End-to-end tests for the Slack destination against a mock Slack Web API.
//!
//! Exercises the real reqwest path, the `ok:false` envelope handling, DM-channel
//! resolution (`conversations.open`), and that the posted message carries the
//! resolved channel + rendered blocks.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use navi_notifier_core::model::{
    Actor, Event, EventKind, PullRequest, Repo, ReviewState, ViewerRelationship,
};
use navi_notifier_core::traits::{Destination, StateStore};
use navi_notifier_core::StateError;
use navi_notifier_slack::{SlackDestination, SlackDestinationConfig};
use serde_json::{json, Value};
use time::OffsetDateTime;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// In-memory store so the destination can persist/read a PR's Slack thread ts.
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
        viewer: ViewerRelationship {
            is_author: true,
            is_reviewer: false,
            actor_is_viewer: false,
        },
        actor: Actor::new("reviewer"),
        occurred_at: OffsetDateTime::UNIX_EPOCH,
        target_url: None,
        excerpt: None,
        dedup_key: "k".into(),
    }
}

fn destination(server: &MockServer, dm_to: &str) -> SlackDestination {
    SlackDestination::new(SlackDestinationConfig {
        token: "xoxb-test".into(),
        dm_to: dm_to.into(),
        api_base: Some(format!("{}/api", server.uri())),
        broadcast: vec!["merged".into(), "closed".into(), "review_dismissed".into()],
    })
    .expect("build destination")
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

    let destination = destination(&server, "self");
    destination
        .send(&sample_event(), &MemState::default())
        .await
        .expect("send");

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
async fn second_event_on_a_pr_replies_in_the_first_ones_thread() {
    let server = MockServer::start().await;
    mount_ok(
        &server,
        "chat.postMessage",
        json!({ "ok": true, "ts": "111.222" }),
    )
    .await;

    // One store shared across both sends, so the first message's ts is remembered.
    let state = MemState::default();
    let destination = destination(&server, "C0123456789");
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
        .filter(|r| r.url.path() == "/api/chat.postMessage")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(posts.len(), 2);
    // The first message opens the thread (no thread_ts); the second replies under it.
    assert!(
        posts[0].get("thread_ts").is_none(),
        "parent must not be threaded"
    );
    assert_eq!(
        posts[1]["thread_ts"], "111.222",
        "reply must anchor to the parent"
    );
}

#[tokio::test]
async fn terminal_events_broadcast_out_of_the_thread() {
    let server = MockServer::start().await;
    mount_ok(
        &server,
        "chat.postMessage",
        json!({ "ok": true, "ts": "111.222" }),
    )
    .await;

    let state = MemState::default();
    let dest = destination(&server, "C0123456789");
    // Parent (a review submission) opens the thread.
    dest.send(&sample_event(), &state).await.expect("parent");
    // A merge on the same PR replies AND broadcasts (it's in the default set).
    let mut merged = sample_event();
    merged.kind = EventKind::Merged;
    dest.send(&merged, &state).await.expect("merge");
    // A non-terminal event on the same PR replies but does not broadcast.
    dest.send(&sample_event(), &state).await.expect("reply");

    let posts: Vec<Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api/chat.postMessage")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(posts.len(), 3);
    assert!(
        posts[0].get("thread_ts").is_none(),
        "parent posts top-level"
    );
    // Merge: threaded and broadcast to the channel.
    assert_eq!(posts[1]["thread_ts"], "111.222");
    assert_eq!(posts[1]["reply_broadcast"], true);
    // Non-terminal reply: threaded, not broadcast.
    assert_eq!(posts[2]["thread_ts"], "111.222");
    assert!(
        posts[2].get("reply_broadcast").is_none(),
        "a non-terminal reply must stay thread-only"
    );
}

#[tokio::test]
async fn review_broadcast_is_per_state() {
    let server = MockServer::start().await;
    mount_ok(
        &server,
        "chat.postMessage",
        json!({ "ok": true, "ts": "111.222" }),
    )
    .await;

    // Broadcast approvals and change requests, but not plain review comments.
    let dest = SlackDestination::new(SlackDestinationConfig {
        token: "xoxb-test".into(),
        dm_to: "C0123456789".into(),
        api_base: Some(format!("{}/api", server.uri())),
        broadcast: vec!["review_approved".into(), "review_changes_requested".into()],
    })
    .expect("build destination");

    let state = MemState::default();
    // A review request opens the thread (distinct from the review replies below).
    let mut opener = sample_event();
    opener.kind = EventKind::ReviewRequested;
    dest.send(&opener, &state).await.expect("parent");
    // Approved and changes-requested replies each broadcast via their per-state tag.
    let mut approved = sample_event();
    approved.kind = EventKind::ReviewSubmitted {
        state: ReviewState::Approved,
    };
    dest.send(&approved, &state).await.expect("approved");
    let mut changes = sample_event();
    changes.kind = EventKind::ReviewSubmitted {
        state: ReviewState::ChangesRequested,
    };
    dest.send(&changes, &state)
        .await
        .expect("changes requested");
    // A plain review comment reply does not (review_commented is not in the set).
    let mut commented = sample_event();
    commented.kind = EventKind::ReviewSubmitted {
        state: ReviewState::Commented,
    };
    dest.send(&commented, &state).await.expect("commented");

    let posts: Vec<Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api/chat.postMessage")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(posts.len(), 4);
    assert_eq!(
        posts[1]["reply_broadcast"], true,
        "approval should broadcast"
    );
    assert_eq!(
        posts[2]["reply_broadcast"], true,
        "changes-requested should broadcast"
    );
    assert!(
        posts[3].get("reply_broadcast").is_none(),
        "a plain review comment must stay thread-only"
    );
}

#[tokio::test]
async fn review_submitted_umbrella_broadcasts_all_states() {
    // Backward compat: the pre-per-state config value `review_submitted` must still
    // broadcast every review state (approved, changes-requested, and commented).
    let server = MockServer::start().await;
    mount_ok(
        &server,
        "chat.postMessage",
        json!({ "ok": true, "ts": "111.222" }),
    )
    .await;

    let dest = SlackDestination::new(SlackDestinationConfig {
        token: "xoxb-test".into(),
        dm_to: "C0123456789".into(),
        api_base: Some(format!("{}/api", server.uri())),
        broadcast: vec!["review_submitted".into()],
    })
    .expect("build destination");

    let state = MemState::default();
    // A review request opens the thread; each review-state reply should broadcast.
    let mut opener = sample_event();
    opener.kind = EventKind::ReviewRequested;
    dest.send(&opener, &state).await.expect("parent");
    for st in [
        ReviewState::Approved,
        ReviewState::ChangesRequested,
        ReviewState::Commented,
    ] {
        let mut e = sample_event();
        e.kind = EventKind::ReviewSubmitted { state: st };
        dest.send(&e, &state).await.expect("review reply");
    }

    let posts: Vec<Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api/chat.postMessage")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(posts.len(), 4);
    for reply in &posts[1..] {
        assert_eq!(
            reply["reply_broadcast"], true,
            "review_submitted must broadcast every state"
        );
    }
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

    let destination = destination(&server, "C0123456789");
    destination
        .send(&sample_event(), &MemState::default())
        .await
        .expect("send");

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

    let destination = destination(&server, "self");
    let who = destination.verify().await.expect("verify");
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

    let destination = destination(&server, "self");
    let err = destination.verify().await.unwrap_err();
    assert!(format!("{err}").contains("invalid_auth"), "got {err}");
}
