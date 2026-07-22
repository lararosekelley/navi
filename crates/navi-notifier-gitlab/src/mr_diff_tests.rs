// Included into `mr_diff.rs`'s `tests` module. Covers the four kinds this path
// owns (merged/closed/ready/comment-reply), silent first-sight baselining, and
// idempotence once notes are recorded as seen.

use super::*;
use crate::api::{Note, SimpleUser};

const VIEWER: &str = "me";
const AUTHOR: &str = "octo";

fn ctx() -> MrContext {
    MrContext {
        viewer: VIEWER.into(),
        repo: Repo::new("acme", "widgets"),
        now: OffsetDateTime::UNIX_EPOCH,
        comment_min_age: None,
    }
}

fn user(name: &str) -> SimpleUser {
    SimpleUser {
        username: name.into(),
        name: None,
        avatar_url: None,
    }
}

fn base_mr() -> MergeRequest {
    MergeRequest {
        iid: 7,
        project_id: 1,
        title: "Add gizmo".into(),
        web_url: Some("https://gl.test/acme/widgets/-/merge_requests/7".into()),
        state: "opened".into(),
        draft: false,
        work_in_progress: false,
        author: Some(user(AUTHOR)),
        merged_by: None,
        merged_at: None,
        closed_at: None,
        updated_at: Some("2024-01-02T03:04:05Z".into()),
        reviewers: vec![],
    }
}

fn note(id: u64, author: &str, body: &str) -> Note {
    Note {
        id,
        system: false,
        body: body.into(),
        author: Some(user(author)),
        created_at: Some("2024-01-02T03:04:05Z".into()),
    }
}

fn discussion(id: &str, notes: Vec<Note>) -> Discussion {
    Discussion {
        id: id.into(),
        notes,
    }
}

fn initialized() -> MrSnapshot {
    MrSnapshot {
        initialized: true,
        ..Default::default()
    }
}

fn kinds(events: &[Event]) -> Vec<&EventKind> {
    events.iter().map(|e| &e.kind).collect()
}

#[test]
fn first_sight_baselines_silently() {
    let mut mr = base_mr();
    mr.state = "merged".into();
    let d = vec![discussion("a", vec![note(1, AUTHOR, "root")])];
    let (events, snap) = diff_mr(&ctx(), &mr, &d, &MrSnapshot::default());
    assert!(events.is_empty(), "nothing back-fills on first sight");
    assert!(snap.initialized && snap.merged);
    assert!(snap.seen_notes.contains(&1), "notes are recorded as seen");
}

#[test]
fn merged_and_closed_fire_once() {
    let mut merged = base_mr();
    merged.state = "merged".into();
    merged.merged_by = Some(user("boss"));
    let (events, snap) = diff_mr(&ctx(), &merged, &[], &initialized());
    assert_eq!(kinds(&events), vec![&EventKind::Merged]);
    assert_eq!(events[0].actor.login, "boss");
    // Idempotent: once the snapshot records merged, it doesn't fire again.
    let (again, _) = diff_mr(&ctx(), &merged, &[], &snap);
    assert!(again.is_empty());

    let mut closed = base_mr();
    closed.state = "closed".into();
    let (events, _) = diff_mr(&ctx(), &closed, &[], &initialized());
    assert_eq!(kinds(&events), vec![&EventKind::Closed]);
}

#[test]
fn ready_fires_once_when_draft_clears() {
    let was_draft = MrSnapshot {
        draft: true,
        ..initialized()
    };
    let mr = base_mr(); // draft = false now
    let (events, snap) = diff_mr(&ctx(), &mr, &[], &was_draft);
    assert_eq!(kinds(&events), vec![&EventKind::ReadyForReview]);
    // Idempotent: once the snapshot records not-draft, it doesn't fire again.
    let (again, _) = diff_mr(&ctx(), &mr, &[], &snap);
    assert!(again.is_empty());
}

#[test]
fn reviewer_relation_reflects_the_reviewers_list() {
    // An event on an MR you only authored is not tagged as a review.
    let mut mr = base_mr();
    mr.author = Some(user(VIEWER));
    mr.state = "merged".into();
    let (events, _) = diff_mr(&ctx(), &mr, &[], &initialized());
    assert!(events[0].viewer.is_author);
    assert!(!events[0].viewer.is_reviewer, "author-only must not read as reviewer");

    // Add the viewer as a requested reviewer: now is_reviewer is set.
    mr.author = Some(user(AUTHOR));
    mr.reviewers = vec![user(VIEWER)];
    let (events, _) = diff_mr(&ctx(), &mr, &[], &initialized());
    assert!(!events[0].viewer.is_author);
    assert!(events[0].viewer.is_reviewer);
}

