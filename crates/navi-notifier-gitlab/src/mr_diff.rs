//! Snapshot-based diffing of a merge request and its discussion notes into the
//! lifecycle and reply events the Todos API can't express: merged, closed,
//! ready-for-review, and replies in threads you took part in. Review-request and
//! mention events still come from the todos path, so this deliberately does not
//! emit them (that would double-fire).
//!
//! Pure and unit-tested: the source layer fetches the MR + discussions and
//! persists the snapshot; the decision logic lives here.

use std::collections::HashSet;

use navi_notifier_core::model::{
    Actor, Backfill, Event, EventKind, PullRequest, Repo, ViewerRelationship,
};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::api::{Discussion, MergeRequest};

const SOURCE_ID: &str = "gitlab";

/// Per-MR state carried between polls so activity fires exactly once.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MrSnapshot {
    /// Ids of human notes already accounted for.
    #[serde(default)]
    pub seen_notes: HashSet<u64>,
    #[serde(default)]
    pub merged: bool,
    #[serde(default)]
    pub closed: bool,
    #[serde(default)]
    pub draft: bool,
    /// False until the first poll has baselined this MR, so pre-existing history
    /// never back-fills.
    #[serde(default)]
    pub initialized: bool,
}

/// Ambient inputs for [`diff_mr`] that don't come from the MR payload.
pub struct MrContext {
    /// The authenticated user's username ("you").
    pub viewer: String,
    pub repo: Repo,
    /// Fallback timestamp when GitLab omits one.
    pub now: OffsetDateTime,
    /// Hold a note back until it is at least this old, so an edit-in-place bot
    /// resolves to its final text before we notify. `None` disables the hold.
    pub comment_min_age: Option<time::Duration>,
    /// First-run backfill override. Only `Some(AllOpen)` surfaces this MR's
    /// derivable activity on first sight; every other value (including `None`)
    /// baselines silently. Review-request backfill is the todos path's job.
    pub first_sight_backfill: Option<Backfill>,
}

