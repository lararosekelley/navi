// Included into `diff.rs`'s `tests` module via `include!`. Exercises every event
// kind and the two properties that keep the daemon quiet: no history back-fill on
// first sight, and idempotence once the snapshot is persisted.

use super::*;
use crate::model::{IssueComment, PrData, PullRequest, Review, ReviewComment, Team, User};
use navi_notifier_core::model::{EventKind, ReviewState};

const VIEWER: &str = "me";
const AUTHOR: &str = "octo";

fn user(login: &str) -> User {
    User {
        login: login.into(),
        avatar_url: None,
        html_url: None,
    }
}

fn ctx() -> DiffContext {
    DiffContext {
        source_id: "github".into(),
        viewer_login: VIEWER.into(),
        repo: Repo::new("acme", "widgets"),
        now: OffsetDateTime::UNIX_EPOCH,
        first_sight_since: None,
        viewer_teams: std::collections::HashSet::new(),
    }
}

fn base_pr() -> PullRequest {
    PullRequest {
        number: 12,
        title: "Add gizmo".into(),
        html_url: "https://gh.test/acme/widgets/pull/12".into(),
        state: "open".into(),
        draft: false,
        merged: false,
        merged_at: None,
        closed_at: None,
        updated_at: Some("2024-01-02T03:04:05Z".into()),
        merge_commit_sha: None,
        user: Some(user(AUTHOR)),
        merged_by: None,
        requested_reviewers: vec![],
        requested_teams: vec![],
    }
}

fn data(pr: PullRequest) -> PrData {
    PrData {
        pull_request: pr,
        reviews: vec![],
        review_comments: vec![],
        issue_comments: vec![],
    }
}

/// A snapshot representing "we've seen this PR before, nothing notable yet".
fn initialized() -> PrSnapshot {
    PrSnapshot {
        initialized: true,
        ..Default::default()
    }
}

fn review(id: u64, login: &str, state: &str) -> Review {
    Review {
        id,
        user: Some(user(login)),
        state: state.into(),
        submitted_at: Some("2024-01-02T03:04:05Z".into()),
        html_url: Some(format!("https://gh.test/r/{id}")),
    }
}

fn review_at(id: u64, login: &str, state: &str, at: &str) -> Review {
    Review {
        submitted_at: Some(at.into()),
        ..review(id, login, state)
    }
}

fn rcomment(id: u64, login: &str, body: &str, in_reply_to: Option<u64>) -> ReviewComment {
    ReviewComment {
        id,
        user: Some(user(login)),
        body: body.into(),
        in_reply_to_id: in_reply_to,
        html_url: Some(format!("https://gh.test/rc/{id}")),
        created_at: Some("2024-01-02T03:04:05Z".into()),
    }
}

fn icomment(id: u64, login: &str, body: &str) -> IssueComment {
    IssueComment {
        id,
        user: Some(user(login)),
        body: body.into(),
        html_url: Some(format!("https://gh.test/ic/{id}")),
        created_at: Some("2024-01-02T03:04:05Z".into()),
    }
}

fn icomment_at(id: u64, login: &str, body: &str, at: &str) -> IssueComment {
    IssueComment {
        created_at: Some(at.into()),
        ..icomment(id, login, body)
    }
}

fn kinds(events: &[Event]) -> Vec<&EventKind> {
    events.iter().map(|e| &e.kind).collect()
}

#[test]
fn review_requested_via_team_membership() {
    // #21: the request is routed to a team you belong to, not to you directly.
    let mut pr = base_pr();
    pr.requested_teams = vec![Team {
        slug: "reviewers".into(),
    }];
    let cx = DiffContext {
        viewer_teams: std::collections::HashSet::from(["acme/reviewers".to_string()]),
        ..ctx()
    };
    let (events, snap) = diff(&cx, &data(pr), &initialized());
    assert_eq!(kinds(&events), vec![&EventKind::ReviewRequested]);
    assert!(snap.viewer_requested, "team request must persist so it won't re-fire");
}

#[test]
fn team_request_ignored_when_not_a_member() {
    let mut pr = base_pr();
    pr.requested_teams = vec![Team {
        slug: "reviewers".into(),
    }];
    // Default ctx has no team memberships.
    let (events, _) = diff(&ctx(), &data(pr), &initialized());
    assert!(events.is_empty(), "unexpected: {:?}", kinds(&events));
}

