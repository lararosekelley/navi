//! The extension seams that make navi provider-agnostic.
//!
//! A provider crate implements [`Source`] (things that produce events) and/or
//! [`Destination`] (things that deliver them). The engine wires arbitrary sets of each
//! together through the registry, so adding GitLab or Discord is "implement a trait,
//! register a constructor" with no engine changes.

use async_trait::async_trait;

use crate::error::{DestinationError, SourceError, StateError};
use crate::model::Event;

/// Durable, provider-agnostic storage the engine and sources rely on.
///
/// Three responsibilities:
/// - **Snapshots**: opaque per-PR bytes a source uses to diff current vs. last-seen
///   state. The store neither interprets nor validates them.
/// - **Dedup**: a set of delivered `dedup_key`s guaranteeing idempotent delivery.
/// - **Cursors**: small opaque strings for poll bookkeeping (ETags, timestamps).
#[async_trait]
pub trait StateStore: Send + Sync {
    async fn get_snapshot(
        &self,
        source_id: &str,
        scope: &str,
    ) -> Result<Option<Vec<u8>>, StateError>;

    async fn put_snapshot(
        &self,
        source_id: &str,
        scope: &str,
        bytes: &[u8],
    ) -> Result<(), StateError>;

    /// True if `dedup_key` was already delivered successfully.
    async fn was_delivered(&self, dedup_key: &str) -> Result<bool, StateError>;

    /// Record `dedup_key` as delivered. Idempotent.
    async fn mark_delivered(&self, dedup_key: &str) -> Result<(), StateError>;

    async fn get_cursor(&self, source_id: &str, key: &str) -> Result<Option<String>, StateError>;

    async fn put_cursor(&self, source_id: &str, key: &str, value: &str) -> Result<(), StateError>;
}

/// A producer of normalized [`Event`]s.
///
/// `poll` is expected to (1) read prior snapshots/cursors from `state`, (2) fetch
/// current provider state, (3) diff to derive events, and (4) persist advanced
/// snapshots/cursors back to `state`. Idempotent *delivery* is the engine's job via
/// the dedup set, so `poll` may legitimately return events it has returned before;
/// the engine filters them out.
#[async_trait]
pub trait Source: Send + Sync {
    /// Stable identifier, e.g. `"github"`. Used in dedup keys and config routing.
    fn id(&self) -> &str;

    /// Poll the provider and return newly-derived events (unordered).
    async fn poll(&self, state: &dyn StateStore) -> Result<Vec<Event>, SourceError>;

    /// Optional hook invoked once an event has been delivered successfully, letting
    /// the source advance provider-side state (e.g. mark a notification thread read).
    /// Default: no-op.
    async fn commit(&self, _state: &dyn StateStore, _event: &Event) -> Result<(), SourceError> {
        Ok(())
    }
}

/// A delivery target for events (Slack, Discord, email, ...).
#[async_trait]
pub trait Destination: Send + Sync {
    /// Stable identifier, e.g. `"slack"`.
    fn id(&self) -> &str;

    /// Deliver a single, already-filtered event. Implementations should be
    /// resilient to transient failure (retry/backoff) before returning `Err`.
    async fn send(&self, event: &Event) -> Result<(), DestinationError>;
}
