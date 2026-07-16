//! Per-PR state we persist between polls so the next poll can diff against it.
//!
//! Stored as JSON bytes in the [`StateStore`](navi_notifier_core::traits::StateStore) under
//! the scope `"{owner}/{repo}#{number}"`. Everything here is data the diff needs to
//! decide "what changed since last time".

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrSnapshot {
    /// Review ids we've already turned into events, mapped to the state we last saw
    /// them in — so a review flipping to `DISMISSED` is detectable.
    #[serde(default)]
    pub seen_reviews: BTreeMap<u64, String>,
    /// Inline review comment ids already emitted.
    #[serde(default)]
    pub seen_review_comments: BTreeSet<u64>,
    /// Conversation comment ids already emitted.
    #[serde(default)]
    pub seen_issue_comments: BTreeSet<u64>,
    /// Whether the viewer was a requested reviewer at last poll (edge-detects new requests).
    #[serde(default)]
    pub viewer_requested: bool,
    /// Whether the viewer had submitted any review before (distinguishes review vs. re-review request).
    #[serde(default)]
    pub viewer_reviewed: bool,
    /// Last-seen PR lifecycle so merge/close/ready transitions fire exactly once.
    #[serde(default)]
    pub merged: bool,
    #[serde(default)]
    pub closed: bool,
    #[serde(default)]
    pub draft: bool,
    /// True once we've recorded an initial observation; the first-ever sighting of a
    /// PR must NOT retroactively emit events for pre-existing history.
    #[serde(default)]
    pub initialized: bool,
}
