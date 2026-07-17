//! Error types shared across navi crates.

use thiserror::Error;

/// Errors a [`crate::traits::Source`] may raise while polling or diffing.
#[derive(Debug, Error)]
pub enum SourceError {
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("rate limited; retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("provider request failed: {0}")]
    Request(String),
    #[error("failed to parse provider payload: {0}")]
    Parse(String),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Other(#[from] anyhow_compat::BoxError),
}

/// Errors a [`crate::traits::Destination`] may raise while delivering.
#[derive(Debug, Error)]
pub enum DestinationError {
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("rate limited; retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("delivery failed: {0}")]
    Delivery(String),
    #[error(transparent)]
    Other(#[from] anyhow_compat::BoxError),
}

/// Errors from the [`crate::traits::StateStore`].
#[derive(Debug, Error)]
pub enum StateError {
    #[error("state backend error: {0}")]
    Backend(String),
    #[error("failed to (de)serialize state: {0}")]
    Serde(String),
}

/// A tiny local alias so trait impls can wrap arbitrary provider errors without
/// forcing a dependency on `anyhow` in `navi-notifier-core`'s public API.
pub mod anyhow_compat {
    pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
}
