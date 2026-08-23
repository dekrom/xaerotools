//! Persistent region-thumbnail store — the on-disk half of the overzoom
//! pyramid.
//!
//! The in-memory thumbnail caches are byte-bounded, and one deeply zoomed-out
//! tile over a dense area can cover more regions than the whole budget holds,
//! so without persistence the caches thrash: imagery that was on screen falls
//! back to coverage rectangles until a background warm re-decodes it, which
//! evicts other tiles' thumbnails in turn. This store keeps every thumbnail
//! ever rendered (32px and 8px, keyed by the region's own mtime) in a SQLite
//! file next to the server config, so a region is decoded at most once per
//! on-disk version — across requests, cache evictions and restarts.
//!
//! Reads happen inline (a point/bbox lookup is microseconds against a warm
//! page cache); writes go through a channel to one writer thread that batches
//! them into transactions, so a render never blocks on fsync. The store is
//! advisory: any failure disables it with a warning and the server falls back
//! to the pure in-memory behaviour.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

/// One rendered region's thumbnails, queued for the writer thread.
pub(crate) struct ThumbRow {
    pub dir: String,
    pub rx: i32,
    pub rz: i32,
    pub mtime_ms: u64,
    pub t32: Vec<u8>,
    pub t8: Vec<u8>,
}

/// Writer queue depth. A full queue drops writes (the thumbnail is still in
/// RAM; it just won't survive eviction) rather than stalling a render.
const WRITE_QUEUE_CAP: usize = 8192;

pub(crate) struct PyramidStore {
    read: Mutex<Connection>,
    tx: mpsc::SyncSender<ThumbRow>,
    /// Committed write batches so far. Lets a caller skip a bbox prefetch it
    /// already ran when nothing new can have landed since.
    writes: Arc<AtomicU64>,
}

fn open_conn(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    // Two serve processes may share the store (they share the config dir);
    // without a timeout the loser of any write/checkpoint collision errors
    // instantly and a whole batch of thumbnails silently never persists.
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| e.to_string())?;
    // t8 sits before t32 so deep-zoom bbox reads (t8 only, the common case on
    // a huge archive) never have to walk the 4 KiB t32 blob's overflow pages.
    // Existing stores created with the reverse order keep working — every
    // statement names its columns.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS thumbs(
            dir   TEXT    NOT NULL,
            rx    INTEGER NOT NULL,
            rz    INTEGER NOT NULL,
            mtime INTEGER NOT NULL,
            t8    BLOB    NOT NULL,
            t32   BLOB    NOT NULL,
            PRIMARY KEY(dir, rx, rz)
        ) WITHOUT ROWID;",
    )
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Bump when the renderer's output changes: stored thumbnails are rendered
/// pixels, so a new algorithm must retire every old row or the map shows a
/// patchwork of old and new looks. Checked via `PRAGMA user_version`.
const RENDER_ALGO_VERSION: i32 = 2;

impl PyramidStore {
    /// Opens (or creates) the store. `Err` is advisory — callers run without.
    pub(crate) fn open(path: &Path) -> Result<PyramidStore, String> {
        let read = open_conn(path)?;
        let stored: i32 = read
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if stored != RENDER_ALGO_VERSION {
            if stored != 0 {
                eprintln!("pyramid store: render algorithm changed — thumbnails will regenerate");
            }
            read.execute_batch(&format!(
                "DELETE FROM thumbs; PRAGMA user_version = {RENDER_ALGO_VERSION}; VACUUM;"
            ))
            .map_err(|e| e.to_string())?;
        }
        let write = open_conn(path)?;
        let (tx, rx) = mpsc::sync_channel::<ThumbRow>(WRITE_QUEUE_CAP);
        let writes = Arc::new(AtomicU64::new(0));
        let writes_w = writes.clone();
        std::thread::Builder::new()
            .name("xt-pyramid-write".into())
            .spawn(move || writer_loop(write, rx, writes_w))
            .map_err(|e| e.to_string())?;
        Ok(PyramidStore {
            read: Mutex::new(read),
            tx,
            writes,
        })
    }

    /// Queues one region's thumbnails. Never blocks; a full queue drops.
    pub(crate) fn put(&self, row: ThumbRow) {
        let _ = self.tx.try_send(row);
    }

    /// Monotonic count of committed write batches. Equal counts before and
    /// after mean a repeated bbox prefetch cannot find anything new.
    pub(crate) fn writes(&self) -> u64 {
        self.writes.load(Ordering::Acquire)
    }

