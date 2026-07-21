//! Live end-to-end smoke test against the real GitHub and Slack APIs.
//!
//! navi keys off *your* GitHub notifications, so a "cause an event, assert the DM"
//! loop would need a second account to act on you. Instead this proves the two
//! things CI's mock tests can't: that real credentials authenticate and that the
//! real request/response shapes still parse. It:
//!   1. builds the GitHub source and runs one real `poll()` (fresh state derives
//!      only your outstanding review requests, usually zero), proving auth and that
//!      the live notification/PR payloads deserialize.
//!   2. verifies the Slack bot token and sends a real DM to the configured target.
//!
//! Gated behind the `e2e` feature so it never ships. Invoked by
//! `.github/workflows/e2e.yml`.
//!
//! Env:
//!   NAVI_GITHUB_TOKEN   GitHub PAT (notifications + repo read)
//!   NAVI_SLACK_TOKEN    Slack bot token (chat:write + im:write)
//!   NAVI_SLACK_DM_TO    DM target: "self" (default), a user id, or #channel

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use navi_notifier_core::model::{
    Actor, Event, EventKind, PullRequest, Repo, ReviewState, ViewerRelationship,
};
use navi_notifier_core::traits::{Destination, Source, StateStore};
use navi_notifier_core::StateError;
use navi_notifier_github::{GitHubSource, GitHubSourceConfig};
use navi_notifier_slack::{SlackDestination, SlackDestinationConfig};
use time::OffsetDateTime;

#[tokio::main]
async fn main() {
    match run().await {
        Ok(()) => println!("e2e: PASSED"),
        Err(error) => {
            eprintln!("e2e: FAILED: {error}");
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<(), String> {
    let github_token = env("NAVI_GITHUB_TOKEN")?;
    let slack_token = env("NAVI_SLACK_TOKEN")?;
    let dm_to = std::env::var("NAVI_SLACK_DM_TO").unwrap_or_else(|_| "self".into());

    // 1. GitHub: one real poll against the live notifications API.
    println!("e2e: polling GitHub notifications…");
    let source = GitHubSource::new(GitHubSourceConfig {
        token: github_token,
        api_base: None,
        // Keep this smoke test scoped to the notifications path.
        track_prs: false,
        mark_read: false,
        comment_min_age_secs: 0,
    })
    .map_err(|e| format!("building GitHub source: {e}"))?;
    let state = MemState::default();
    let events = source
        .poll(&state)
        .await
        .map_err(|e| format!("GitHub poll failed: {e}"))?;
    println!(
        "e2e: GitHub OK: derived {} event(s) on first poll",
        events.len()
    );

    // 2. Slack: verify credentials and deliver a real DM.
    println!("e2e: verifying Slack + sending a DM to {dm_to}…");
    let destination = SlackDestination::new(SlackDestinationConfig {
        token: slack_token,
        dm_to,
        api_base: None,
    })
    .map_err(|e| format!("building Slack destination: {e}"))?;
    let who = destination
        .verify()
        .await
        .map_err(|e| format!("Slack auth.test failed: {e}"))?;
    println!("e2e: Slack authenticated as {who}");
    destination
        .send(&sample_event())
        .await
        .map_err(|e| format!("Slack delivery failed: {e}"))?;
    println!("e2e: Slack OK: sample DM delivered");

    Ok(())
}

fn env(key: &str) -> Result<String, String> {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(format!("missing or empty env var {key}")),
    }
}

fn sample_event() -> Event {
    Event {
        source_id: "github".into(),
        kind: EventKind::ReviewSubmitted {
            state: ReviewState::Approved,
        },
        pull_request: PullRequest {
            repo: Repo::new("navi", "e2e"),
            number: 1,
            title: "navi e2e smoke test".into(),
            url: "https://github.com/navi/e2e/pull/1".into(),
            author: Actor::new("navi-e2e"),
            draft: false,
        },
        viewer: ViewerRelationship {
            is_author: true,
            is_reviewer: false,
            actor_is_viewer: false,
        },
        actor: Actor::new("navi-e2e"),
        occurred_at: OffsetDateTime::now_utc(),
        target_url: Some("https://github.com/navi/e2e/pull/1".into()),
        excerpt: Some("If you can read this, the navi e2e run reached Slack. ✅".into()),
        dedup_key: format!("navi:e2e:{}", std::process::id()),
    }
}

/// Throwaway in-memory state so the poll has somewhere to read/write snapshots.
#[derive(Default)]
struct MemState {
    snapshots: Mutex<HashMap<String, Vec<u8>>>,
    delivered: Mutex<HashMap<String, ()>>,
    cursors: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl StateStore for MemState {
    async fn get_snapshot(&self, s: &str, scope: &str) -> Result<Option<Vec<u8>>, StateError> {
        Ok(self.snapshots.lock().unwrap().get(&key(s, scope)).cloned())
    }
    async fn put_snapshot(&self, s: &str, scope: &str, b: &[u8]) -> Result<(), StateError> {
        self.snapshots
            .lock()
            .unwrap()
            .insert(key(s, scope), b.to_vec());
        Ok(())
    }
    async fn was_delivered(&self, k: &str) -> Result<bool, StateError> {
        Ok(self.delivered.lock().unwrap().contains_key(k))
    }
    async fn mark_delivered(&self, k: &str) -> Result<(), StateError> {
        self.delivered.lock().unwrap().insert(k.to_string(), ());
        Ok(())
    }
    async fn get_cursor(&self, s: &str, k: &str) -> Result<Option<String>, StateError> {
        Ok(self.cursors.lock().unwrap().get(&key(s, k)).cloned())
    }
    async fn put_cursor(&self, s: &str, k: &str, v: &str) -> Result<(), StateError> {
        self.cursors
            .lock()
            .unwrap()
            .insert(key(s, k), v.to_string());
        Ok(())
    }
}

fn key(a: &str, b: &str) -> String {
    format!("{a}:{b}")
}
