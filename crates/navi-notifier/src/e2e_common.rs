//! Shared helpers for the live e2e harnesses under `src/bin/navi-e2e-*`.
//!
//! Not a crate module: this file is `#[path]`-included by each e2e binary (all gated
//! behind the `e2e` feature), so the `env`/`json_ok`/`MemState` boilerplate lives in
//! one place instead of once per binary. It is never compiled into the `navi` binary.

// Each binary includes the whole module but uses a subset (e.g. Slack has its own
// `ok`-envelope parser and never calls `json_ok`), so unused-in-one-bin is expected.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use navi_notifier_core::traits::StateStore;
use navi_notifier_core::StateError;
use navi_notifier_email::{EmailDestination, EmailDestinationConfig, EmailTls};
use serde_json::Value;

/// The Mailpit email destination every source slice delivers into (unencrypted local
/// SMTP; `navi.local` addresses are cosmetic).
pub fn mailpit_email(smtp_host: String, smtp_port: u16) -> Result<EmailDestination, String> {
    EmailDestination::new(EmailDestinationConfig {
        smtp_host,
        smtp_port,
        tls: EmailTls::None,
        username: None,
        password: None,
        from: "navi <navi@navi.local>".into(),
        to: "you <you@navi.local>".into(),
    })
    .map_err(|e| format!("build email destination: {e}"))
}

/// Find a "requested your review" subject in Mailpit's message list. With
/// `expect = Some(marker)` only a subject containing that marker matches (so a live
/// viewer's unrelated real reviews can't pass the test); `None` matches any.
pub async fn mailpit_review_request(
    http: &reqwest::Client,
    mailpit: &str,
    expect: Option<&str>,
) -> Result<Option<String>, String> {
    let resp = http
        .get(format!("{mailpit}/api/v1/messages"))
        .send()
        .await
        .map_err(|e| format!("mailpit query: {e}"))?;
    let value = json_ok(resp, "mailpit query").await?;
    let found = value["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["Subject"].as_str())
        .find(|s| s.contains("requested your review") && expect.is_none_or(|e| s.contains(e)))
        .map(str::to_string);
    Ok(found)
}

/// A required env var, or an error naming it.
pub fn env(key: &str) -> Result<String, String> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("missing env var {key}"))
}

/// An env var, or `default` when unset/blank.
pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Read an HTTP response as JSON, turning a non-2xx status into an error that
/// includes the body. `what` labels the call in the message.
pub async fn json_ok(resp: reqwest::Response, what: &str) -> Result<Value, String> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("{what}: read body: {e}"))?;
    if !status.is_success() {
        return Err(format!("{what}: {status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("{what}: parse: {e}"))
}

/// In-memory [`StateStore`] so a poll/delivery has somewhere to keep snapshots,
/// dedup marks, and cursors for the duration of one e2e run. A fresh instance never
/// reports a prior delivery or a thread parent, which is what these one-shot tests
/// want.
#[derive(Default)]
pub struct MemState {
    snapshots: Mutex<HashMap<String, Vec<u8>>>,
    delivered: Mutex<HashSet<String>>,
    cursors: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl StateStore for MemState {
    async fn get_snapshot(&self, s: &str, scope: &str) -> Result<Option<Vec<u8>>, StateError> {
        Ok(self.snapshots.lock().unwrap().get(&k(s, scope)).cloned())
    }
    async fn put_snapshot(&self, s: &str, scope: &str, b: &[u8]) -> Result<(), StateError> {
        self.snapshots
            .lock()
            .unwrap()
            .insert(k(s, scope), b.to_vec());
        Ok(())
    }
    async fn was_delivered_exact(&self, key: &str, sink: &str) -> Result<bool, StateError> {
        self.was_delivered(key, sink).await
    }
    async fn was_delivered(&self, key: &str, sink: &str) -> Result<bool, StateError> {
        Ok(self
            .delivered
            .lock()
            .unwrap()
            .contains(&format!("{key}@{sink}")))
    }
    async fn mark_delivered(&self, key: &str, sink: &str) -> Result<(), StateError> {
        self.delivered
            .lock()
            .unwrap()
            .insert(format!("{key}@{sink}"));
        Ok(())
    }
    async fn get_cursor(&self, s: &str, key: &str) -> Result<Option<String>, StateError> {
        Ok(self.cursors.lock().unwrap().get(&k(s, key)).cloned())
    }
    async fn put_cursor(&self, s: &str, key: &str, v: &str) -> Result<(), StateError> {
        self.cursors
            .lock()
            .unwrap()
            .insert(k(s, key), v.to_string());
        Ok(())
    }
}

fn k(a: &str, b: &str) -> String {
    format!("{a}\u{0}{b}")
}
