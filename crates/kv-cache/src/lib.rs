//! kwikvid content cache.
//!
//! Stores binary blobs (thumbnail JPEGs, probe results, HiLight tags, music
//! analysis, ...) in one SQLite file, keyed by:
//!
//! - the identity of the source file ([`SourceKey`]: path, size, mtime),
//! - a short `kind` string such as `"filmstrip"` or `"music-analysis"`,
//! - a `params` string such as `"n=12,h=90,v=1"`. Put an algorithm version in
//!   it so that a format change invalidates old entries.
//!
//! When the source file changes (different size or mtime), lookups miss and
//! the next [`Cache::put`] for that path and kind removes the stale rows.
//! [`Cache::prune`] keeps the total blob size under a budget by evicting the
//! least recently used entries.
//!
//! [`Cache`] is `Send + Sync`; share it between background workers with an
//! `Arc`. Where the cache file lives on disk is the caller's decision.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::UNIX_EPOCH;

use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;
use thiserror::Error;

/// Schema version written by this build. Bump it when the schema changes;
/// older databases are wiped on open (it is only a cache).
pub const SCHEMA_VERSION: i64 = 1;

/// Why a cache operation failed.
#[derive(Debug, Error)]
pub enum CacheError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// The database was written by a newer kwikvid with an unknown schema.
    #[error("cache schema version {found} is newer than supported version {supported}")]
    UnsupportedSchema { found: i64, supported: i64 },
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, CacheError>;

/// Identity of a source file: its path plus the size and modification time
/// seen when it was stat-ed. Content is not hashed: a file rewritten with the
/// same size and mtime is (wrongly) treated as unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceKey {
    pub path: PathBuf,
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch (negative before it).
    pub mtime_ns: i128,
}

impl SourceKey {
    /// Stat `path` and build its key. The path is canonicalised so that
    /// different spellings of the same file share cache entries.
    pub fn from_file(path: &Path) -> Result<Self> {
        let path = std::fs::canonicalize(path)?;
        let meta = std::fs::metadata(&path)?;
        let modified = meta.modified()?;
        let mtime_ns = match modified.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        };
        Ok(Self {
            path,
            size: meta.len(),
            mtime_ns,
        })
    }

    fn path_bytes(&self) -> &[u8] {
        self.path.as_os_str().as_encoded_bytes()
    }

    /// SQLite integers are 64-bit, so the i128 mtime is stored as text.
    fn mtime_text(&self) -> String {
        self.mtime_ns.to_string()
    }

    /// Sizes over i64::MAX cannot exist on real filesystems; saturate.
    fn size_i64(&self) -> i64 {
        i64::try_from(self.size).unwrap_or(i64::MAX)
    }
}

struct Inner {
    conn: Connection,
    /// Monotonic "last used" clock. A counter rather than wall time, so LRU
    /// order is exact even for accesses within the same clock tick.
    clock: i64,
}

/// A blob cache backed by one SQLite database.
pub struct Cache {
    inner: Mutex<Inner>,
}