    /// Every stored thumbnail inside the bbox (inclusive), as
    /// `(rx, rz, mtime_ms, blob)` of the requested tier. Callers filter by
    /// mtime against the live index — a stale row is simply ignored (and
    /// overwritten whenever the region is next rendered).
    pub(crate) fn load_bbox(
        &self,
        dir: &str,
        rx0: i32,
        rz0: i32,
        rx1: i32,
        rz1: i32,
        want_32: bool,
    ) -> Vec<(i32, i32, u64, Vec<u8>)> {
        let col = if want_32 { "t32" } else { "t8" };
        let conn = self.read.lock().unwrap();
        let sql = format!(
            "SELECT rx, rz, mtime, {col} FROM thumbs
             WHERE dir = ?1 AND rx BETWEEN ?2 AND ?3 AND rz BETWEEN ?4 AND ?5"
        );
        let Ok(mut stmt) = conn.prepare_cached(&sql) else {
            return Vec::new();
        };
        let rows = stmt.query_map(rusqlite::params![dir, rx0, rx1, rz0, rz1], |r| {
            Ok((
                r.get::<_, i32>(0)?,
                r.get::<_, i32>(1)?,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, Vec<u8>>(3)?,
            ))
        });
        match rows {
            Ok(it) => it.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// One region's thumbnails, if stored at exactly this mtime.
    pub(crate) fn load_one(
        &self,
        dir: &str,
        rx: i32,
        rz: i32,
        mtime_ms: u64,
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn
            .prepare_cached("SELECT mtime, t32, t8 FROM thumbs WHERE dir=?1 AND rx=?2 AND rz=?3")
            .ok()?;
        let (mtime, t32, t8): (i64, Vec<u8>, Vec<u8>) = stmt
            .query_row(rusqlite::params![dir, rx, rz], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .ok()?;
        (mtime as u64 == mtime_ms).then_some((t32, t8))
    }
}

/// Drains the queue into batched transactions. Exits when every sender is
/// dropped (server shutdown).
fn writer_loop(conn: Connection, rx: mpsc::Receiver<ThumbRow>, writes: Arc<AtomicU64>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        // Take whatever else is already queued, up to one transaction's worth.
        while batch.len() < 512 {
            match rx.try_recv() {
                Ok(row) => batch.push(row),
                Err(_) => break,
            }
        }
        let write = || -> rusqlite::Result<()> {
            // IMMEDIATE takes the write lock up front, so a collision with
            // another process surfaces here (and waits out the busy timeout)
            // instead of failing the COMMIT after all the work.
            conn.execute_batch("BEGIN IMMEDIATE")?;
            {
                let mut stmt = conn.prepare_cached(
                    "INSERT OR REPLACE INTO thumbs(dir, rx, rz, mtime, t32, t8)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                )?;
                for row in &batch {
                    stmt.execute(rusqlite::params![
                        row.dir,
                        row.rx,
                        row.rz,
                        row.mtime_ms as i64,
                        row.t32,
                        row.t8,
                    ])?;
                }
            }
            conn.execute_batch("COMMIT")?;
            Ok(())
        };
        match write() {
            Ok(()) => {
                writes.fetch_add(1, Ordering::Release);
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                eprintln!("pyramid store write failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_mtime_filtering() {
        let dir = std::env::temp_dir().join(format!("xt-pyr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = PyramidStore::open(&dir.join("pyramid.db")).unwrap();
        store.put(ThumbRow {
            dir: "/maps/a".into(),
            rx: 3,
            rz: -4,
            mtime_ms: 1000,
            t32: vec![1; 8],
            t8: vec![2; 4],
        });
        store.put(ThumbRow {
            dir: "/maps/a".into(),
            rx: 5,
            rz: 5,
            mtime_ms: 2000,
            t32: vec![3; 8],
            t8: vec![4; 4],
        });
        // The writer thread is async; poll briefly for the rows to land.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if store.load_one("/maps/a", 3, -4, 1000).is_some()
                && store.load_one("/maps/a", 5, 5, 2000).is_some()
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "writes never landed");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let (t32, t8) = store.load_one("/maps/a", 3, -4, 1000).unwrap();
        assert_eq!((t32, t8), (vec![1; 8], vec![2; 4]));
        // Wrong mtime = miss; other dir = miss.
        assert!(store.load_one("/maps/a", 3, -4, 999).is_none());
        assert!(store.load_one("/maps/b", 3, -4, 1000).is_none());

        let hits = store.load_bbox("/maps/a", 0, -10, 10, 10, false);
        assert_eq!(hits.len(), 2);
        assert!(hits
            .iter()
            .any(|&(rx, rz, m, ref b)| (rx, rz, m) == (3, -4, 1000) && b == &vec![2; 4]));

        // Replacing at a newer mtime wins.
        store.put(ThumbRow {
            dir: "/maps/a".into(),
            rx: 3,
            rz: -4,
            mtime_ms: 5000,
            t32: vec![9; 8],
            t8: vec![9; 4],
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while store.load_one("/maps/a", 3, -4, 5000).is_none() {
            assert!(std::time::Instant::now() < deadline, "replace never landed");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(store.load_one("/maps/a", 3, -4, 1000).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
