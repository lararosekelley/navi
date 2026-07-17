//! Shared engine for forge-style providers (GitHub, Gitea, ...).
//!
//! Holds the provider-agnostic pieces of a "poll notifications, fetch a PR, diff
//! against a snapshot" source: the input [`model`], the persisted [`snapshot`], and
//! the pure [`diff`] engine. A source crate deserializes or maps its provider's
//! payloads into [`model::PrData`] and calls [`diff::diff`].

pub mod diff;
pub mod model;
pub mod snapshot;

pub use diff::{diff, first_sight_watermark, team_key, DiffContext, FIRST_SIGHT_LEEWAY};
pub use snapshot::PrSnapshot;