#[test]
fn reply_in_your_thread_sets_on_your_comment() {
    // You started the discussion; someone else replied.
    let d = vec![discussion(
        "a",
        vec![note(1, VIEWER, "please look"), note(2, AUTHOR, "done")],
    )];
    let old = MrSnapshot {
        seen_notes: HashSet::from([1]),
        ..initialized()
    };
    let (events, _) = diff_mr(&ctx(), &base_mr(), &d, &old);
    assert_eq!(
        kinds(&events),
        vec![&EventKind::CommentReply {
            on_your_comment: true
        }]
    );
    assert_eq!(events[0].actor.login, AUTHOR);
}

#[test]
fn reply_on_your_mr_without_your_note_is_not_on_your_comment() {
    // You authored the MR but didn't start this thread; a reply still reaches you,
    // just not flagged as a direct reply to your comment.
    let mut mr = base_mr();
    mr.author = Some(user(VIEWER));
    let d = vec![discussion("a", vec![note(2, AUTHOR, "a thought")])];
    let (events, _) = diff_mr(&ctx(), &mr, &d, &initialized());
    assert_eq!(
        kinds(&events),
        vec![&EventKind::CommentReply {
            on_your_comment: false
        }]
    );
}

#[test]
fn your_own_notes_and_system_notes_never_fire() {
    let d = vec![discussion(
        "a",
        vec![
            Note {
                system: true,
                ..note(1, AUTHOR, "changed the milestone")
            },
            note(2, VIEWER, "my own reply"),
        ],
    )];
    let mut mr = base_mr();
    mr.author = Some(user(VIEWER));
    let (events, _) = diff_mr(&ctx(), &mr, &d, &initialized());
    assert!(events.is_empty());
}

#[test]
fn reply_in_a_thread_you_never_touched_is_left_to_todos() {
    // Not your MR, not your thread: the note-diff stays silent (a mention there
    // would surface via the todos path instead).
    let d = vec![discussion(
        "a",
        vec![note(1, AUTHOR, "hey"), note(2, "third", "reply")],
    )];
    let (events, _) = diff_mr(&ctx(), &base_mr(), &d, &initialized());
    assert!(events.is_empty());
}

#[test]
fn holds_a_too_fresh_note_until_it_settles() {
    // Your MR, so any reply reaches you; the note is edited in place after posting.
    let mut mr = base_mr();
    mr.author = Some(user(VIEWER));
    let fresh = Note {
        created_at: Some("2024-02-02T03:04:05Z".into()),
        ..note(2, AUTHOR, "working… -> the real reply")
    };
    let d = vec![discussion("a", vec![fresh])];
    let min_age = Some(time::Duration::seconds(60));
    let held_now = OffsetDateTime::parse("2024-02-02T03:04:35Z", &Rfc3339).unwrap(); // +30s
    let settled_now = OffsetDateTime::parse("2024-02-02T03:06:05Z", &Rfc3339).unwrap(); // +120s

    // Too fresh: held back, and left unseen so it re-derives once settled.
    let cx = MrContext {
        now: held_now,
        comment_min_age: min_age,
        ..ctx()
    };
    let (events, snap) = diff_mr(&cx, &mr, &d, &initialized());
    assert!(events.is_empty(), "a fresh note must be held: {:?}", kinds(&events));
    assert!(
        !snap.seen_notes.contains(&2),
        "a held note must stay unseen so it re-derives next poll"
    );

    // Settled: emitted and now recorded as seen.
    let cx = MrContext {
        now: settled_now,
        comment_min_age: min_age,
        ..ctx()
    };
    let (events, snap) = diff_mr(&cx, &mr, &d, &initialized());
    assert_eq!(
        kinds(&events),
        vec![&EventKind::CommentReply {
            on_your_comment: false
        }]
    );
    assert!(snap.seen_notes.contains(&2));

    // Disabled (None): emitted even when fresh - matches GitHub/Gitea.
    let cx = MrContext {
        now: held_now,
        comment_min_age: None,
        ..ctx()
    };
    let (events, _) = diff_mr(&cx, &mr, &d, &initialized());
    assert_eq!(events.len(), 1, "min-age off must not hold anything");
}