#[test]
fn team_request_isolated_by_org() {
    // The repo is acme/widgets. Membership in a same-named team of a *different*
    // org must not match this org's request.
    let mut pr = base_pr();
    pr.requested_teams = vec![Team {
        slug: "reviewers".into(),
    }];
    let cx = DiffContext {
        viewer_teams: std::collections::HashSet::from(["contoso/reviewers".to_string()]),
        ..ctx()
    };
    let (events, _) = diff(&cx, &data(pr), &initialized());
    assert!(
        events.is_empty(),
        "cross-org team must not match: {:?}",
        kinds(&events)
    );
}

#[test]
fn first_sighting_emits_outstanding_review_request() {
    let mut pr = base_pr();
    pr.requested_reviewers = vec![user(VIEWER)];
    let (events, snap) = diff(&ctx(), &data(pr), &PrSnapshot::default());
    assert_eq!(kinds(&events), vec![&EventKind::ReviewRequested]);
    assert!(snap.initialized);
    assert!(snap.viewer_requested);
}

#[test]
fn first_sighting_does_not_backfill_history() {
    let mut d = data(base_pr());
    d.reviews = vec![review(1, "someoneelse", "APPROVED")];
    d.issue_comments = vec![icomment(9, "someoneelse", "hey @me look")];
    // Fresh (uninitialized) snapshot, no watermark: nothing outstanding → zero events.
    let (events, snap) = diff(&ctx(), &d, &PrSnapshot::default());
    assert!(events.is_empty(), "unexpected: {:?}", kinds(&events));
    assert!(snap.initialized);
    // The history is recorded so it won't fire on the next poll either.
    assert!(snap.seen_reviews.contains_key(&1));
    assert!(snap.seen_issue_comments.contains(&9));
}

#[test]
fn first_sighting_surfaces_the_triggering_review() {
    // The #34 case: you authored the PR (so GitHub never notified you when you
    // opened it), a review just landed, and this is the first time navi sees the
    // PR. The review that triggered the notification must be surfaced; an older
    // review on the same PR must not be back-filled.
    let mut pr = base_pr();
    pr.user = Some(user(VIEWER));
    let mut d = data(pr);
    d.reviews = vec![
        review_at(1, "reviewer", "APPROVED", "2024-01-01T00:00:00Z"),
        review_at(2, "reviewer", "CHANGES_REQUESTED", "2024-01-02T03:04:05Z"),
    ];
    let cx = DiffContext {
        // Watermark from the triggering notification's update time.
        first_sight_since: first_sight_watermark(Some("2024-01-02T03:04:05Z")),
        ..ctx()
    };
    let (events, snap) = diff(&cx, &d, &PrSnapshot::default());
    assert_eq!(
        kinds(&events),
        vec![&EventKind::ReviewSubmitted {
            state: ReviewState::ChangesRequested
        }]
    );
    assert!(snap.initialized);
}

#[test]
fn first_sighting_surfaces_a_recent_comment_not_old_ones() {
    // First sight triggered by a reply on a PR you authored: the recent comment
    // must surface; an older one on the same PR must not be back-filled.
    let mut pr = base_pr();
    pr.user = Some(user(VIEWER));
    let mut d = data(pr);
    d.issue_comments = vec![
        icomment_at(1, "someoneelse", "old note", "2024-01-01T00:00:00Z"),
        icomment_at(2, "someoneelse", "just replied", "2024-01-02T03:04:05Z"),
    ];
    let cx = DiffContext {
        first_sight_since: first_sight_watermark(Some("2024-01-02T03:04:05Z")),
        ..ctx()
    };
    let (events, _) = diff(&cx, &d, &PrSnapshot::default());
    assert_eq!(
        kinds(&events),
        vec![&EventKind::CommentReply {
            on_your_comment: false
        }]
    );
}

#[test]
fn new_review_request_fires_once() {
    let mut pr = base_pr();
    pr.requested_reviewers = vec![user(VIEWER)];
    let old = initialized();
    let (events, snap) = diff(&ctx(), &data(pr.clone()), &old);
    assert_eq!(kinds(&events), vec![&EventKind::ReviewRequested]);
    // Re-running with the persisted snapshot must be silent (idempotent).
    let (again, _) = diff(&ctx(), &data(pr), &snap);
    assert!(again.is_empty(), "unexpected: {:?}", kinds(&again));
}

