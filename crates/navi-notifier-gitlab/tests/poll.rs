//! GitLab source poll test against a mock GitLab API.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use navi_notifier_core::model::EventKind;
use navi_notifier_core::traits::{Source, StateStore};
use navi_notifier_core::StateError;
use navi_notifier_gitlab::{GitLabSource, GitLabSourceConfig};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// GitLab's todo poll ignores state, so a no-op store suffices.
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

/// In-memory store so the note-diff path can be exercised across a seeded snapshot.
#[derive(Default)]
struct MemState {
    snapshots: Mutex<HashMap<String, Vec<u8>>>,
    cursors: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl StateStore for MemState {
    async fn get_snapshot(&self, src: &str, scope: &str) -> Result<Option<Vec<u8>>, StateError> {
        Ok(self
            .snapshots
            .lock()
            .unwrap()
            .get(&key(src, scope))
            .cloned())
    }
    async fn put_snapshot(&self, src: &str, scope: &str, bytes: &[u8]) -> Result<(), StateError> {
        self.snapshots
            .lock()
            .unwrap()
            .insert(key(src, scope), bytes.to_vec());
        Ok(())
    }
    async fn was_delivered(&self, _: &str) -> Result<bool, StateError> {
        Ok(false)
    }
    async fn mark_delivered(&self, _: &str) -> Result<(), StateError> {
        Ok(())
    }
    async fn get_cursor(&self, src: &str, k: &str) -> Result<Option<String>, StateError> {
        Ok(self.cursors.lock().unwrap().get(&key(src, k)).cloned())
    }
    async fn put_cursor(&self, src: &str, k: &str, v: &str) -> Result<(), StateError> {
        self.cursors
            .lock()
            .unwrap()
            .insert(key(src, k), v.to_string());
        Ok(())
    }
}

fn key(src: &str, k: &str) -> String {
    format!("{src}:{k}")
}

#[tokio::test]
async fn poll_maps_review_request_todo_to_event() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "username": "me" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/todos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id": 42,
            "action_name": "review_requested",
            "target_type": "MergeRequest",
            "target_url": "https://gitlab.test/group/proj/-/merge_requests/3",
            "body": "please review",
            "created_at": "2024-01-02T03:04:05Z",
            "author": { "username": "alice" },
            "project": { "path_with_namespace": "group/proj", "web_url": "https://gitlab.test/group/proj" },
            "target": {
                "iid": 3,
                "title": "Add thing",
                "web_url": "https://gitlab.test/group/proj/-/merge_requests/3",
                "state": "opened",
                "author": { "username": "bob" }
            }
        }])))
        .mount(&server)
        .await;

    let source = GitLabSource::new(GitLabSourceConfig {
        token: "test-token".into(),
        api_base: Some(server.uri()),
        comment_min_age_secs: 0,
    })
    .expect("build");

    let events = source.poll(&NoState).await.expect("poll");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventKind::ReviewRequested);
    assert_eq!(events[0].pull_request.repo.full_name(), "group/proj");
    assert_eq!(events[0].pull_request.number, 3);
    assert_eq!(events[0].source_id, "gitlab");
}

#[tokio::test]
async fn poll_diffs_an_involved_mr_for_merge_and_reply() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "username": "me" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/todos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    // All three involvement queries share this path; they return the same MR,
    // which the source dedupes by (project_id, iid).
    Mock::given(method("GET"))
        .and(path("/merge_requests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "iid": 3,
            "project_id": 5,
            "title": "Add thing",
            "web_url": "https://gitlab.test/group/proj/-/merge_requests/3",
            "state": "merged",
            "author": { "username": "me" },
            "merged_by": { "username": "boss" },
            "merged_at": "2024-02-02T00:00:00Z",
            "updated_at": "2024-02-02T00:00:00Z"
        }])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/projects/5/merge_requests/3/discussions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id": "abc",
            "notes": [
                { "id": 1, "system": false, "body": "please look", "author": { "username": "me" }, "created_at": "2024-02-01T00:00:00Z" },
                { "id": 2, "system": false, "body": "done", "author": { "username": "alice" }, "created_at": "2024-02-02T00:00:00Z" }
            ]
        }])))
        .mount(&server)
        .await;

    // Seed a baselined snapshot: MR seen while open, my root note already recorded.
    let state = MemState::default();
    let seeded = json!({
        "seen_notes": [1],
        "merged": false,
        "closed": false,
        "draft": false,
        "initialized": true
    });
    state
        .put_snapshot(
            "gitlab",
            "group/proj#3",
            &serde_json::to_vec(&seeded).unwrap(),
        )
        .await
        .unwrap();

    let source = GitLabSource::new(GitLabSourceConfig {
        token: "test-token".into(),
        api_base: Some(server.uri()),
        comment_min_age_secs: 0,
    })
    .expect("build");

    let events = source.poll(&state).await.expect("poll");
    let kinds: Vec<&EventKind> = events.iter().map(|e| &e.kind).collect();
    assert!(
        kinds.contains(&&EventKind::Merged),
        "expected a merge event, got {kinds:?}"
    );
    assert!(
        kinds.contains(&&EventKind::CommentReply {
            on_your_comment: true
        }),
        "expected a reply in your thread, got {kinds:?}"
    );
    assert!(events.iter().all(|e| e.source_id == "gitlab"));
}

#[tokio::test]
async fn committed_snapshot_stops_the_event_re_firing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "username": "me" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/todos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/merge_requests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "iid": 3, "project_id": 5, "title": "Add thing",
            "web_url": "https://gitlab.test/group/proj/-/merge_requests/3",
            "state": "merged", "author": { "username": "me" },
            "merged_at": "2024-02-02T00:00:00Z", "updated_at": "2024-02-02T00:00:00Z"
        }])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/projects/5/merge_requests/3/discussions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let state = MemState::default();
    state
        .put_snapshot(
            "gitlab",
            "group/proj#3",
            &serde_json::to_vec(&json!({ "merged": false, "initialized": true })).unwrap(),
        )
        .await
        .unwrap();

    let source = GitLabSource::new(GitLabSourceConfig {
        token: "test-token".into(),
        api_base: Some(server.uri()),
        comment_min_age_secs: 0,
    })
    .expect("build");

    // First poll derives the merge; snapshots are deferred until commit.
    let first = source.poll(&state).await.expect("first poll");
    assert!(first.iter().any(|e| e.kind == EventKind::Merged));
    // Persist the deferred snapshot (no scope failed delivery).
    source
        .commit_snapshots(&state, &HashSet::new())
        .await
        .expect("commit");
    // Second poll must not re-derive the merge now that the snapshot is committed.
    let second = source.poll(&state).await.expect("second poll");
    assert!(
        !second.iter().any(|e| e.kind == EventKind::Merged),
        "committed snapshot must stop the merge re-firing, got {:?}",
        second.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
}

#[test]
fn empty_token_is_rejected() {
    match GitLabSource::new(GitLabSourceConfig {
        token: "  ".into(),
        api_base: None,
        comment_min_age_secs: 0,
    }) {
        Err(e) => assert!(format!("{e}").contains("empty")),
        Ok(_) => panic!("expected an error for an empty token"),
    }
}
