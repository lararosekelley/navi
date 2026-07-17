//! End-to-end tests for the GitHub source against a mock GitHub API.
//!
//! These exercise the real HTTP + deserialization + poll→diff path (what the pure
//! unit tests in `diff.rs` can't): octocrab against a wiremock server standing in
//! for api.github.com, asserting the source turns live-shaped payloads into the
//! right normalized events.

mod common;

use common::MemState;
use navi_notifier_core::model::EventKind;
use navi_notifier_core::traits::{Source, StateStore};
use navi_notifier_github::{GitHubSource, GitHubSourceConfig};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Stand up a mock GitHub API with the given notifications payload and empty
/// review/comment lists for PR #1 of acme/widgets, plus the given PR object.
async fn mock_github(server: &MockServer, notifications: serde_json::Value, pr: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "me" })))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/notifications"))
        .respond_with(ResponseTemplate::new(200).set_body_json(notifications))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr))
        .mount(server)
        .await;

    for sub in [
        "/repos/acme/widgets/pulls/1/reviews",
        "/repos/acme/widgets/pulls/1/comments",
        "/repos/acme/widgets/issues/1/comments",
    ] {
        Mock::given(method("GET"))
            .and(path(sub))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(server)
            .await;
    }
}

fn source_for(server: &MockServer) -> GitHubSource {
    source_with(server, false)
}

fn source_with(server: &MockServer, track_prs: bool) -> GitHubSource {
    GitHubSource::new(GitHubSourceConfig {
        token: "test-token".into(),
        api_base: Some(server.uri()),
        track_prs,
    })
    .expect("build source")
}

fn pr_notification(server_uri: &str) -> serde_json::Value {
    json!([{
        "id": "thread-1",
        "reason": "review_requested",
        "updated_at": "2024-01-02T03:04:05Z",
        "subject": {
            "title": "Add gizmo",
            "url": format!("{server_uri}/repos/acme/widgets/pulls/1"),
            "type": "PullRequest"
        },
        "repository": {
            "name": "widgets",
            "owner": { "login": "acme" },
            "html_url": "https://github.com/acme/widgets"
        }
    }])
}

fn open_pr(requested_reviewers: serde_json::Value) -> serde_json::Value {
    json!({
        "number": 1,
        "title": "Add gizmo",
        "html_url": "https://github.com/acme/widgets/pull/1",
        "state": "open",
        "draft": false,
        "merged": false,
        "updated_at": "2024-01-02T03:04:05Z",
        "user": { "login": "octo" },
        "requested_reviewers": requested_reviewers
    })
}

#[tokio::test]
async fn poll_emits_outstanding_review_request() {
    let server = MockServer::start().await;
    mock_github(
        &server,
        pr_notification(&server.uri()),
        open_pr(json!([{ "login": "me" }])),
    )
    .await;

    let source = source_for(&server);
    let state = MemState::default();
    let events = source.poll(&state).await.expect("poll");

    assert_eq!(events.len(), 1, "expected one event, got {events:?}");
    assert_eq!(events[0].kind, EventKind::ReviewRequested);
    assert_eq!(events[0].pull_request.number, 1);
    assert_eq!(events[0].pull_request.repo.full_name(), "acme/widgets");
    assert!(events[0].viewer.is_reviewer);
}

#[tokio::test]
async fn poll_backfills_nothing_when_not_involved() {
    let server = MockServer::start().await;
    // Viewer is not a requested reviewer and there's no other activity.
    mock_github(
        &server,
        pr_notification(&server.uri()),
        open_pr(json!([{ "login": "someone-else" }])),
    )
    .await;

    let source = source_for(&server);
    let events = source.poll(&MemState::default()).await.expect("poll");
    assert!(events.is_empty(), "expected no events, got {events:?}");
}

