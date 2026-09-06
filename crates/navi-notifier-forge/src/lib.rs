//! Shared engine for forge-style providers (GitHub, Gitea, ...).
//!
//! Holds the provider-agnostic pieces of a "poll notifications, fetch a PR, diff
//! against a snapshot" source: the input [`model`], the persisted [`snapshot`], and
//! the pure [`diff`] engine. A source crate deserializes or maps its provider's
//! payloads into [`model::PrData`] and calls [`diff::diff`].

pub mod diff;
pub mod model;
pub mod snapshot;

pub use diff::{
    diff, excerpt, first_sight_watermark, is_settled, team_key, ts_key, DiffContext,
    FIRST_SIGHT_LEEWAY,
};
pub use snapshot::PrSnapshot;

use navi_notifier_core::model::Event;

/// What one pass over a single pull request achieved.
///
/// Sources keep a per-PR cursor so an unchanged PR is skipped cheaply, and the
/// cursor may only advance past a PR that was actually compared against its
/// snapshot. Advancing past one that was never fetched makes the source skip it
/// until its timestamp moves again, so a momentary failure costs far more than the
/// poll it happened on, and leaves a cursor sitting ahead of its snapshot.
#[derive(Debug)]
pub enum PrOutcome {
    /// Fetched and diffed. The events may be empty; that is still a real result.
    Diffed(Vec<Event>),
    /// The provider says this PR is gone, or invisible to this token. Nothing about
    /// it will change, so the cursor should advance rather than retry for ever.
    Gone,
    /// A transient failure. Nothing was compared, so no cursor may advance past it.
    Unfetched,
}

impl PrOutcome {
    /// Whether the caller may record this PR as seen at its current timestamp.
    pub fn may_advance_cursor(&self) -> bool {
        !matches!(self, PrOutcome::Unfetched)
    }

    /// The events produced, if any. `Gone` and `Unfetched` yield none.
    pub fn into_events(self) -> Vec<Event> {
        match self {
            PrOutcome::Diffed(events) => events,
            PrOutcome::Gone | PrOutcome::Unfetched => Vec::new(),
        }
    }
}
