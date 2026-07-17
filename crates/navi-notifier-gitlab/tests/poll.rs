//! GitLab source poll test against a mock GitLab API.

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
    })
    .expect("build");

    let events = source.poll(&NoState).await.expect("poll");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventKind::ReviewRequested);
    assert_eq!(events[0].pull_request.repo.full_name(), "group/proj");
    assert_eq!(events[0].pull_request.number, 3);
    assert_eq!(events[0].source_id, "gitlab");
}

#[test]
fn empty_token_is_rejected() {
    match GitLabSource::new(GitLabSourceConfig {
        token: "  ".into(),
        api_base: None,
    }) {
        Err(e) => assert!(format!("{e}").contains("empty")),
        Ok(_) => panic!("expected an error for an empty token"),
    }
}