#[tokio::test]
async fn poll_ignores_non_pull_request_notifications() {
    let server = MockServer::start().await;
    let notifications = json!([{
        "id": "thread-9",
        "reason": "mention",
        "updated_at": "2024-01-02T03:04:05Z",
        "subject": {
            "title": "a plain issue",
            "url": format!("{}/repos/acme/widgets/issues/7", server.uri()),
            "type": "Issue"
        },
        "repository": {
            "name": "widgets",
            "owner": { "login": "acme" },
            "html_url": "https://github.com/acme/widgets"
        }
    }]);
    // Only /user and /notifications are needed; no PR fetch should occur.
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "me" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/notifications"))
        .respond_with(ResponseTemplate::new(200).set_body_json(notifications))
        .mount(&server)
        .await;

    let source = source_for(&server);
    let events = source.poll(&MemState::default()).await.expect("poll");
    assert!(events.is_empty());
}

#[tokio::test]
async fn poll_finds_involved_pr_via_search_not_in_notifications() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "me" })))
        .mount(&server)
        .await;
    // Empty inbox: PR #2 is reachable ONLY via the involved-PR search - the #38
    // case (muted repo / author activity GitHub never notifies about).
    Mock::given(method("GET"))
        .and(path("/notifications"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "repository_url": format!("{}/repos/acme/widgets", server.uri()),
                "number": 2,
                "updated_at": "2024-01-02T03:04:05Z"
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 2,
            "title": "Add gadget",
            "html_url": "https://github.com/acme/widgets/pull/2",
            "state": "open",
            "draft": false,
            "merged": false,
            "updated_at": "2024-01-02T03:04:05Z",
            "user": { "login": "octo" },
            "requested_reviewers": [{ "login": "me" }]
        })))
        .mount(&server)
        .await;
    for sub in [
        "/repos/acme/widgets/pulls/2/reviews",
        "/repos/acme/widgets/pulls/2/comments",
        "/repos/acme/widgets/issues/2/comments",
    ] {
        Mock::given(method("GET"))
            .and(path(sub))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
    }

    let source = source_with(&server, true);
    let events = source.poll(&MemState::default()).await.expect("poll");

    assert_eq!(
        events.len(),
        1,
        "search path should surface the involved PR, got {events:?}"
    );
    assert_eq!(events[0].kind, EventKind::ReviewRequested);
    assert_eq!(events[0].pull_request.number, 2);
    assert_eq!(events[0].pull_request.repo.full_name(), "acme/widgets");
}

#[tokio::test]
async fn poll_dedupes_a_pr_seen_in_both_notifications_and_search() {
    let server = MockServer::start().await;
    mock_github(
        &server,
        pr_notification(&server.uri()),
        open_pr(json!([{ "login": "me" }])),
    )
    .await;
    // The same PR #1 also comes back from the involved-PR search this poll.
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "repository_url": format!("{}/repos/acme/widgets", server.uri()),
                "number": 1,
                "updated_at": "2024-01-02T03:04:05Z"
            }]
        })))
        .mount(&server)
        .await;

    let source = source_with(&server, true);
    let events = source.poll(&MemState::default()).await.expect("poll");

    assert_eq!(
        events.len(),
        1,
        "a PR in both paths must fire once, got {events:?}"
    );
    assert_eq!(events[0].kind, EventKind::ReviewRequested);
}

#[tokio::test]
async fn involved_path_catches_a_pr_whose_notification_is_stale() {
    let server = MockServer::start().await;
    mock_github(
        &server,
        pr_notification(&server.uri()),
        open_pr(json!([{ "login": "me" }])),
    )
    .await;
    // The same PR #1, but the search reports a NEWER updated_at than the
    // notification thread ever reflected - the exact gap #38 exists to close.
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "repository_url": format!("{}/repos/acme/widgets", server.uri()),
                "number": 1,
                "updated_at": "2024-06-01T00:00:00Z"
            }]
        })))
        .mount(&server)
        .await;

    let state = MemState::default();
    // Seed the thread cursor so the notification early-skip fires (thread unchanged).
    state
        .put_cursor("github", "thread:acme/widgets#1", "2024-01-02T03:04:05Z")
        .await
        .unwrap();

    let source = source_with(&server, true);
    let events = source.poll(&state).await.expect("poll");

    assert_eq!(
        events.len(),
        1,
        "involved path must still catch a PR whose notification was stale, got {events:?}"
    );
    assert_eq!(events[0].kind, EventKind::ReviewRequested);
}
