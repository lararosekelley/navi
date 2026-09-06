//! Gitea source poll test against a mock Gitea API.

mod common;

use std::collections::HashSet;

use common::MemState;
use navi_notifier_core::model::EventKind;
use navi_notifier_core::traits::{Source, StateStore};
use navi_notifier_gitea::{GiteaSource, GiteaSourceConfig};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
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
        track_prs: false,
        backfill: Default::default(),
    })
    .expect("build");

    let events = source.poll(&MemState::default()).await.expect("poll");
    assert_eq!(events.len(), 1, "unexpected: {events:?}");
    assert_eq!(events[0].kind, EventKind::ReviewRequested);
    assert_eq!(events[0].pull_request.repo.full_name(), "acme/widgets");
    assert_eq!(events[0].pull_request.number, 3);
    assert_eq!(events[0].source_id, "gitea");
}

#[tokio::test]
async fn self_merged_pr_caught_by_the_closed_sweep() {
    // #92: you merge your own Gitea PR. It doesn't notify you and has left the open
    // sweep, so only the recently-closed sweep catches it.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "me" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/notifications"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    // Open sweep: nothing (the PR already merged).
    Mock::given(method("GET"))
        .and(path("/repos/issues/search"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    // Closed sweep: the just-merged PR.
    Mock::given(method("GET"))
        .and(path("/repos/issues/search"))
        .and(query_param("state", "closed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "number": 3,
            "updated_at": "2024-02-02T00:00:00Z",
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
            "state": "closed",
            "draft": false,
            "merged": true,
            "merged_at": "2024-02-02T00:00:00Z",
            "user": { "login": "me" },
            "requested_reviewers": []
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

    let state = MemState::default();
    state
        .put_snapshot("gitea", "acme/widgets#3", br#"{"initialized":true}"#)
        .await
        .unwrap();
    state
        .put_cursor("gitea", "pr_closed_since", "2024-01-01T00:00:00Z")
        .await
        .unwrap();

    let source = GiteaSource::new(GiteaSourceConfig {
        token: "test-token".into(),
        api_base: Some(server.uri()),
        comment_min_age_secs: 0,
        track_prs: true,
        backfill: Default::default(),
    })
    .expect("build");

    let events = source.poll(&state).await.expect("poll");
    let merge = events
        .iter()
        .find(|e| e.kind == EventKind::Merged)
        .expect("self-merge must be caught by the closed sweep");
    // The repo url from the search result flows through, so notifications can link.
    assert_eq!(
        merge.pull_request.repo.url.as_deref(),
        Some("https://gitea.test/acme/widgets")
    );

    // #98: the `pr:` cursor is deferred until commit, so a failed delivery re-derives
    // the merge next poll instead of skipping it.
    assert_eq!(
        state
            .get_cursor("gitea", "pr:acme/widgets#3")
            .await
            .unwrap(),
        None,
        "sweep cursor must be deferred until delivery"
    );
    source
        .commit_snapshots(&state, &HashSet::new())
        .await
        .unwrap();
    assert_eq!(
        state
            .get_cursor("gitea", "pr:acme/widgets#3")
            .await
            .unwrap()
            .as_deref(),
        Some("2024-02-02T00:00:00Z"),
        "commit advances the cursor once delivery is confirmed"
    );
}

/// Mount an empty inbox plus an open-sweep hit for `acme/widgets#4`, with the PR
/// fetch answering `status`.
async fn mock_sweep_hit_with_failing_pr(server: &MockServer, status: u16) {
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "me" })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/notifications"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/issues/search"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "number": 4,
            "updated_at": "2024-02-02T00:00:00Z",
            "repository": { "full_name": "acme/widgets", "html_url": "https://gitea.test/acme/widgets" }
        }])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/issues/search"))
        .and(query_param("state", "closed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/4"))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

fn sweeping_source(server: &MockServer) -> GiteaSource {
    GiteaSource::new(GiteaSourceConfig {
        token: "test-token".into(),
        api_base: Some(server.uri()),
        comment_min_age_secs: 0,
        track_prs: true,
        backfill: Default::default(),
    })
    .expect("build")
}

/// #162: a PR that couldn't be fetched was never compared against its snapshot, so
/// recording it as seen hides it until its timestamp moves again.
#[tokio::test]
async fn a_transient_fetch_failure_does_not_advance_the_pr_cursor() {
    let server = MockServer::start().await;
    mock_sweep_hit_with_failing_pr(&server, 500).await;

    let state = MemState::default();
    let source = sweeping_source(&server);
    assert!(source.poll(&state).await.expect("poll").is_empty());
    source
        .commit_snapshots(&state, &HashSet::new())
        .await
        .unwrap();

    assert_eq!(
        state
            .get_cursor("gitea", "pr:acme/widgets#4")
            .await
            .unwrap(),
        None,
        "a PR that was never fetched must be retried, not skipped until it changes"
    );
}

/// And the other half: a gone PR is skipped for good rather than re-fetched for
/// ever. 404 and 410 (the deleted-resource response) are both permanent.
async fn assert_gone_status_advances(status: u16) {
    let server = MockServer::start().await;
    mock_sweep_hit_with_failing_pr(&server, status).await;

    let state = MemState::default();
    let source = sweeping_source(&server);
    assert!(source.poll(&state).await.expect("poll").is_empty());
    source
        .commit_snapshots(&state, &HashSet::new())
        .await
        .unwrap();

    assert_eq!(
        state
            .get_cursor("gitea", "pr:acme/widgets#4")
            .await
            .unwrap()
            .as_deref(),
        Some("2024-02-02T00:00:00Z"),
        "a deleted or invisible PR is skipped for good"
    );
}

#[tokio::test]
async fn a_not_found_pr_does_advance_the_cursor() {
    assert_gone_status_advances(404).await;
}

#[tokio::test]
async fn a_deleted_pr_does_advance_the_cursor() {
    assert_gone_status_advances(410).await;
}
