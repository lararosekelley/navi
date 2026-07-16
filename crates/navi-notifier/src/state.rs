//! SQLite-backed [`StateStore`].
//!
//! Three tables mirror the trait's three jobs: `snapshots` (opaque per-PR bytes for
//! diffing), `delivered` (the dedup set), and `cursors` (poll bookkeeping like
//! ETags). rusqlite is synchronous, so every access runs on the blocking pool via
//! [`tokio::task::spawn_blocking`] over a shared `Arc<Mutex<Connection>>`. Local
//! SQLite calls are microsecond-scale, but this keeps them off the async reactor.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use navi_notifier_core::traits::StateStore;
use navi_notifier_core::StateError;
use rusqlite::{params, Connection, OptionalExtension};
use tokio::task::spawn_blocking;

pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// Open (creating if needed) the database at `path` and run migrations.
    pub fn open(path: &Path) -> Result<Self, StateError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StateError::Backend(format!("creating data dir: {e}")))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| StateError::Backend(format!("opening {}: {e}", path.display())))?;
        Self::from_connection(conn)
    }

    /// In-memory store, primarily for tests.
    #[allow(dead_code)]
    pub fn open_in_memory() -> Result<Self, StateError> {
        let conn = Connection::open_in_memory().map_err(|e| StateError::Backend(e.to_string()))?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, StateError> {
        // WAL keeps the single-writer daemon snappy alongside any read-only peeks.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| StateError::Backend(e.to_string()))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| StateError::Backend(format!("migrations: {e}")))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS snapshots (
    source_id  TEXT NOT NULL,
    scope      TEXT NOT NULL,
    bytes      BLOB NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (source_id, scope)
);

CREATE TABLE IF NOT EXISTS delivered (
    dedup_key    TEXT PRIMARY KEY,
    delivered_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS cursors (
    source_id TEXT NOT NULL,
    key       TEXT NOT NULL,
    value     TEXT NOT NULL,
    PRIMARY KEY (source_id, key)
);
"#;

/// Lock the connection, mapping a poisoned mutex to a backend error rather than
/// panicking the whole daemon.
fn lock(conn: &Mutex<Connection>) -> Result<std::sync::MutexGuard<'_, Connection>, StateError> {
    conn.lock()
        .map_err(|_| StateError::Backend("state mutex poisoned".into()))
}

fn backend<E: std::fmt::Display>(e: E) -> StateError {
    StateError::Backend(e.to_string())
}

fn join<E: std::fmt::Display>(e: E) -> StateError {
    StateError::Backend(format!("state task failed to join: {e}"))
}

#[async_trait]
impl StateStore for SqliteStore {
    async fn get_snapshot(
        &self,
        source_id: &str,
        scope: &str,
    ) -> Result<Option<Vec<u8>>, StateError> {
        let conn = self.conn.clone();
        let (source_id, scope) = (source_id.to_string(), scope.to_string());
        spawn_blocking(move || {
            let c = lock(&conn)?;
            c.query_row(
                "SELECT bytes FROM snapshots WHERE source_id = ?1 AND scope = ?2",
                params![source_id, scope],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(backend)
        })
        .await
        .map_err(join)?
    }

    async fn put_snapshot(
        &self,
        source_id: &str,
        scope: &str,
        bytes: &[u8],
    ) -> Result<(), StateError> {
        let conn = self.conn.clone();
        let (source_id, scope, bytes) = (source_id.to_string(), scope.to_string(), bytes.to_vec());
        spawn_blocking(move || {
            let c = lock(&conn)?;
            c.execute(
                "INSERT INTO snapshots (source_id, scope, bytes, updated_at)
                 VALUES (?1, ?2, ?3, datetime('now'))
                 ON CONFLICT(source_id, scope)
                 DO UPDATE SET bytes = excluded.bytes, updated_at = excluded.updated_at",
                params![source_id, scope, bytes],
            )
            .map(|_| ())
            .map_err(backend)
        })
        .await
        .map_err(join)?
    }

    async fn was_delivered(&self, dedup_key: &str) -> Result<bool, StateError> {
        let conn = self.conn.clone();
        let dedup_key = dedup_key.to_string();
        spawn_blocking(move || {
            let c = lock(&conn)?;
            let found: Option<i64> = c
                .query_row(
                    "SELECT 1 FROM delivered WHERE dedup_key = ?1",
                    params![dedup_key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(backend)?;
            Ok(found.is_some())
        })
        .await
        .map_err(join)?
    }

    async fn mark_delivered(&self, dedup_key: &str) -> Result<(), StateError> {
        let conn = self.conn.clone();
        let dedup_key = dedup_key.to_string();
        spawn_blocking(move || {
            let c = lock(&conn)?;
            c.execute(
                "INSERT INTO delivered (dedup_key) VALUES (?1)
                 ON CONFLICT(dedup_key) DO NOTHING",
                params![dedup_key],
            )
            .map(|_| ())
            .map_err(backend)
        })
        .await
        .map_err(join)?
    }

    async fn get_cursor(&self, source_id: &str, key: &str) -> Result<Option<String>, StateError> {
        let conn = self.conn.clone();
        let (source_id, key) = (source_id.to_string(), key.to_string());
        spawn_blocking(move || {
            let c = lock(&conn)?;
            c.query_row(
                "SELECT value FROM cursors WHERE source_id = ?1 AND key = ?2",
                params![source_id, key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(backend)
        })
        .await
        .map_err(join)?
    }

    async fn put_cursor(&self, source_id: &str, key: &str, value: &str) -> Result<(), StateError> {
        let conn = self.conn.clone();
        let (source_id, key, value) = (source_id.to_string(), key.to_string(), value.to_string());
        spawn_blocking(move || {
            let c = lock(&conn)?;
            c.execute(
                "INSERT INTO cursors (source_id, key, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT(source_id, key) DO UPDATE SET value = excluded.value",
                params![source_id, key, value],
            )
            .map(|_| ())
            .map_err(backend)
        })
        .await
        .map_err(join)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_round_trips_and_overwrites() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert_eq!(
            store.get_snapshot("github", "acme/w#1").await.unwrap(),
            None
        );

        store
            .put_snapshot("github", "acme/w#1", b"v1")
            .await
            .unwrap();
        assert_eq!(
            store.get_snapshot("github", "acme/w#1").await.unwrap(),
            Some(b"v1".to_vec())
        );

        store
            .put_snapshot("github", "acme/w#1", b"v2")
            .await
            .unwrap();
        assert_eq!(
            store.get_snapshot("github", "acme/w#1").await.unwrap(),
            Some(b"v2".to_vec())
        );
    }

    #[tokio::test]
    async fn dedup_is_idempotent() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(!store.was_delivered("k1").await.unwrap());
        store.mark_delivered("k1").await.unwrap();
        assert!(store.was_delivered("k1").await.unwrap());
        // Marking twice must not error.
        store.mark_delivered("k1").await.unwrap();
        assert!(store.was_delivered("k1").await.unwrap());
        // Unrelated key is unaffected.
        assert!(!store.was_delivered("k2").await.unwrap());
    }

    #[tokio::test]
    async fn cursors_round_trip_and_update() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert_eq!(store.get_cursor("github", "etag").await.unwrap(), None);
        store.put_cursor("github", "etag", "abc").await.unwrap();
        assert_eq!(
            store.get_cursor("github", "etag").await.unwrap(),
            Some("abc".to_string())
        );
        store.put_cursor("github", "etag", "def").await.unwrap();
        assert_eq!(
            store.get_cursor("github", "etag").await.unwrap(),
            Some("def".to_string())
        );
    }
}
