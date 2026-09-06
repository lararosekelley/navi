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
use tracing::warn;

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

/// How many rows a [`SqliteStore::prune`] pass removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pruned {
    pub cursors: u64,
}

impl SqliteStore {
    /// Drop poll cursors for pull requests that have been quiet for
    /// `retention_days`, so they stop accumulating one row per PR the daemon has
    /// ever seen. The database still grows per PR through `snapshots`, which this
    /// deliberately leaves alone. `0` disables it. Run once a day by the `run`
    /// daemon only; `once` polls and exits without sweeping.
    ///
    /// Only cursors, and only these two kinds. Both are safe to lose because the
    /// snapshot, which is *not* pruned, is what actually suppresses re-notification:
    ///
    /// - `pr:{scope}` gates the involved-PR sweep on the PR's `updated_at`. Without
    ///   it the PR is diffed once more against its stored snapshot. That yields no
    ///   events whenever the snapshot is level with the PR, which is every row except
    ///   the ones described under "the one case that alerts" below.
    /// - `thread:{scope}` gates notification re-processing the same way, and the
    ///   source already notes the snapshot would suppress any duplicate.
    ///
    /// `mq:{scope}` is deliberately **not** swept, though it is per-PR and grows the
    /// same way. It holds the last-seen merge-queue state, and it is the one kind
    /// whose loss costs an event rather than an API call: a missing prior state
    /// baselines instead of firing, and GitHub does not always bump a PR's
    /// `updated_at` on a queue change (`github/src/source.rs:499`), so a PR quiet by
    /// every dated cursor can still have a live queue. Dating it by association with
    /// those cursors would swallow a transition that landed while the baseline was
    /// missing. Giving the value its own stamp, the way `mqcfg:` does, would not help
    /// either: `merge_queue_event` runs inside `process_pr`, so the state is only
    /// observed when the PR is diffed, which is the same gate the sibling cursors
    /// use. That was tried and rejected in #161.
    ///
    /// It is one row per merge-queue PR, not per repo, so it does grow without bound,
    /// just slowly, and a missed queue transition is worse than the row. The shape
    /// that would be safe is sweeping only PRs the snapshot records as merged or
    /// closed, since a settled PR cannot enter a queue; that needs the store to read
    /// the snapshot it currently treats as opaque bytes.
    ///
    /// ## What this actually reclaims
    ///
    /// Permanently, for a PR that has settled: the closed-PR sweep is bounded by
    /// `updated:>=`, and old notifications fall behind the `notif_since` watermark,
    /// so neither comes back and the rows stay gone.
    ///
    /// Once per retention period, for a PR still open: `involved_open_prs` is
    /// `is:open is:pr involves:{viewer}` with no date bound, so a quiet open PR keeps
    /// being returned. Deleting its cursor costs one re-diff (a few REST calls, no
    /// events), and that re-diff refreshes `snapshots.updated_at`, which is what this
    /// dates off, so the row is not eligible again until another full period passes.
    /// On a measured install about three quarters of stale `pr:` cursors belonged to
    /// settled PRs, which are never re-diffed and so never come back at all.
    ///
    /// ## The one case that alerts
    ///
    /// A cursor can sit *ahead* of its snapshot. The sources no longer create that
    /// state (#162: a cursor only advances past a PR that was actually diffed), but
    /// databases written before that fix still hold such rows, left by a failed PR
    /// fetch on a PR that had just changed: the cursor holds the new timestamp while
    /// the snapshot holds the pre-change state. They resolve themselves the first
    /// time the PR is diffed successfully.
    ///
    /// For those rows the re-diff is against a snapshot that is behind rather than
    /// level, so it emits everything accumulated since the last successful diff, with
    /// the original `occurred_at`. That is a late alert for activity that was never
    /// delivered, not a duplicate and not a first sight (`old.initialized` is still
    /// true, so no back-fill runs) - arguably #162 repairing itself on a timer. It is
    /// the only path here that produces an alert at all, which is why the guarantee
    /// elsewhere is worded as "never a duplicate" rather than "never an alert".
    ///
    /// `snapshots` and `delivered` are left alone. Snapshots are the one table where
    /// eviction *can* re-notify: a first sight re-emits outstanding review requests,
    /// and the review-request dedup key is salted with the PR's `updated_at`, so the
    /// stored key can never match the re-derived one. They gain the fewest rows per
    /// day of the three tables, though the largest rows, and `seen_issue_comments`
    /// and friends grow with a PR's discussion while a cursor row stays fixed-size.
    /// Both tables remain deliberately retained and unbounded.
    ///
    /// Destination-side `thread:{source}:{scope}` cursors (Slack and Discord message
    /// ids, used to group a PR's alerts into one thread) are not pruned yet. They
    /// could be, by rebuilding the key forward from the `snapshots` row rather than
    /// by interpreting the value. Anchor on the snapshot, not on a `pr:` cursor: this
    /// sweep deletes those, permanently for the settled majority, so anything keyed
    /// off them would miss every destination cursor whose sibling an earlier release
    /// already swept.
    pub async fn prune(&self, retention_days: u32) -> Result<Pruned, StateError> {
        if retention_days == 0 {
            return Ok(Pruned::default());
        }
        let conn = self.conn.clone();
        spawn_blocking(move || {
            let cutoff = format!("-{retention_days} days");
            let c = lock(&conn)?;
            let tx = c.unchecked_transaction().map_err(backend)?;

            // Two guards, one per assumption the sweep makes.
            //
            // The key guard: a destination's own thread cursor is
            // `thread:{source}:{scope}` (`core::Event::thread_key`), so it carries a
            // second colon that a source's `thread:{scope}` never does. Those belong
            // to Slack and Discord, group a PR's alerts into one message thread, and
            // are not this sweep's business.
            //
            // The snapshot guard carries the safety argument *and* the schedule. It
            // has to exist at all because "the PR re-diffs against its unchanged
            // snapshot and emits nothing" is only true when there is a snapshot. A
            // source advances the `pr:` cursor for a PR the forge reports as
            // permanently gone without ever staging one (`PrOutcome::Gone`), and
            // databases written before #162 hold the same shape from any failed
            // fetch, so these rows are real. Dropping one would turn the next
            // sighting into a first sight, which re-emits an outstanding review
            // request under a dedup key `delivered` has never seen, since the key
            // is salted with the PR's `updated_at`.
            //
            // Dating off `snapshots.updated_at` rather than off the cursor's own
            // value is what keeps that re-diff from repeating. `put_snapshot`
            // refreshes the column on every upsert, so it records when navi last
            // actually diffed the PR, which is not the same as when the PR last
            // changed. A quiet but still-open PR is returned by every involved-PR
            // sweep (`is:open is:pr involves:{viewer}` has no date bound), so
            // deleting its cursor causes one re-diff - and that re-diff writes a
            // fresh `updated_at`, holding the row for another full retention period
            // instead of it being deleted again tomorrow.
            let dated = tx
                .execute(
                    "DELETE FROM cursors
                      WHERE (key LIKE 'pr:%'
                             OR (key LIKE 'thread:%' AND instr(substr(key, 8), ':') = 0))
                        AND EXISTS (
                          SELECT 1 FROM snapshots s
                           WHERE s.source_id = cursors.source_id
                             AND s.scope = CASE
                                   WHEN cursors.key LIKE 'pr:%' THEN substr(cursors.key, 4)
                                   ELSE substr(cursors.key, 8)
                                 END
                             AND s.updated_at < datetime('now', ?1))",
                    params![cutoff],
                )
                .map_err(backend)? as u64;

            tx.commit().map_err(backend)?;
            let pruned = Pruned { cursors: dated };
            // Deleting leaves the pages allocated, so the file only shrinks on a
            // VACUUM. It rewrites the whole database, which at this size (single
            // digit MB) is cheap enough to run on any day that removed something,
            // and PRs keep settling past the horizon, so that is most days.
            if pruned.cursors > 0 {
                // The delete is already committed, so a VACUUM failure costs disk,
                // not correctness. Reporting it as a failed prune would understate
                // what actually happened.
                if let Err(err) = c.execute_batch("VACUUM") {
                    warn!(%err, "pruned state but could not compact the database");
                }
            }
            Ok(pruned)
        })
        .await
        .map_err(join)?
    }
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

