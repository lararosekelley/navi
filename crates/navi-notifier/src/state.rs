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
    #[cfg(test)]
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
        migrate_delivered_to_per_sink(&conn)?;
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
    dedup_key    TEXT NOT NULL,
    sink         TEXT NOT NULL,
    delivered_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (dedup_key, sink)
);

CREATE TABLE IF NOT EXISTS cursors (
    source_id TEXT NOT NULL,
    key       TEXT NOT NULL,
    value     TEXT NOT NULL,
    PRIMARY KEY (source_id, key)
);
"#;

/// Sink recorded for rows written before delivery was tracked per sink. Matched by
/// every lookup, so an upgraded database treats its history as "delivered
/// everywhere" and does not re-notify.
///
/// Safe as a sentinel because real sinks are a closed set of fixed literals: the
/// destination ids in [`crate::config::DESTINATION_IDS`], plus the engine's
/// `__`-wrapped buffer sinks. `sentinel_cannot_be_a_destination_id` pins that.
const ANY_SINK: &str = "*";

/// Widen `delivered` from one row per event to one row per (event, sink).
///
/// Pre-0.3.4 databases have `dedup_key` as the sole primary key, so the table has to
/// be rebuilt rather than altered. Existing rows carry [`ANY_SINK`]: they record
/// that the event was delivered to every destination routed at the time, which is
/// what the old single-key semantics meant. Anything else would re-notify the whole
/// dedup history on first run after an upgrade.
fn migrate_delivered_to_per_sink(conn: &Connection) -> Result<(), StateError> {
    let has_sink = conn
        .prepare("SELECT 1 FROM pragma_table_info('delivered') WHERE name = 'sink'")
        .and_then(|mut stmt| stmt.exists([]))
        .map_err(|e| StateError::Backend(format!("inspecting delivered: {e}")))?;
    if has_sink {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "BEGIN;
         CREATE TABLE delivered_new (
             dedup_key    TEXT NOT NULL,
             sink         TEXT NOT NULL,
             delivered_at TEXT NOT NULL DEFAULT (datetime('now')),
             PRIMARY KEY (dedup_key, sink)
         );
         INSERT INTO delivered_new (dedup_key, sink, delivered_at)
             SELECT dedup_key, '{ANY_SINK}', delivered_at FROM delivered;
         DROP TABLE delivered;
         ALTER TABLE delivered_new RENAME TO delivered;
         COMMIT;"
    ))
    .map_err(|e| StateError::Backend(format!("migrating delivered: {e}")))?;
    Ok(())
}

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

    async fn was_delivered(&self, dedup_key: &str, sink: &str) -> Result<bool, StateError> {
        let conn = self.conn.clone();
        let (dedup_key, sink) = (dedup_key.to_string(), sink.to_string());
        spawn_blocking(move || {
            let c = lock(&conn)?;
            let found: Option<i64> = c
                .query_row(
                    // ANY_SINK covers pre-migration rows, which stand for every sink.
                    "SELECT 1 FROM delivered
                     WHERE dedup_key = ?1 AND sink IN (?2, ?3)",
                    params![dedup_key, sink, ANY_SINK],
                    |row| row.get(0),
                )
                .optional()
                .map_err(backend)?;
            Ok(found.is_some())
        })
        .await
        .map_err(join)?
    }

    async fn was_delivered_exact(&self, dedup_key: &str, sink: &str) -> Result<bool, StateError> {
        let conn = self.conn.clone();
        let (dedup_key, sink) = (dedup_key.to_string(), sink.to_string());
        spawn_blocking(move || {
            let c = lock(&conn)?;
            let found: Option<i64> = c
                .query_row(
                    // Deliberately without ANY_SINK: see the trait's doc comment.
                    "SELECT 1 FROM delivered WHERE dedup_key = ?1 AND sink = ?2",
                    params![dedup_key, sink],
                    |row| row.get(0),
                )
                .optional()
                .map_err(backend)?;
            Ok(found.is_some())
        })
        .await
        .map_err(join)?
    }

    async fn mark_delivered(&self, dedup_key: &str, sink: &str) -> Result<(), StateError> {
        let conn = self.conn.clone();
        let (dedup_key, sink) = (dedup_key.to_string(), sink.to_string());
        spawn_blocking(move || {
            let c = lock(&conn)?;
            c.execute(
                "INSERT INTO delivered (dedup_key, sink) VALUES (?1, ?2)
                 ON CONFLICT(dedup_key, sink) DO NOTHING",
                params![dedup_key, sink],
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
    async fn dedup_is_idempotent_per_sink() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(!store.was_delivered("k1", "slack").await.unwrap());
        store.mark_delivered("k1", "slack").await.unwrap();
        assert!(store.was_delivered("k1", "slack").await.unwrap());
        // Marking twice must not error.
        store.mark_delivered("k1", "slack").await.unwrap();
        assert!(store.was_delivered("k1", "slack").await.unwrap());
        // Unrelated key is unaffected.
        assert!(!store.was_delivered("k2", "slack").await.unwrap());
    }

    /// The point of the pair key: one destination taking an event says nothing
    /// about the others, so a retry can reach the ones that failed.
    #[tokio::test]
    async fn sinks_are_tracked_independently() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.mark_delivered("k1", "slack").await.unwrap();
        assert!(store.was_delivered("k1", "slack").await.unwrap());
        assert!(!store.was_delivered("k1", "email").await.unwrap());

        store.mark_delivered("k1", "email").await.unwrap();
        assert!(store.was_delivered("k1", "slack").await.unwrap());
        assert!(store.was_delivered("k1", "email").await.unwrap());
    }

    /// [`ANY_SINK`] means "every destination" on lookup, so a destination that could
    /// ever be named `*` would silently inherit another's dedup history.
    #[test]
    fn sentinel_cannot_be_a_destination_id() {
        assert!(!crate::config::DESTINATION_IDS.contains(&ANY_SINK));
    }

    /// The pre-0.3.4 `delivered` table: `dedup_key` alone as the primary key.
    const LEGACY_SCHEMA: &str = "CREATE TABLE delivered (
         dedup_key    TEXT PRIMARY KEY,
         delivered_at TEXT NOT NULL DEFAULT (datetime('now'))
     );";

    /// Upgrading must not re-notify. Rows written before delivery was tracked per
    /// sink stand for "delivered everywhere", so every sink sees them as done.
    #[tokio::test]
    async fn legacy_delivered_rows_survive_the_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("navi.db");

        // A database as an existing install would have left it.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO delivered (dedup_key, delivered_at) VALUES ('old', '2026-01-01 00:00:00')",
            [],
        )
        .unwrap();
        drop(conn);

        let store = SqliteStore::open(&path).unwrap();
        // The old event counts as delivered to every destination, named or not.
        assert!(store.was_delivered("old", "slack").await.unwrap());
        assert!(store.was_delivered("old", "email").await.unwrap());
        // Unrelated keys are still undelivered.
        assert!(!store.was_delivered("new", "slack").await.unwrap());

        // The migrated table takes per-sink writes, and keeps the old timestamp.
        store.mark_delivered("new", "slack").await.unwrap();
        assert!(store.was_delivered("new", "slack").await.unwrap());
        assert!(!store.was_delivered("new", "email").await.unwrap());
        let when: String = {
            let c = store.conn.lock().unwrap();
            c.query_row(
                "SELECT delivered_at FROM delivered WHERE dedup_key = 'old'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(when, "2026-01-01 00:00:00");
    }

    /// The two lookups must disagree on a migrated record: it stands for every sink
    /// when asking "should this be sent", and for none of them when asking "did this
    /// sink get it". Conflating them either re-notifies on upgrade or silently drops
    /// a buffered batch, depending on which way it collapses.
    #[tokio::test]
    async fn exact_lookup_ignores_the_migrated_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("navi.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        conn.execute("INSERT INTO delivered (dedup_key) VALUES ('old')", [])
            .unwrap();
        drop(conn);

        let store = SqliteStore::open(&path).unwrap();
        assert!(store.was_delivered("old", "slack").await.unwrap());
        assert!(!store.was_delivered_exact("old", "slack").await.unwrap());

        // A record written since the migration answers both the same way.
        store.mark_delivered("new", "slack").await.unwrap();
        assert!(store.was_delivered("new", "slack").await.unwrap());
        assert!(store.was_delivered_exact("new", "slack").await.unwrap());
        assert!(!store.was_delivered_exact("new", "email").await.unwrap());
    }

    /// Opening twice must not re-run the rebuild or lose rows.
    #[tokio::test]
    async fn migration_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("navi.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        conn.execute("INSERT INTO delivered (dedup_key) VALUES ('old')", [])
            .unwrap();
        drop(conn);

        let store = SqliteStore::open(&path).unwrap();
        store.mark_delivered("new", "slack").await.unwrap();
        drop(store);

        let store = SqliteStore::open(&path).unwrap();
        assert!(store.was_delivered("old", "slack").await.unwrap());
        assert!(store.was_delivered("new", "slack").await.unwrap());
        assert!(!store.was_delivered("new", "email").await.unwrap());
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
