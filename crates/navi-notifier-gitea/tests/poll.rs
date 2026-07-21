//! Gitea source poll test against a mock Gitea API.

mod common;

use common::MemState;
use navi_notifier_core::model::EventKind;
use navi_notifier_core::traits::Source;
use navi_notifier_gitea::{GiteaSource, GiteaSourceConfig};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn poll_emits_outstanding_review_request() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "me" })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/notifications"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "updated_at": "2024-01-02T03:04:05Z",
            "subject": {
                "title": "Add gizmo",
                "url": format!("{}/repos/acme/widgets/issues/3", server.uri()),
                "type": "Pull"
            },
            "repository": { "full_name": "acme/widgets", "html_url": "https://gitea.test/acme/widgets" }
        }])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 3,
            "title": "Add gizmo",
            "html_url": "https://gitea.test/acme/widgets/pulls/3",
            "state": "open",
            "draft": false,
            "merged": false,
            "user": { "login": "octo" },
            "requested_reviewers": [{ "login": "me" }]
        })))
        .mount(&server)
        .await;

    for sub in [
        "/repos/acme/widgets/pulls/3/reviews",
        "/repos/acme/widgets/issues/3/comments",
    ] {
        Mock::given(method("GET"))
            .and(path(sub))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
    }

    let source = GiteaSource::new(GiteaSourceConfig {
        token: "test-token".into(),
        api_base: Some(server.uri()),
        comment_min_age_secs: 0,
    })
    .expect("build");

    let events = source.poll(&MemState::default()).await.expect("poll");
    assert_eq!(events.len(), 1, "unexpected: {events:?}");
    assert_eq!(events[0].kind, EventKind::ReviewRequested);
    assert_eq!(events[0].pull_request.repo.full_name(), "acme/widgets");
    assert_eq!(events[0].pull_request.number, 3);
    assert_eq!(events[0].source_id, "gitea");
}
