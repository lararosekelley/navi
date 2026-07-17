//! The pure diff engine: `(previous snapshot, freshly fetched PR data) -> events`.
//!
//! Maps GitHub's shape onto navi's taxonomy. Free of I/O so it can be unit-tested
//! from fixtures; the source layer  handles fetching and
//! persistence.

use std::collections::{HashMap, HashSet};

use navi_notifier_core::model::{
    Actor, Event, EventKind, PullRequest, Repo, ReviewState, ViewerRelationship,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::model::{PrData, ReviewComment, User};
use crate::snapshot::PrSnapshot;

/// On first sight of a PR, how far back before the triggering notification's
/// update time still counts as "what just happened" - tolerates a burst of
/// activity and small clock differences between a review and its notification.
pub const FIRST_SIGHT_LEEWAY: time::Duration = time::Duration::minutes(10);

/// The first-sight watermark for a notification: activity from its `updated_at`
/// (RFC3339) back through [`FIRST_SIGHT_LEEWAY`] counts as "what just happened".
/// `None` when the timestamp is missing or unparseable, which falls back to
/// surfacing only outstanding review asks.
pub fn first_sight_watermark(updated_at: Option<&str>) -> Option<OffsetDateTime> {
    updated_at
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .map(|t| t - FIRST_SIGHT_LEEWAY)
}

/// Ambient inputs for a diff that don't come from the PR payload.
pub struct DiffContext {
    pub source_id: String,
    /// Login of the authenticated user ("you").
    pub viewer_login: String,
    pub repo: Repo,
    /// Fallback timestamp when the provider omits one.
    pub now: OffsetDateTime,
    /// On a PR's first sighting, activity at/after this instant is surfaced (it is
    /// what the triggering notification points at); older history is not
    /// back-filled. `None` surfaces only outstanding review asks and suppresses
    /// everything else on first sight (reviews, comments, mentions) - the old
    /// behaviour, for sources that can't supply a watermark.
    pub first_sight_since: Option<OffsetDateTime>,
}

/// Diff `data` against `old`, returning the events to deliver and the snapshot to
/// persist. The first time a PR is seen (`!old.initialized`), outstanding review
/// requests plus activity at/after `ctx.first_sight_since` are surfaced; older
/// history is not back-filled.
pub fn diff(ctx: &DiffContext, data: &PrData, old: &PrSnapshot) -> (Vec<Event>, PrSnapshot) {
    let pr = &data.pull_request;
    let viewer = &ctx.viewer_login;

    let author_login = pr
        .user
        .as_ref()
        .map(|u| u.login.clone())
        .unwrap_or_default();
    let is_author = eq_login(&author_login, viewer);
    let viewer_requested_now = pr
        .requested_reviewers
        .iter()
        .any(|u| eq_login(&u.login, viewer));
    let viewer_reviewed_now = old.viewer_reviewed
        || data.reviews.iter().any(|r| {
            r.user.as_ref().is_some_and(|u| eq_login(&u.login, viewer)) && r.state != "PENDING"
        });
    let is_reviewer = viewer_requested_now || viewer_reviewed_now;

    let normalized_pr = PullRequest {
        repo: ctx.repo.clone(),
        number: pr.number,
        title: pr.title.clone(),
        url: pr.html_url.clone(),
        author: Actor::new(if author_login.is_empty() {
            "ghost".to_string()
        } else {
            author_login.clone()
        }),
        draft: pr.draft,
    };
    let viewer_rel = ViewerRelationship {
        is_author,
        is_reviewer,
    };

    let new_snapshot = build_snapshot(data, viewer, old);

    let mut events = Vec::new();
    let mut emit = |kind: EventKind,
                    actor: Actor,
                    occurred: OffsetDateTime,
                    target: Option<String>,
                    excerpt: Option<String>,
                    disc: String| {
        events.push(Event {
            source_id: ctx.source_id.clone(),
            kind,
            pull_request: normalized_pr.clone(),
            viewer: viewer_rel,
            actor,
            occurred_at: occurred,
            target_url: target.or_else(|| Some(normalized_pr.url.clone())),
            excerpt,
            dedup_key: Event::make_dedup_key(&ctx.source_id, &ctx.repo, pr.number, &disc),
        });
    };

    let author_actor = || {
        Actor::new(if author_login.is_empty() {
            "ghost"
        } else {
            &author_login
        })
    };

    // Review request / re-review request (edge-detected).
    if viewer_requested_now && !old.viewer_requested && !is_author {
        let (kind, disc) = if old.viewer_reviewed {
            (EventKind::ReReviewRequested, "re_review_requested")
        } else {
            (EventKind::ReviewRequested, "review_requested")
        };
        emit(
            kind,
            author_actor(),
            parse_ts(pr.updated_at.as_deref(), ctx.now),
            Some(pr.html_url.clone()),
            None,
            format!("{disc}:{}", ts_key(pr.updated_at.as_deref())),
        );
    }

    // Reviews: submissions by others, and dismissals of your reviews.
    for review in &data.reviews {
        let reviewer_login = review
            .user
            .as_ref()
            .map(|u| u.login.as_str())
            .unwrap_or("ghost");
        let by_viewer = eq_login(reviewer_login, viewer);
        let previously = old.seen_reviews.get(&review.id).map(String::as_str);

        // Your review just became dismissed.
        if by_viewer && review.state == "DISMISSED" && previously != Some("DISMISSED") {
            emit(
                EventKind::ReviewDismissed,
                author_actor(),
                parse_ts(review.submitted_at.as_deref(), ctx.now),
                review.html_url.clone(),
                None,
                format!("review_dismissed:{}", review.id),
            );
            continue;
        }

        // A new review by someone else, on a PR you're involved in.
        if !by_viewer && previously.is_none() {
            if let Some(state) = review_state(&review.state) {
                if is_author || is_reviewer {
                    emit(
                        EventKind::ReviewSubmitted { state },
                        actor_from(review.user.as_ref()),
                        parse_ts(review.submitted_at.as_deref(), ctx.now),
                        review.html_url.clone(),
                        None,
                        format!("review:{}", review.id),
                    );
                }
            }
        }
    }

    // Inline review comments: replies in threads you're part of, and mentions.
    let by_id: HashMap<u64, &ReviewComment> =
        data.review_comments.iter().map(|c| (c.id, c)).collect();
    let viewer_roots: HashSet<u64> = data
        .review_comments
        .iter()
        .filter(|c| c.user.as_ref().is_some_and(|u| eq_login(&u.login, viewer)))
        .map(|c| c.in_reply_to_id.unwrap_or(c.id))
        .collect();

    for c in &data.review_comments {
        if old.seen_review_comments.contains(&c.id) {
            continue;
        }
        let author = c.user.as_ref().map(|u| u.login.as_str()).unwrap_or("ghost");
        if eq_login(author, viewer) {
            continue;
        }
        let root = c.in_reply_to_id.unwrap_or(c.id);
        let on_your_comment = c
            .in_reply_to_id
            .and_then(|pid| by_id.get(&pid))
            .and_then(|p| p.user.as_ref())
            .is_some_and(|u| eq_login(&u.login, viewer));
        let participated = viewer_roots.contains(&root);

        if mentions(&c.body, viewer) {
            emit(
                EventKind::Mentioned,
                actor_from(c.user.as_ref()),
                parse_ts(c.created_at.as_deref(), ctx.now),
                c.html_url.clone(),
                excerpt(&c.body),
                format!("review_comment:{}", c.id),
            );
        } else if on_your_comment || participated {
            emit(
                EventKind::CommentReply { on_your_comment },
                actor_from(c.user.as_ref()),
                parse_ts(c.created_at.as_deref(), ctx.now),
                c.html_url.clone(),
                excerpt(&c.body),
                format!("review_comment:{}", c.id),
            );
        }
    }

    // Conversation (issue) comments: mentions, and replies where you took part.
    let viewer_in_conversation = data
        .issue_comments
        .iter()
        .any(|c| c.user.as_ref().is_some_and(|u| eq_login(&u.login, viewer)));

    for c in &data.issue_comments {
        if old.seen_issue_comments.contains(&c.id) {
            continue;
        }
        let author = c.user.as_ref().map(|u| u.login.as_str()).unwrap_or("ghost");
        if eq_login(author, viewer) {
            continue;
        }
        if mentions(&c.body, viewer) {
            emit(
                EventKind::Mentioned,
                actor_from(c.user.as_ref()),
                parse_ts(c.created_at.as_deref(), ctx.now),
                c.html_url.clone(),
                excerpt(&c.body),
                format!("issue_comment:{}", c.id),
            );
        } else if viewer_in_conversation || is_author {
            emit(
                EventKind::CommentReply {
                    on_your_comment: false,
                },
                actor_from(c.user.as_ref()),
                parse_ts(c.created_at.as_deref(), ctx.now),
                c.html_url.clone(),
                excerpt(&c.body),
                format!("issue_comment:{}", c.id),
            );
        }
    }

    // Lifecycle transitions.
    if pr.merged && !old.merged {
        let sha = pr
            .merge_commit_sha
            .clone()
            .unwrap_or_else(|| pr.number.to_string());
        emit(
            EventKind::Merged,
            pr.merged_by
                .as_ref()
                .map(|u| actor_from(Some(u)))
                .unwrap_or_else(author_actor),
            parse_ts(pr.merged_at.as_deref(), ctx.now),
            Some(pr.html_url.clone()),
            None,
            format!("merged:{sha}"),
        );
    } else if pr.state == "closed" && !pr.merged && !old.closed {
        emit(
            EventKind::Closed,
            author_actor(),
            parse_ts(pr.closed_at.as_deref(), ctx.now),
            Some(pr.html_url.clone()),
            None,
            format!("closed:{}", ts_key(pr.closed_at.as_deref())),
        );
    }

    if !pr.draft && old.draft {
        emit(
            EventKind::ReadyForReview,
            author_actor(),
            parse_ts(pr.updated_at.as_deref(), ctx.now),
            Some(pr.html_url.clone()),
            None,
            format!("ready:{}", ts_key(pr.updated_at.as_deref())),
        );
    }

    // First sighting: `old` was empty, so every check above fired as if brand new.
    // Keep the currently-outstanding review ask (so a fresh start still shows what
    // is waiting on you), but otherwise surface only what happened at/after the
    // watermark - the triggering notification's moment - instead of back-filling
    // the PR's entire history.
    if !old.initialized {
        events.retain(|e| {
            // ReReviewRequested is unreachable on first sight (it needs a prior
            // review in `old`); kept here so the "review ask always survives" rule
            // reads completely.
            matches!(
                e.kind,
                EventKind::ReviewRequested | EventKind::ReReviewRequested
            ) || ctx
                .first_sight_since
                .is_some_and(|since| e.occurred_at >= since)
        });
    }

    (events, new_snapshot)
}

/// Compute the snapshot to persist after this poll.
fn build_snapshot(data: &PrData, viewer: &str, old: &PrSnapshot) -> PrSnapshot {
    let pr = &data.pull_request;
    PrSnapshot {
        seen_reviews: data
            .reviews
            .iter()
            .map(|r| (r.id, r.state.clone()))
            .collect(),
        seen_review_comments: data.review_comments.iter().map(|c| c.id).collect(),
        seen_issue_comments: data.issue_comments.iter().map(|c| c.id).collect(),
        viewer_requested: pr
            .requested_reviewers
            .iter()
            .any(|u| eq_login(&u.login, viewer)),
        viewer_reviewed: old.viewer_reviewed
            || data.reviews.iter().any(|r| {
                r.user.as_ref().is_some_and(|u| eq_login(&u.login, viewer)) && r.state != "PENDING"
            }),
        merged: pr.merged,
        closed: pr.state == "closed",
        draft: pr.draft,
        initialized: true,
    }
}

fn review_state(raw: &str) -> Option<ReviewState> {
    match raw {
        "APPROVED" => Some(ReviewState::Approved),
        "CHANGES_REQUESTED" => Some(ReviewState::ChangesRequested),
        "COMMENTED" => Some(ReviewState::Commented),
        _ => None,
    }
}

/// GitHub logins are case-insensitive.
fn eq_login(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn actor_from(user: Option<&User>) -> Actor {
    match user {
        Some(u) => Actor {
            login: u.login.clone(),
            display_name: None,
            avatar_url: u.avatar_url.clone(),
        },
        None => Actor::new("ghost"),
    }
}

fn parse_ts(raw: Option<&str>, fallback: OffsetDateTime) -> OffsetDateTime {
    raw.and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .unwrap_or(fallback)
}

/// Stable-enough discriminator fragment from a timestamp string.
fn ts_key(raw: Option<&str>) -> String {
    raw.unwrap_or("0").to_string()
}

/// First non-empty line of a comment body, trimmed to a readable length.
fn excerpt(body: &str) -> Option<String> {
    let line = body.lines().map(str::trim).find(|l| !l.is_empty())?;
    const MAX: usize = 140;
    if line.chars().count() > MAX {
        Some(format!("{}…", line.chars().take(MAX).collect::<String>()))
    } else {
        Some(line.to_string())
    }
}

/// True if `body` @-mentions `login` (case-insensitive, respecting username boundaries).
fn mentions(body: &str, login: &str) -> bool {
    let body = body.to_ascii_lowercase();
    let needle = format!("@{}", login.to_ascii_lowercase());
    let bytes = body.as_bytes();
    let mut from = 0;
    while let Some(rel) = body[from..].find(&needle) {
        let at = from + rel;
        let before_ok = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        let after_idx = at + needle.len();
        let after_ok = bytes
            .get(after_idx)
            .is_none_or(|&b| !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'/'));
        if before_ok && after_ok {
            return true;
        }
        from = at + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    include!("diff_tests.rs");
}
