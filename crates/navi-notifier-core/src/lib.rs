//! `navi-notifier-core`: the provider-agnostic core of navi.
//!
//! It defines the normalized [`model`] every provider maps into, the [`traits`]
//! ([`Source`](traits::Source), [`Destination`](traits::Destination),
//! [`StateStore`](traits::StateStore)) that providers implement, the [`rules`]
//! filter layer, and the [`engine`] that ties a poll into filtered, deduplicated
//! delivery. It has no knowledge of GitHub, Slack, SQLite, or async transport
//! details beyond the trait boundaries.

pub mod config;
pub mod engine;
pub mod error;
pub mod model;
pub mod rules;
pub mod traits;

pub use config::RuleConfig;
pub use engine::{Engine, EventOutcome, EventRecord, Route, RunReport};
pub use error::{DestinationError, SourceError, StateError};
pub use model::{
    Actor, Backfill, Event, EventKind, MergeQueueRemoval, PullRequest, Repo, ReviewState,
    ViewerRelationship,
};
pub use rules::{Decision, DropReason, FilterContext, RuleEngine};
pub use traits::{Destination, Source, StateStore};
