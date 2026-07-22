//! End-to-end tests for the GitHub source against a mock GitHub API.
//!
//! These exercise the real HTTP + deserialization + poll→diff path (what the pure
//! unit tests in `diff.rs` can't): octocrab against a wiremock server standing in
//! for api.github.com, asserting the source turns live-shaped payloads into the
//! right normalized events.

mod common;

use std::collections::HashSet;

use common::MemState;
use navi_notifier_core::model::EventKind;
use navi_notifier_core::traits::{Source, StateStore};
use navi_notifier_github::{GitHubSource, GitHubSourceConfig};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
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
        mark_read: false,
        comment_min_age_secs: 0,
        backfill: Default::default(),
    })
    .expect("build source")
}

fn source_marks_read(server: &MockServer) -> GitHubSource {
    GitHubSource::new(GitHubSourceConfig {
        token: "test-token".into(),
        api_base: Some(server.uri()),
        track_prs: false,
        mark_read: true,
        comment_min_age_secs: 0,
        backfill: Default::default(),
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
async fn mark_read_marks_the_thread_after_commit() {
    let server = MockServer::start().await;
    // A numeric thread id, since commit() marks the thread by parsed id.
    let notif = json!([{
        "id": "123456",
        "reason": "review_requested",
        "updated_at": "2024-01-02T03:04:05Z",
        "subject": {
            "title": "Add gizmo",
            "url": format!("{}/repos/acme/widgets/pulls/1", server.uri()),
            "type": "PullRequest"
        },
        "repository": { "name": "widgets", "owner": { "login": "acme" }, "html_url": "https://github.com/acme/widgets" }
    }]);
    mock_github(&server, notif, open_pr(json!([{ "login": "me" }]))).await;
    Mock::given(method("PATCH"))
        .and(path("/notifications/threads/123456"))
        .respond_with(ResponseTemplate::new(205))
        .expect(1)
        .mount(&server)
        .await;

    let source = source_marks_read(&server);
    let state = MemState::default();
    let events = source.poll(&state).await.expect("poll");
    assert_eq!(events.len(), 1);
    // The engine calls commit() after delivery; here we call it directly.
    source.commit(&state, &events[0]).await.expect("commit");
    // wiremock verifies the PATCH fired (.expect(1)) when the server drops.
}

#[tokio::test]
async fn mark_read_patches_once_across_repeated_commits() {
    let server = MockServer::start().await;
    let notif = json!([{
        "id": "123456",
        "reason": "review_requested",
        "updated_at": "2024-01-02T03:04:05Z",
        "subject": {
            "title": "Add gizmo",
            "url": format!("{}/repos/acme/widgets/pulls/1", server.uri()),
            "type": "PullRequest"
        },
        "repository": { "name": "widgets", "owner": { "login": "acme" }, "html_url": "https://github.com/acme/widgets" }
    }]);
    mock_github(&server, notif, open_pr(json!([{ "login": "me" }]))).await;
    // Exactly one PATCH must fire even if commit() runs twice for the same PR.
    Mock::given(method("PATCH"))
        .and(path("/notifications/threads/123456"))
        .respond_with(ResponseTemplate::new(205))
        .expect(1)
        .mount(&server)
        .await;

    let source = source_marks_read(&server);
    let state = MemState::default();
    let events = source.poll(&state).await.expect("poll");
    assert_eq!(events.len(), 1);
    source.commit(&state, &events[0]).await.expect("commit");
    source
        .commit(&state, &events[0])
        .await
        .expect("second commit is a no-op");
}

#[tokio::test]
async fn snapshot_is_deferred_until_commit() {
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
    assert_eq!(events.len(), 1);

    // poll() must not have persisted the snapshot yet - that's the exactly-once fix.
    assert!(
        state
            .get_snapshot("github", "acme/widgets#1")
            .await
            .unwrap()
            .is_none(),
        "snapshot should be deferred until commit_snapshots"
    );

    // Flushing with no failed scopes persists it.
    source
        .commit_snapshots(&state, &HashSet::new())
        .await
        .expect("commit");
    assert!(state
        .get_snapshot("github", "acme/widgets#1")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn failed_scope_snapshot_is_held_back() {
    let server = MockServer::start().await;
    mock_github(
        &server,
        pr_notification(&server.uri()),
        open_pr(json!([{ "login": "me" }])),
    )
    .await;

    let source = source_for(&server);
    let state = MemState::default();
    source.poll(&state).await.expect("poll");

    // The PR's delivery failed this pass, so its snapshot must not advance -
    // leaving the old state so the event re-derives (and dedup covers re-sends).
    let failed = HashSet::from(["acme/widgets#1".to_string()]);
    source
        .commit_snapshots(&state, &failed)
        .await
        .expect("commit");
    assert!(
        state
            .get_snapshot("github", "acme/widgets#1")
            .await
            .unwrap()
            .is_none(),
        "a failed scope's snapshot must be held back"
    );
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
async fn self_merged_pr_is_caught_by_the_closed_sweep() {
    // #86: you merge your own PR. GitHub doesn't notify you, and the merged PR has
    // left the `is:open` sweep, so only the recently-closed sweep can catch it.
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
    // Open sweep finds nothing: the PR already merged and left `is:open`.
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .and(query_param("q", "is:open is:pr involves:me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [] })))
        .mount(&server)
        .await;
    // Closed sweep (bounded by the seeded cursor) finds the just-merged PR.
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .and(query_param(
            "q",
            "is:closed is:pr involves:me updated:>=2024-01-01T00:00:00Z",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "repository_url": format!("{}/repos/acme/widgets", server.uri()),
                "number": 1,
                "updated_at": "2024-02-02T00:00:00Z"
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 1,
            "title": "Add gizmo",
            "html_url": "https://github.com/acme/widgets/pull/1",
            "state": "closed",
            "draft": false,
            "merged": true,
            "merged_at": "2024-02-02T00:00:00Z",
            "updated_at": "2024-02-02T00:00:00Z",
            "user": { "login": "me" },
            "requested_reviewers": []
        })))
        .mount(&server)
        .await;
    for sub in [
        "/repos/acme/widgets/pulls/1/reviews",
        "/repos/acme/widgets/pulls/1/comments",
        "/repos/acme/widgets/issues/1/comments",
    ] {
        Mock::given(method("GET"))
            .and(path(sub))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
    }

    let state = MemState::default();
    // navi had baselined the PR while open, and the closed-sweep cursor is set (i.e.
    // this isn't navi's first poll).
    state
        .put_snapshot("github", "acme/widgets#1", br#"{"initialized":true}"#)
        .await
        .unwrap();
    state
        .put_cursor("github", "pr_closed_since", "2024-01-01T00:00:00Z")
        .await
        .unwrap();

    let source = source_with(&server, true);
    let events = source.poll(&state).await.expect("poll");
    let merges = events
        .iter()
        .filter(|e| e.kind == EventKind::Merged)
        .count();
    assert_eq!(
        merges,
        1,
        "the self-merge must fire exactly one Merged, got {:?}",
        events.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
    assert_eq!(events[0].pull_request.number, 1);
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

/// Mount a GraphQL response reporting whether the base uses a queue and the PR's
/// current entry state (or null).
async fn mock_merge_queue(server: &MockServer, enabled: bool, state: Option<&str>) {
    let entry = match state {
        Some(s) => json!({ "state": s }),
        None => json!(null),
    };
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "repository": { "pullRequest": {
                "isMergeQueueEnabled": enabled,
                "mergeQueueEntry": entry
            } } }
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn merge_queue_entry_emits_entered_event() {
    let server = MockServer::start().await;
    mock_github(
        &server,
        pr_notification(&server.uri()),
        open_pr(json!([{ "login": "me" }])),
    )
    .await;
    mock_merge_queue(&server, true, Some("QUEUED")).await;

    // Seed a prior "not queued" state so this poll sees a transition, not first sight.
    let state = MemState::default();
    state
        .put_cursor("github", "mq:acme/widgets#1", "absent")
        .await
        .unwrap();

    let events = source_for(&server).poll(&state).await.expect("poll");
    let kinds: Vec<&EventKind> = events.iter().map(|e| &e.kind).collect();
    assert!(
        kinds.contains(&&EventKind::EnteredMergeQueue),
        "expected an entered-merge-queue event, got {kinds:?}"
    );
}

#[tokio::test]
async fn merge_queue_state_is_deferred_until_commit() {
    let server = MockServer::start().await;
    mock_github(
        &server,
        pr_notification(&server.uri()),
        open_pr(json!([{ "login": "me" }])),
    )
    .await;
    mock_merge_queue(&server, true, Some("QUEUED")).await;

    let state = MemState::default();
    state
        .put_cursor("github", "mq:acme/widgets#1", "absent")
        .await
        .unwrap();

    // One source instance: it stashes the deferred state during poll and flushes it
    // on commit.
    let src = source_for(&server);
    src.poll(&state).await.expect("poll");
    // Poll alone must NOT advance the queue cursor, so a delivery failure re-derives
    // the transition next poll.
    assert_eq!(
        cursor(&state, "mq:acme/widgets#1").await.as_deref(),
        Some("absent"),
        "queue cursor must not advance before delivery"
    );

    // Commit with no failed scopes advances it to the observed state.
    src.commit_snapshots(&state, &HashSet::new()).await.unwrap();
    assert_eq!(
        cursor(&state, "mq:acme/widgets#1").await.as_deref(),
        Some("QUEUED"),
        "queue cursor advances once its scope's delivery is committed"
    );
}

async fn cursor(state: &MemState, key: &str) -> Option<String> {
    state.get_cursor("github", key).await.unwrap()
}

#[tokio::test]
async fn merge_queue_baselines_silently_on_first_sight() {
    let server = MockServer::start().await;
    mock_github(
        &server,
        pr_notification(&server.uri()),
        open_pr(json!([{ "login": "me" }])),
    )
    .await;
    mock_merge_queue(&server, true, Some("QUEUED")).await;

    // No seeded cursor: the first sight of the queue state must not fire an event.
    let events = source_for(&server)
        .poll(&MemState::default())
        .await
        .expect("poll");
    let kinds: Vec<&EventKind> = events.iter().map(|e| &e.kind).collect();
    assert!(
        !kinds.contains(&&EventKind::EnteredMergeQueue),
        "first sight must baseline, got {kinds:?}"
    );
}

#[tokio::test]
async fn no_merge_queue_repo_emits_nothing_and_caches_the_verdict() {
    let server = MockServer::start().await;
    mock_github(
        &server,
        pr_notification(&server.uri()),
        open_pr(json!([{ "login": "me" }])),
    )
    .await;
    // The repo has no merge queue; even a seeded prior state must not fire.
    mock_merge_queue(&server, false, None).await;

    let state = MemState::default();
    state
        .put_cursor("github", "mq:acme/widgets#1", "absent")
        .await
        .unwrap();

    let events = source_for(&server).poll(&state).await.expect("poll");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.kind, EventKind::EnteredMergeQueue)),
        "a repo without a merge queue must not produce queue events"
    );
    // The "no queue" verdict is cached so later polls can skip the GraphQL call.
    let cached = state
        .get_cursor("github", "mqcfg:acme/widgets")
        .await
        .unwrap()
        .expect("a cached verdict");
    assert!(cached.starts_with("no|"), "got {cached}");
}

#[tokio::test]
async fn merge_on_a_queued_pr_does_not_double_report_removal() {
    let server = MockServer::start().await;
    // The PR is now merged and no longer in the queue (entry gone).
    let merged_pr = json!({
        "number": 1,
        "title": "Add gizmo",
        "html_url": "https://github.com/acme/widgets/pull/1",
        "state": "closed",
        "draft": false,
        "merged": true,
        "merged_at": "2024-01-02T03:04:06Z",
        "updated_at": "2024-01-02T03:04:06Z",
        "user": { "login": "me" },
        "requested_reviewers": []
    });
    mock_github(&server, pr_notification(&server.uri()), merged_pr).await;
    mock_merge_queue(&server, true, None).await;

    // Prior state: the PR was queued. Now it's gone because it merged.
    let state = MemState::default();
    state
        .put_cursor("github", "mq:acme/widgets#1", "QUEUED")
        .await
        .unwrap();

    let events = source_for(&server).poll(&state).await.expect("poll");
    let kinds: Vec<&EventKind> = events.iter().map(|e| &e.kind).collect();
    assert!(
        kinds.contains(&&EventKind::Merged),
        "expected the merge event, got {kinds:?}"
    );
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, EventKind::RemovedFromMergeQueue { .. })),
        "a merge must not also report removal from the queue, got {kinds:?}"
    );
}
