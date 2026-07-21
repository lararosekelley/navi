//! `navi test`: exercise one provider without touching real state. `--destination`
//! sends a sample message; `--source` runs one real poll and prints what it derives.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use navi_notifier_core::model::{
    Actor, Event, EventKind, PullRequest, Repo, ReviewState, ViewerRelationship,
};
use navi_notifier_core::traits::StateStore;
use navi_notifier_core::StateError;
use time::OffsetDateTime;

use crate::config::Config;
use crate::wiring;

pub async fn run(
    config: &Config,
    source: Option<String>,
    destination: Option<String>,
) -> Result<()> {
    if source.is_none() && destination.is_none() {
        bail!("give at least one of --source <id> or --destination <id>");
    }

    if let Some(id) = destination {
        let dest = wiring::build_destination(config, &id)?;
        dest.send(&sample_event())
            .await
            .with_context(|| format!("sending a sample to `{id}`"))?;
        println!("sent a sample message to `{id}`");
    }

    if let Some(id) = source {
        let src = wiring::build_source(config, &id)?;
        // Ephemeral state so this preview leaves the real snapshots/cursors alone.
        let state = MemStore::default();
        let events = src
            .poll(&state)
            .await
            .with_context(|| format!("polling `{id}`"))?;
        println!("`{id}`: {} event(s) derived this poll:", events.len());
        for e in &events {
            println!(
                "  {} {}#{} by {}",
                e.kind.tag(),
                e.pull_request.repo.full_name(),
                e.pull_request.number,
                e.actor.login
            );
        }
    }
    Ok(())
}

fn sample_event() -> Event {
    Event {
        source_id: "github".into(),
        kind: EventKind::ReviewSubmitted {
            state: ReviewState::ChangesRequested,
        },
        pull_request: PullRequest {
            repo: Repo::new("acme", "widgets"),
            number: 42,
            title: "navi test message".into(),
            url: "https://github.com/acme/widgets/pull/42".into(),
            author: Actor::new("you"),
            draft: false,
        },
        viewer: ViewerRelationship {
            is_author: true,
            is_reviewer: false,
            actor_is_viewer: false,
        },
        actor: Actor::new("navi"),
        occurred_at: OffsetDateTime::now_utc(),
        target_url: Some("https://github.com/acme/widgets/pull/42".into()),
        excerpt: Some("If you can read this, navi can reach you. 🎉".into()),
        dedup_key: "navi:test".into(),
    }
}

/// Throwaway in-memory state store, so `navi test --source` doesn't advance any
/// real cursors or snapshots.
#[derive(Default)]
struct MemStore {
    snapshots: Mutex<HashMap<String, Vec<u8>>>,
    delivered: Mutex<HashMap<String, ()>>,
    cursors: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl StateStore for MemStore {
    async fn get_snapshot(&self, s: &str, scope: &str) -> Result<Option<Vec<u8>>, StateError> {
        Ok(self
            .snapshots
            .lock()
            .unwrap()
            .get(&format!("{s}:{scope}"))
            .cloned())
    }
    async fn put_snapshot(&self, s: &str, scope: &str, bytes: &[u8]) -> Result<(), StateError> {
        self.snapshots
            .lock()
            .unwrap()
            .insert(format!("{s}:{scope}"), bytes.to_vec());
        Ok(())
    }
    async fn was_delivered(&self, key: &str) -> Result<bool, StateError> {
        Ok(self.delivered.lock().unwrap().contains_key(key))
    }
    async fn mark_delivered(&self, key: &str) -> Result<(), StateError> {
        self.delivered.lock().unwrap().insert(key.to_string(), ());
        Ok(())
    }
    async fn get_cursor(&self, s: &str, key: &str) -> Result<Option<String>, StateError> {
        Ok(self
            .cursors
            .lock()
            .unwrap()
            .get(&format!("{s}:{key}"))
            .cloned())
    }
    async fn put_cursor(&self, s: &str, key: &str, value: &str) -> Result<(), StateError> {
        self.cursors
            .lock()
            .unwrap()
            .insert(format!("{s}:{key}"), value.to_string());
        Ok(())
    }
}