impl Cache {
    /// Open (or create) the cache database at `path`, in WAL mode.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        // journal_mode returns a row, so it must be queried, not executed.
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::init(conn)
    }

    /// An in-memory cache, for tests.
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);",
        )?;
        let found: Option<i64> = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .optional()?
            .flatten();
        match found {
            Some(v) if v > SCHEMA_VERSION => {
                return Err(CacheError::UnsupportedSchema {
                    found: v,
                    supported: SCHEMA_VERSION,
                })
            }
            Some(v) if v == SCHEMA_VERSION => {}
            // Missing or older: it is only a cache, so wipe and recreate.
            _ => Self::create_schema(&conn)?,
        }
        let clock: i64 =
            conn.query_row("SELECT COALESCE(MAX(last_used), 0) FROM entries", [], |r| {
                r.get(0)
            })?;
        Ok(Self {
            inner: Mutex::new(Inner { conn, clock }),
        })
    }

    fn create_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(&format!(
            "BEGIN;
             DROP TABLE IF EXISTS entries;
             CREATE TABLE entries (
                 path      BLOB    NOT NULL,
                 kind      TEXT    NOT NULL,
                 params    TEXT    NOT NULL,
                 size      INTEGER NOT NULL,
                 mtime_ns  TEXT    NOT NULL,
                 data      BLOB    NOT NULL,
                 len       INTEGER NOT NULL,
                 last_used INTEGER NOT NULL,
                 PRIMARY KEY (path, kind, params)
             );
             CREATE INDEX entries_last_used ON entries (last_used);
             DELETE FROM schema_version;
             INSERT INTO schema_version (version) VALUES ({SCHEMA_VERSION});
             COMMIT;"
        ))?;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic in another worker cannot leave SQLite inconsistent (each
        // statement is atomic), so recover from poisoning.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Fetch the blob for `(source, kind, params)`. A changed source size or
    /// mtime is a miss. A hit marks the entry as most recently used.
    pub fn get(&self, source: &SourceKey, kind: &str, params: &str) -> Result<Option<Vec<u8>>> {
        let mut inner = self.lock();
        inner.clock += 1;
        let now = inner.clock;
        let tx = inner.conn.transaction()?;
        let hit: Option<(i64, Vec<u8>)> = tx
            .query_row(
                "SELECT rowid, data FROM entries
                 WHERE path = ?1 AND kind = ?2 AND params = ?3 AND size = ?4 AND mtime_ns = ?5",
                params![
                    source.path_bytes(),
                    kind,
                    params,
                    source.size_i64(),
                    source.mtime_text()
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((rowid, _)) = &hit {
            tx.execute(
                "UPDATE entries SET last_used = ?1 WHERE rowid = ?2",
                params![now, rowid],
            )?;
        }
        tx.commit()?;
        Ok(hit.map(|(_, data)| data))
    }

    /// Store `bytes` for `(source, kind, params)`, replacing any previous
    /// value and every entry of the same path and kind recorded for a
    /// different size or mtime (a stale version of the file).
    pub fn put(&self, source: &SourceKey, kind: &str, params: &str, bytes: &[u8]) -> Result<()> {
        let mut inner = self.lock();
        inner.clock += 1;
        let now = inner.clock;
        let tx = inner.conn.transaction()?;
        tx.execute(
            "DELETE FROM entries
             WHERE path = ?1 AND kind = ?2 AND (size <> ?3 OR mtime_ns <> ?4)",
            params![
                source.path_bytes(),
                kind,
                source.size_i64(),
                source.mtime_text()
            ],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO entries
                 (path, kind, params, size, mtime_ns, data, len, last_used)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                source.path_bytes(),
                kind,
                params,
                source.size_i64(),
                source.mtime_text(),
                bytes,
                bytes.len() as i64,
                now
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// [`Cache::get`] and decode the blob as JSON.
    pub fn get_json<T: DeserializeOwned>(
        &self,
        source: &SourceKey,
        kind: &str,
        params: &str,
    ) -> Result<Option<T>> {
        match self.get(source, kind, params)? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Encode `value` as JSON and [`Cache::put`] it.
    pub fn put_json<T: Serialize + ?Sized>(
        &self,
        source: &SourceKey,
        kind: &str,
        params: &str,
        value: &T,
    ) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        self.put(source, kind, params, &bytes)
    }

    /// Total size in bytes of all stored blobs.
    pub fn total_bytes(&self) -> Result<u64> {
        let inner = self.lock();
        let total: i64 =
            inner
                .conn
                .query_row("SELECT COALESCE(SUM(len), 0) FROM entries", [], |r| {
                    r.get(0)
                })?;
        Ok(total.max(0) as u64)
    }

    /// Delete least recently used entries until the total blob size is at
    /// most `max_bytes`. Returns the number of entries deleted.
    pub fn prune(&self, max_bytes: u64) -> Result<usize> {
        let mut inner = self.lock();
        let tx = inner.conn.transaction()?;
        let total: i64 = tx.query_row("SELECT COALESCE(SUM(len), 0) FROM entries", [], |r| {
            r.get(0)
        })?;
        let mut total = total.max(0) as u64;
        let mut victims = Vec::new();
        if total > max_bytes {
            let mut stmt = tx.prepare("SELECT rowid, len FROM entries ORDER BY last_used ASC")?;
            let mut rows = stmt.query([])?;
            while total > max_bytes {
                let Some(row) = rows.next()? else { break };
                let rowid: i64 = row.get(0)?;
                let len: i64 = row.get(1)?;
                victims.push(rowid);
                total = total.saturating_sub(len.max(0) as u64);
            }
        }
        {
            let mut del = tx.prepare("DELETE FROM entries WHERE rowid = ?1")?;
            for rowid in &victims {
                del.execute([rowid])?;
            }
        }
        tx.commit()?;
        Ok(victims.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn cache_is_send_and_sync() {
        assert_send_sync::<Cache>();
    }

    #[test]
    fn mtime_before_epoch_is_negative_text() {
        let k = SourceKey {
            path: "/x".into(),
            size: 1,
            mtime_ns: -5,
        };
        assert_eq!(k.mtime_text(), "-5");
    }
}
