// Included into lib.rs's `tests` module. Unit tests for the pure todo->event map.

use super::api::Todo;
use super::*;
use navi_notifier_core::model::EventKind;
use serde_json::json;

fn todo(action: &str, target_type: &str) -> Todo {
    serde_json::from_value(json!({
        "id": 7,
        "action_name": action,
        "target_type": target_type,
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
    }))
    .unwrap()
}

fn map(action: &str, viewer: &str) -> Option<Event> {
    todo_to_event(&todo(action, "MergeRequest"), viewer, OffsetDateTime::UNIX_EPOCH)
}

#[test]
fn review_requested_maps_to_review_requested() {
    let e = map("review_requested", "me").unwrap();
    assert_eq!(e.kind, EventKind::ReviewRequested);
    assert_eq!(e.pull_request.repo.full_name(), "group/proj");
    assert_eq!(e.pull_request.number, 3);
    assert_eq!(e.actor.login, "alice");
    assert!(e.viewer.is_reviewer);
    assert!(!e.viewer.is_author);
    assert!(e.dedup_key.contains("todo:7"));
}

#[test]
fn approval_and_assigned_are_review_requests() {
    assert_eq!(map("approval_required", "me").unwrap().kind, EventKind::ReviewRequested);
    assert_eq!(map("assigned", "me").unwrap().kind, EventKind::ReviewRequested);
}

#[test]
fn mentioned_maps_to_mentioned() {
    assert_eq!(map("mentioned", "me").unwrap().kind, EventKind::Mentioned);
    assert_eq!(map("directly_addressed", "me").unwrap().kind, EventKind::Mentioned);
}

#[test]
fn issue_todos_are_ignored() {
    assert!(todo_to_event(&todo("mentioned", "Issue"), "me", OffsetDateTime::UNIX_EPOCH).is_none());
}

#[test]
fn unhandled_actions_are_ignored() {
    assert!(map("build_failed", "me").is_none());
    assert!(map("unmergeable", "me").is_none());
}

#[test]
fn viewer_authoring_the_mr_is_detected() {
    assert!(map("review_requested", "bob").unwrap().viewer.is_author);
}

#[test]
fn actor_being_the_viewer_is_flagged() {
    // The todo's author (the actor) is "alice"; when that's the viewer, flag it so
    // the render can say "you" instead of the login.
    assert!(map("mentioned", "alice").unwrap().viewer.actor_is_viewer);
    // A different viewer is not the actor.
    assert!(!map("mentioned", "me").unwrap().viewer.actor_is_viewer);
}