    /// Write a cursor directly, so a value can be dated into the past.
    fn put_raw(store: &SqliteStore, source: &str, key: &str, value: &str) {
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO cursors (source_id, key, value) VALUES (?1, ?2, ?3)",
                params![source, key, value],
            )
            .unwrap();
    }

    fn cursor_keys(store: &SqliteStore) -> Vec<String> {
        let c = store.conn.lock().unwrap();
        let mut stmt = c
            .prepare("SELECT source_id || '|' || key FROM cursors ORDER BY 1")
            .unwrap();
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    }

    /// A realistic cursor table: one long-quiet PR and one touched today, each with
    /// the full set of per-PR bookkeeping, plus the global and per-repo rows.
    /// Give a scope a snapshot last diffed `days` ago. The sweep dates off this,
    /// not off the cursor's value, so this is what decides eligibility.
    fn put_snapshot_aged(store: &SqliteStore, source: &str, scope: &str, days: u32) {
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO snapshots (source_id, scope, bytes, updated_at)
                 VALUES (?1, ?2, x'7b7d', datetime('now', ?3))",
                params![source, scope, format!("-{days} days")],
            )
            .unwrap();
    }

    /// Diffed just now: never eligible.
    fn put_snapshot_fresh(store: &SqliteStore, source: &str, scope: &str) {
        put_snapshot_aged(store, source, scope, 0);
    }

    fn seeded_cursors() -> SqliteStore {
        let store = SqliteStore::open_in_memory().unwrap();
        put_snapshot_aged(&store, "github", "acme/w#1", 400);
        put_snapshot_fresh(&store, "github", "acme/w#2");
        put_raw(&store, "github", "pr:acme/w#1", "2020-01-01T00:00:00Z");
        put_raw(&store, "github", "thread:acme/w#1", "2020-01-01T00:00:00Z");
        put_raw(&store, "github", "mq:acme/w#1", "absent");
        put_raw(&store, "github", "pr:acme/w#2", "2999-01-01T00:00:00Z");
        put_raw(&store, "github", "thread:acme/w#2", "2999-01-01T00:00:00Z");
        put_raw(&store, "github", "mq:acme/w#2", "queued");
        // Not per-PR: a global watermark and a per-repo capability flag.
        put_raw(&store, "github", "notif_since", "2020-01-01T00:00:00Z");
        put_raw(&store, "github", "mqcfg:acme/w", "true");
        // Slack's own thread cursor shares the `thread:` prefix but holds a Slack
        // `ts`, which sorts before any RFC3339 date.
        put_raw(
            &store,
            "slack",
            "thread:github:acme/w#1",
            "1750000000.123456",
        );
        store
    }

    #[tokio::test]
    async fn prune_drops_per_pr_cursors_for_long_quiet_prs() {
        let store = seeded_cursors();
        assert_eq!(store.prune(90).await.unwrap().cursors, 2);
        assert_eq!(
            cursor_keys(&store),
            vec![
                "github|mq:acme/w#1",
                "github|mq:acme/w#2",
                "github|mqcfg:acme/w",
                "github|notif_since",
                "github|pr:acme/w#2",
                "github|thread:acme/w#2",
                "slack|thread:github:acme/w#1",
            ]
        );
    }

    /// A global watermark is a cursor with a dated value too, and deleting it would
    /// force a full re-poll.
    #[tokio::test]
    async fn prune_leaves_global_and_per_repo_cursors() {
        let store = SqliteStore::open_in_memory().unwrap();
        put_raw(&store, "github", "notif_since", "2020-01-01T00:00:00Z");
        put_raw(&store, "github", "pr_closed_since", "2020-01-01T00:00:00Z");
        put_raw(&store, "github", "backfilled", "2020-01-01T00:00:00Z");
        put_raw(&store, "github", "mqcfg:acme/w", "true");
        assert_eq!(store.prune(90).await.unwrap(), Pruned::default());
        assert_eq!(cursor_keys(&store).len(), 4);
    }

    /// `mq:` is never swept, even when every dated cursor for its PR is stale.
    /// GitHub does not always bump `updated_at` on a queue change, so "quiet by the
    /// dated cursors" does not mean "quiet queue", and a missing baseline swallows a
    /// transition rather than duplicating one.
    #[tokio::test]
    async fn prune_never_touches_merge_queue_state() {
        let store = SqliteStore::open_in_memory().unwrap();
        put_snapshot_aged(&store, "github", "acme/w#1", 400);
        put_raw(&store, "github", "pr:acme/w#1", "2020-01-01T00:00:00Z");
        put_raw(&store, "github", "thread:acme/w#1", "2020-01-01T00:00:00Z");
        put_raw(&store, "github", "mq:acme/w#1", "queued");

        assert_eq!(store.prune(90).await.unwrap().cursors, 2);
        assert_eq!(
            cursor_keys(&store),
            vec!["github|mq:acme/w#1"],
            "the merge-queue baseline is the one kind whose loss costs an event"
        );
    }

    /// The exclusion has to come from the key, not from `mq:` values happening not
    /// to look like dates. Sweeping it needs a self-dating value (the `mqcfg:`
    /// pattern), and that change must not quietly enrol these rows in the sweep.
    #[tokio::test]
    async fn merge_queue_state_is_excluded_by_key_not_by_its_value() {
        let store = SqliteStore::open_in_memory().unwrap();
        put_snapshot_aged(&store, "github", "acme/w#1", 400);
        put_raw(&store, "github", "pr:acme/w#1", "2020-01-01T00:00:00Z");
        // A value in the shape the follow-up would give it.
        put_raw(&store, "github", "mq:acme/w#1", "2020-01-01T00:00:00Z");

        assert_eq!(store.prune(90).await.unwrap().cursors, 1);
        assert_eq!(cursor_keys(&store), vec!["github|mq:acme/w#1"]);
    }

    /// The safety argument is "the PR re-diffs against its unchanged snapshot and
    /// emits nothing", which needs a snapshot to exist. A source stages the `pr:`
    /// cursor even when the PR fetch failed, so these rows are real: five of them on
    /// the install this PR was measured against. Dropping one makes the next sighting
    /// a first sight, which re-emits an outstanding review request under a dedup key
    /// `delivered` has never seen.
    #[tokio::test]
    async fn prune_keeps_cursors_whose_snapshot_is_missing() {
        let store = SqliteStore::open_in_memory().unwrap();
        // Cursor staged by a failed fetch: no snapshot was ever written.
        put_raw(&store, "github", "pr:acme/w#1", "2020-01-01T00:00:00Z");
        put_raw(&store, "github", "thread:acme/w#1", "2020-01-01T00:00:00Z");
        // The same pair for a PR that was diffed successfully.
        put_snapshot_aged(&store, "github", "acme/w#2", 400);
        put_raw(&store, "github", "pr:acme/w#2", "2020-01-01T00:00:00Z");
        put_raw(&store, "github", "thread:acme/w#2", "2020-01-01T00:00:00Z");

        assert_eq!(store.prune(90).await.unwrap().cursors, 2);
        assert_eq!(
            cursor_keys(&store),
            vec!["github|pr:acme/w#1", "github|thread:acme/w#1"],
            "without a snapshot to diff against, the cursor is the only thing \
             stopping a first sight"
        );
    }

    /// The sweep never reads the cursor's own value: the snapshot's age is what
    /// decides. That is what stops a still-open quiet PR from being re-diffed every
    /// day, since its value stays stale while the re-diff refreshes the snapshot.
    #[tokio::test]
    async fn eligibility_comes_from_the_snapshot_not_the_cursor_value() {
        let store = SqliteStore::open_in_memory().unwrap();
        // Stale value, freshly diffed: the re-diff has already happened, so this
        // must not be swept again.
        put_snapshot_fresh(&store, "github", "acme/w#1");
        put_raw(&store, "github", "pr:acme/w#1", "2020-01-01T00:00:00Z");
        // Value that is not a date at all, but long unvisited: still eligible.
        put_snapshot_aged(&store, "github", "acme/w#2", 400);
        put_raw(&store, "github", "pr:acme/w#2", "not-a-timestamp");

        assert_eq!(store.prune(90).await.unwrap().cursors, 1);
        assert_eq!(cursor_keys(&store), vec!["github|pr:acme/w#1"]);
    }

    /// The `EXISTS` is correlated on `source_id` as well as scope. GitHub and Gitea
    /// both scope as `owner/repo#n`, so without it one source's fresh snapshot would
    /// vouch for another source's stale cursor, or the reverse.
    #[tokio::test]
    async fn a_snapshot_only_vouches_for_its_own_source() {
        let store = SqliteStore::open_in_memory().unwrap();
        // Same scope, two sources: gitea long-quiet, github diffed just now.
        put_snapshot_aged(&store, "gitea", "acme/w#1", 400);
        put_snapshot_fresh(&store, "github", "acme/w#1");
        put_raw(&store, "gitea", "pr:acme/w#1", "2020-01-01T00:00:00Z");
        put_raw(&store, "github", "pr:acme/w#1", "2020-01-01T00:00:00Z");

        assert_eq!(store.prune(90).await.unwrap().cursors, 1);
        assert_eq!(
            cursor_keys(&store),
            vec!["github|pr:acme/w#1"],
            "github's fresh snapshot must not vouch for gitea's cursor, or vice versa"
        );
    }

    /// A destination's thread cursor is `thread:{source}:{scope}`, so the extra colon
    /// keeps it out even if its value ever started looking like a date.
    #[tokio::test]
    async fn prune_leaves_destination_thread_cursors() {
        let store = SqliteStore::open_in_memory().unwrap();
        put_snapshot_aged(&store, "github", "acme/w#1", 400);
        put_snapshot_aged(&store, "github", "acme/w#2", 400);
        // Contrived, so that only the key guard is left to save these rows: give the
        // destination namespaces the snapshots the other guard would otherwise miss.
        // Destinations never write snapshots, so this state cannot arise; the point
        // is to test one guard rather than two.
        put_snapshot_aged(&store, "slack", "github:acme/w#1", 400);
        put_snapshot_aged(&store, "discord", "github:acme/w#2", 400);
        // Give these a dated value
        // so only the key guard can save it.
        put_raw(
            &store,
            "slack",
            "thread:github:acme/w#1",
            "2020-01-01T00:00:00Z",
        );
        put_raw(
            &store,
            "discord",
            "thread:github:acme/w#2",
            "2020-01-01T00:00:00Z",
        );
        // The source's own cursor, same prefix, no second colon: this one goes.
        put_raw(&store, "github", "thread:acme/w#1", "2020-01-01T00:00:00Z");

        assert_eq!(store.prune(90).await.unwrap().cursors, 1);
        assert_eq!(
            cursor_keys(&store),
            vec![
                "discord|thread:github:acme/w#2",
                "slack|thread:github:acme/w#1"
            ]
        );
    }

    /// Pruning must never touch the two tables that make delivery exactly-once.
    #[tokio::test]
    async fn prune_never_touches_snapshots_or_delivered() {
        let store = seeded_cursors();
        store
            .put_snapshot("github", "acme/w#1", b"v1")
            .await
            .unwrap();
        store.mark_delivered("k1", "slack").await.unwrap();
        {
            let c = store.conn.lock().unwrap();
            c.execute(
                "UPDATE snapshots SET updated_at = '2020-01-01 00:00:00'",
                [],
            )
            .unwrap();
            c.execute(
                "UPDATE delivered SET delivered_at = '2020-01-01 00:00:00'",
                [],
            )
            .unwrap();
        }

        assert!(store.prune(90).await.unwrap().cursors > 0);
        assert_eq!(
            store.get_snapshot("github", "acme/w#1").await.unwrap(),
            Some(b"v1".to_vec()),
            "a snapshot is what suppresses a re-derived event; it must survive"
        );
        assert!(store.was_delivered("k1", "slack").await.unwrap());
    }

    #[tokio::test]
    async fn prune_is_disabled_at_zero_and_is_idempotent() {
        let store = seeded_cursors();
        assert_eq!(store.prune(0).await.unwrap(), Pruned::default());
        assert_eq!(cursor_keys(&store).len(), 9);

        assert_eq!(store.prune(90).await.unwrap().cursors, 2);
        assert_eq!(store.prune(90).await.unwrap(), Pruned::default());
        // Still usable after the VACUUM.
        store.put_cursor("github", "etag", "abc").await.unwrap();
        assert_eq!(
            store.get_cursor("github", "etag").await.unwrap(),
            Some("abc".to_string())
        );
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