#[test]
fn re_review_request_when_already_reviewed() {
    let mut pr = base_pr();
    pr.requested_reviewers = vec![user(VIEWER)];
    let old = PrSnapshot {
        viewer_reviewed: true,
        ..initialized()
    };
    let (events, _) = diff(&ctx(), &data(pr), &old);
    assert_eq!(kinds(&events), vec![&EventKind::ReReviewRequested]);
}

#[test]
fn review_submitted_by_other_on_my_pr() {
    // Viewer authored the PR.
    let mut pr = base_pr();
    pr.user = Some(user(VIEWER));
    let mut d = data(pr);
    d.reviews = vec![review(5, "reviewer", "CHANGES_REQUESTED")];
    let (events, _) = diff(&ctx(), &d, &initialized());
    assert_eq!(
        kinds(&events),
        vec![&EventKind::ReviewSubmitted {
            state: ReviewState::ChangesRequested
        }]
    );
    assert_eq!(events[0].actor.login, "reviewer");
}

#[test]
fn my_review_dismissed() {
    let mut d = data(base_pr());
    d.reviews = vec![review(7, VIEWER, "DISMISSED")];
    // Previously seen as an approval.
    let old = PrSnapshot {
        seen_reviews: [(7, "APPROVED".to_string())].into_iter().collect(),
        viewer_reviewed: true,
        ..initialized()
    };
    let (events, _) = diff(&ctx(), &d, &old);
    assert_eq!(kinds(&events), vec![&EventKind::ReviewDismissed]);
}

#[test]
fn reply_to_my_review_comment_is_on_your_comment() {
    let mut d = data(base_pr());
    d.review_comments = vec![
        rcomment(100, VIEWER, "please rename this", None), // my thread root (already seen)
        rcomment(101, "collab", "done!", Some(100)),       // their new reply
    ];
    let old = PrSnapshot {
        seen_review_comments: [100].into_iter().collect(),
        ..initialized()
    };
    let (events, _) = diff(&ctx(), &d, &old);
    assert_eq!(
        kinds(&events),
        vec![&EventKind::CommentReply {
            on_your_comment: true
        }]
    );
    assert_eq!(events[0].excerpt.as_deref(), Some("done!"));
}

#[test]
fn comment_in_thread_i_never_joined_is_ignored() {
    let mut d = data(base_pr());
    d.review_comments = vec![
        rcomment(200, "alice", "nit", None),
        rcomment(201, "bob", "agreed", Some(200)),
    ];
    let (events, _) = diff(&ctx(), &d, &initialized());
    assert!(events.is_empty(), "unexpected: {:?}", kinds(&events));
}

#[test]
fn mention_beats_reply() {
    let mut d = data(base_pr());
    d.issue_comments = vec![icomment(300, "alice", "hey @me can you take a look?")];
    let (events, _) = diff(&ctx(), &d, &initialized());
    assert_eq!(kinds(&events), vec![&EventKind::Mentioned]);
}

#[test]
fn merged_closed_ready_transitions() {
    // Merged.
    let mut pr = base_pr();
    pr.merged = true;
    pr.state = "closed".into();
    pr.merged_at = Some("2024-01-03T00:00:00Z".into());
    pr.merge_commit_sha = Some("abc123".into());
    pr.merged_by = Some(user("merger"));
    let (events, _) = diff(&ctx(), &data(pr), &initialized());
    assert_eq!(kinds(&events), vec![&EventKind::Merged]);
    assert_eq!(events[0].actor.login, "merger");

    // Closed without merge.
    let mut pr = base_pr();
    pr.state = "closed".into();
    pr.closed_at = Some("2024-01-03T00:00:00Z".into());
    let (events, _) = diff(&ctx(), &data(pr), &initialized());
    assert_eq!(kinds(&events), vec![&EventKind::Closed]);

    // Draft → ready.
    let pr = base_pr(); // draft=false now
    let old = PrSnapshot {
        draft: true,
        ..initialized()
    };
    let (events, _) = diff(&ctx(), &data(pr), &old);
    assert_eq!(kinds(&events), vec![&EventKind::ReadyForReview]);
}

#[test]
fn mentions_respects_boundaries() {
    assert!(mentions("ping @me now", "me"));
    assert!(mentions("@Me at the start", "me")); // case-insensitive
    assert!(mentions("(@me)", "me"));
    assert!(!mentions("email me@host.com", "me")); // preceded by alnum
    assert!(!mentions("hi @mentor", "me")); // longer username
    assert!(!mentions("nothing here", "me"));
}