/// Diff a merge request and its discussions against the last-seen snapshot,
/// returning the events to deliver and the snapshot to persist. The first time an
/// MR is seen (`!old.initialized`) it is baselined silently: state and notes are
/// recorded but nothing is emitted.
pub fn diff_mr(
    ctx: &MrContext,
    mr: &MergeRequest,
    discussions: &[Discussion],
    old: &MrSnapshot,
) -> (Vec<Event>, MrSnapshot) {
    let viewer = &ctx.viewer;
    let author_login = mr
        .author
        .as_ref()
        .map(|u| u.username.clone())
        .unwrap_or_default();
    let is_author = eq(&author_login, viewer);
    let is_reviewer = mr.has_reviewer(viewer);

    let normalized = PullRequest {
        repo: ctx.repo.clone(),
        number: mr.iid,
        title: mr.title.clone(),
        url: mr.web_url.clone().unwrap_or_default(),
        author: Actor::new(if author_login.is_empty() {
            "unknown".to_string()
        } else {
            author_login.clone()
        }),
        draft: mr.is_draft(),
    };

    let new_snapshot = MrSnapshot {
        // A held (not-yet-settled) note is left unseen so it is re-evaluated next
        // poll, by which point its edits have landed.
        seen_notes: discussions
            .iter()
            .flat_map(|d| &d.notes)
            .filter(|n| {
                !n.system && is_settled(n.created_at.as_deref(), ctx.now, ctx.comment_min_age)
            })
            .map(|n| n.id)
            .collect(),
        merged: mr.is_merged(),
        closed: mr.is_closed(),
        draft: mr.is_draft(),
        initialized: true,
    };

    // First sight: baseline only, so a freshly-added MR doesn't replay its history -
    // unless `all_open` backfill asked for the full history on the first poll.
    if !old.initialized && ctx.first_sight_backfill != Some(Backfill::AllOpen) {
        return (Vec::new(), new_snapshot);
    }

    let mut events = Vec::new();
    let mut emit = |kind: EventKind,
                    actor: Actor,
                    occurred: OffsetDateTime,
                    target: Option<String>,
                    excerpt: Option<String>,
                    disc: String| {
        events.push(Event {
            source_id: SOURCE_ID.to_string(),
            kind,
            pull_request: normalized.clone(),
            viewer: ViewerRelationship {
                is_author,
                is_reviewer,
                actor_is_viewer: eq(&actor.login, viewer),
            },
            actor,
            occurred_at: occurred,
            target_url: target,
            excerpt,
            dedup_key: Event::make_dedup_key(SOURCE_ID, &ctx.repo, mr.iid, &disc),
        });
    };
    let author_actor = || {
        Actor::new(if author_login.is_empty() {
            "unknown"
        } else {
            &author_login
        })
    };

    // Lifecycle transitions.
    if mr.is_merged() && !old.merged {
        emit(
            EventKind::Merged,
            mr.merged_by
                .as_ref()
                .map(|u| Actor::new(u.username.as_str()))
                .unwrap_or_else(author_actor),
            parse_ts(mr.merged_at.as_deref(), ctx.now),
            mr.web_url.clone(),
            None,
            format!("merged:{}", ts_key(mr.merged_at.as_deref())),
        );
    } else if mr.is_closed() && !old.closed {
        emit(
            EventKind::Closed,
            author_actor(),
            parse_ts(mr.closed_at.as_deref(), ctx.now),
            mr.web_url.clone(),
            None,
            format!("closed:{}", ts_key(mr.closed_at.as_deref())),
        );
    }

    if !mr.is_draft() && old.draft {
        emit(
            EventKind::ReadyForReview,
            author_actor(),
            parse_ts(mr.updated_at.as_deref(), ctx.now),
            mr.web_url.clone(),
            None,
            format!("ready:{}", ts_key(mr.updated_at.as_deref())),
        );
    }

    // Replies in threads you took part in. A note is surfaced when it's new, not
    // yours, not a system breadcrumb, and lands in a discussion you started or
    // participated in (or on your own MR). Mentions stay with the todos path.
    for d in discussions {
        let root_by_viewer = d
            .notes
            .iter()
            .find(|n| !n.system)
            .and_then(|n| n.author.as_ref())
            .is_some_and(|u| eq(&u.username, viewer));
        let viewer_in_thread = d
            .notes
            .iter()
            .filter(|n| !n.system)
            .any(|n| n.author.as_ref().is_some_and(|u| eq(&u.username, viewer)));

        for note in &d.notes {
            if note.system || old.seen_notes.contains(&note.id) {
                continue;
            }
            // Hold a too-fresh note; it stays unseen and re-derives once settled.
            if !is_settled(note.created_at.as_deref(), ctx.now, ctx.comment_min_age) {
                continue;
            }
            let note_author = note.author.as_ref().map(|u| u.username.as_str());
            if note_author.is_some_and(|a| eq(a, viewer)) {
                continue;
            }
            if !(viewer_in_thread || is_author) {
                continue;
            }
            emit(
                EventKind::CommentReply {
                    on_your_comment: root_by_viewer,
                },
                note.author
                    .as_ref()
                    .map(|u| Actor::new(u.username.as_str()))
                    .unwrap_or_else(|| Actor::new("unknown")),
                parse_ts(note.created_at.as_deref(), ctx.now),
                mr.web_url.clone(),
                excerpt(&note.body),
                format!("note:{}", note.id),
            );
        }
    }

    (events, new_snapshot)
}

/// GitLab usernames are case-insensitive.
fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Whether a note is old enough to notify about, given the optional min-age hold. A
/// missing or unparseable timestamp counts as settled, so bad data never holds a
/// note back forever. Mirrors the forge diff's `is_settled`.
fn is_settled(
    created_at: Option<&str>,
    now: OffsetDateTime,
    min_age: Option<time::Duration>,
) -> bool {
    let Some(min_age) = min_age else {
        return true;
    };
    let Some(created) = created_at.and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok()) else {
        return true;
    };
    now - created >= min_age
}

fn parse_ts(raw: Option<&str>, fallback: OffsetDateTime) -> OffsetDateTime {
    raw.and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .unwrap_or(fallback)
}

fn ts_key(raw: Option<&str>) -> String {
    raw.unwrap_or("0").to_string()
}

/// First non-empty line of a note body, trimmed to a readable length.
fn excerpt(body: &str) -> Option<String> {
    let line = body.lines().map(str::trim).find(|l| !l.is_empty())?;
    const MAX: usize = 140;
    if line.chars().count() > MAX {
        Some(format!("{}…", line.chars().take(MAX).collect::<String>()))
    } else {
        Some(line.to_string())
    }
}

#[cfg(test)]
mod tests {
    include!("mr_diff_tests.rs");
}
