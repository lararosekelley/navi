//! Shared helpers for the GitHub source integration tests.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use navi_notifier_core::traits::StateStore;
use navi_notifier_core::StateError;

/// An in-memory [`StateStore`] so poll tests can run without SQLite or a daemon.
#[derive(Default)]
pub struct MemState {
    snapshots: Mutex<HashMap<String, Vec<u8>>>,
    delivered: Mutex<HashMap<String, ()>>,
    cursors: Mutex<HashMap<String, String>>,
}

fn key(a: &str, b: &str) -> String {
    format!("{a}\u{0}{b}")
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
