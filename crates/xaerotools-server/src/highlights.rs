//! Chunk-highlight ingest — the server's own XaeroPlus database.
//!
//! `POST /ingest/v1/highlights` takes rows a client's XaeroPlus has just
//! found — new chunks, old chunks, portals — and merges them into a database
//! the server owns, under `merged/world-map/<world>/XaeroPlus<Kind>.db`. That
//! tree is already served as a map root, so the rows show up as the ordinary
//! highlight overlay for that world, and the file stays a valid XaeroPlus v2
//! database: it can be copied straight back into a game instance.
//!
//! Only rows travel, never the database. The real ones run to gigabytes and
//! carry no index on `foundTime`, so neither uploading nor rescanning them is
//! affordable; the client reads XaeroPlus's in-memory cache instead and sends
//! what it has found since the last batch (see `docs/INGEST.md`).
//!
//! **Remote servers only.** A server sharing a machine with the game is
//! already reading that instance's live databases through a scanned root, and
//! feeding its own copy back would be a second, diverging map of the same
//! data — so uploads from loopback are refused, and the addon does not offer
//! the feature when its server URL is local.
//!
//! Merge rule is `xaero-db`'s: oldest `foundTime` wins, because it is the
//! first-sighting time and the older sighting is the true one. (The
//! height-valued LavaColumns database merges the other way, which is why it
//! is not one of the syncable kinds.)
//!
//! Batch wire format (little-endian), Content-Type application/octet-stream:
//!   "XTHL" u8 version=1 u16 count, then count x { i32 x, i32 z, i64 foundTime }
//! with x/z in CHUNK coordinates.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rusqlite::{Connection, OpenFlags};

use crate::ingest::safe_segment;
use crate::live::{bucket_allow, Bucket};
use crate::{now_ms, AppState};

/// Rows per batch. 4096 x 16 B keeps a batch under 64 KB, which is more than
/// a client finds between two sweeps even flying a highway.
pub(crate) const BATCH_MAX: usize = 4096;
/// 4 magic + 1 version + 2 count + BATCH_MAX rows.
pub(crate) const HIGHLIGHT_BODY_MAX: usize = 7 + BATCH_MAX * 16;
/// Uploads per second per player.
const RATE_PER_SEC: f64 = 2.0;
const RATE_BURST: f64 = 6.0;
/// |x|/|z| cap in chunks: the world border with slack.
const CHUNK_COORD_CAP: u32 = 2_600_000;

/// The databases a client may sync. All three are timestamp-valued and small
/// enough per session to be worth sharing; the multi-gigabyte palette and
/// modern-chunk databases are seeded out of band, not streamed.
pub(crate) const SYNCABLE: &[&str] = &[
    "XaeroPlusNewChunks.db",
    "XaeroPlusOldChunks.db",
    "XaeroPlusPortals.db",
];

#[derive(serde::Deserialize)]
pub(crate) struct HighlightQuery {
    world: String,
    db: String,
    dim: String,
}

/// A dimension resource key, which is also the v2 table name. XaeroPlus writes
/// `minecraft:overworld` and friends; a modded dimension is any namespaced id.
fn safe_dim_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.contains(':')
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || ":_-./".contains(c))
}

fn validate(q: &HighlightQuery) -> Result<(), &'static str> {
    if !safe_segment(&q.world) {
        return Err("bad world folder name");
    }
    if !SYNCABLE.contains(&q.db.as_str()) {
        return Err("that database is not syncable");
    }
    if !safe_dim_key(&q.dim) {
        return Err("bad dimension key");
    }
    Ok(())
}

/// One parsed row: chunk coordinates and the value the module stored.
pub(crate) type HighlightRow = (i32, i32, i64);

pub(crate) fn parse_batch(body: &[u8]) -> Result<Vec<HighlightRow>, &'static str> {
    if body.len() < 7 || &body[0..4] != b"XTHL" {
        return Err("bad magic");
    }
    if body[4] != 1 {
        return Err("unsupported batch version");
    }
    let count = u16::from_le_bytes([body[5], body[6]]) as usize;
    if count > BATCH_MAX {
        return Err("too many rows");
    }
    if body.len() != 7 + count * 16 {
        return Err("batch length does not match its count");
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let o = 7 + i * 16;
        let x = i32::from_le_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]]);
        let z = i32::from_le_bytes([body[o + 4], body[o + 5], body[o + 6], body[o + 7]]);
        let v = i64::from_le_bytes([
            body[o + 8],
            body[o + 9],
            body[o + 10],
            body[o + 11],
            body[o + 12],
            body[o + 13],
            body[o + 14],
            body[o + 15],
        ]);
        // unsigned_abs, because i32::MIN.abs() overflows — and a client that
        // sends it is exactly the one this check exists for.
        if x.unsigned_abs() > CHUNK_COORD_CAP || z.unsigned_abs() > CHUNK_COORD_CAP {
            return Err("chunk coordinates out of range");
        }
        out.push((x, z, v));
    }
    Ok(out)
}

/// Path of the server-owned database for one world.
pub(crate) fn db_path(ingest_dir: &Path, world: &str, db: &str) -> PathBuf {
    ingest_dir
        .join("merged")
        .join("world-map")
        .join(world)
        .join(db)
}

/// Opens (creating if needed) the server's copy and brings it to XaeroPlus's
/// v2 shape, so the file is one a game instance would accept verbatim.
fn open_v2(path: &Path, table: &str) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| e.to_string())?;
    // WAL matches what XaeroPlus writes, and keeps the read-only handle the
    // highlight tiles hold from blocking these writes.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .map_err(|e| e.to_string())?;
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS metadata (id INTEGER PRIMARY KEY, version INTEGER); \
         INSERT OR REPLACE INTO metadata (id, version) VALUES (0, 2); \
         CREATE TABLE IF NOT EXISTS {t} (x INTEGER, z INTEGER, foundTime INTEGER, \
         PRIMARY KEY (x, z)) WITHOUT ROWID;",
        t = xaero_db::quote_ident(table),
    ))
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Upserts one batch. Returns how many rows were new or moved the value.
fn store_rows(
    ingest_dir: &Path,
    lock: &std::sync::Mutex<()>,
    q: &HighlightQuery,
    rows: &[HighlightRow],
) -> Result<usize, (StatusCode, String)> {
    let path = db_path(ingest_dir, &q.world, &q.db);
    let prefers_max = xaero_db::highlight_semantics(&q.db).prefers_max();
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let conn = open_v2(&path, &q.dim).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("open {}: {e}", q.db),
        )
    })?;
    let sql = format!(
        "INSERT INTO {t} (x, z, foundTime) VALUES (?1, ?2, ?3) \
         ON CONFLICT(x, z) DO UPDATE SET foundTime = {agg}(foundTime, excluded.foundTime) \
         WHERE foundTime != {agg}(foundTime, excluded.foundTime)",
        t = xaero_db::quote_ident(&q.dim),
        agg = if prefers_max { "MAX" } else { "MIN" },
    );
    let changed = (|| -> Result<usize, rusqlite::Error> {
        let tx = conn.unchecked_transaction()?;
        let mut n = 0usize;
        {
            let mut stmt = tx.prepare(&sql)?;
            for (x, z, v) in rows {
                n += stmt.execute((x, z, v))?;
            }
        }
        tx.commit()?;
        Ok(n)
    })()
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("write: {e}")))?;
    Ok(changed)
}

// --------------------------------------------- POST /ingest/v1/highlights --

pub(crate) async fn ingest_highlights(
    State(st): State<Arc<AppState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Query(q): Query<HighlightQuery>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // The whole point of the channel is a server that is *not* this instance's
    // machine. Locally the server already reads the live databases through a
    // scanned root, so accepting a copy would fork the same data in two.
    if peer.ip().is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            "highlight sync is for remote servers — this one already reads your local databases",
        )
            .into_response();
    }
    let declared = headers.get("x-xt-player").and_then(|v| v.to_str().ok());
    let player = match crate::live::ingest_player(&st, &headers, peer, declared).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    if let Err(msg) = validate(&q) {
        return (StatusCode::BAD_REQUEST, msg).into_response();
    }
    if body.len() > HIGHLIGHT_BODY_MAX {
        return (StatusCode::PAYLOAD_TOO_LARGE, "batch too large").into_response();
    }
    let rows = match parse_batch(&body) {
        Ok(r) => r,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };
    {
        let mut rate = st.ingest.hl_rate.lock().unwrap();
        let bucket = rate
            .entry(player.clone())
            .or_insert_with(|| Bucket::new(RATE_BURST, now_ms()));
        if !bucket_allow(bucket, now_ms(), RATE_PER_SEC, RATE_BURST) {
            return (StatusCode::TOO_MANY_REQUESTS, "rate limited").into_response();
        }
    }
    if rows.is_empty() {
        return StatusCode::NO_CONTENT.into_response();
    }

    let existed = db_path(&st.ingest_dir, &q.world, &q.db).exists();
    let st2 = st.clone();
    let stored = tokio::task::spawn_blocking(move || {
        let out = store_rows(&st2.ingest_dir, &st2.ingest.write_lock, &q, &rows);
        (out, q)
    })
    .await;
    let (stored, q) = match stored {
        Ok(v) => v,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "store task failed").into_response(),
    };
    let changed = match stored {
        Ok(n) => n,
        Err((code, msg)) => return (code, msg).into_response(),
    };

    if !existed {
        // A database the world did not have is a new overlay: the world list
        // has to grow before the viewer can ask for its tiles.
        let _gate = st.ingest.rescan_gate.lock().await;
        crate::rescan_roots(&st).await;
    } else if changed > 0 {
        // Our own write, announced directly rather than waiting on inotify —
        // the same shortcut region uploads take.
        let _ = st.live.fs_tx.send(crate::live::FsEvent::Path(db_path(
            &st.ingest_dir,
            &q.world,
            &q.db,
        )));
        st.live.seq.fetch_add(1, Ordering::Relaxed);
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(rows: &[HighlightRow]) -> Vec<u8> {
        let mut b = vec![b'X', b'T', b'H', b'L', 1];
        b.extend_from_slice(&(rows.len() as u16).to_le_bytes());
        for (x, z, v) in rows {
            b.extend_from_slice(&x.to_le_bytes());
            b.extend_from_slice(&z.to_le_bytes());
            b.extend_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn parses_valid_batches_and_rejects_junk() {
        let rows = vec![(1, -2, 1_700_000_000_000i64), (-2_600_000, 2_600_000, 0)];
        assert_eq!(parse_batch(&batch(&rows)).unwrap(), rows);
        assert!(parse_batch(b"nope").is_err());
        let mut short = batch(&rows);
        short.truncate(short.len() - 1);
        assert!(parse_batch(&short).is_err());
        let mut wrong_version = batch(&rows);
        wrong_version[4] = 2;
        assert!(parse_batch(&wrong_version).is_err());
        // Coordinates past the world border are a broken client, not data.
        assert!(parse_batch(&batch(&[(9_000_000, 0, 1)])).is_err());
        assert!(parse_batch(&batch(&[(i32::MIN, 0, 1)])).is_err());
    }

    #[test]
    fn query_validation_matches_the_syncable_set() {
        let q = |db: &str, dim: &str| HighlightQuery {
            world: "Multiplayer_2b2t".into(),
            db: db.into(),
            dim: dim.into(),
        };
        assert!(validate(&q("XaeroPlusNewChunks.db", "minecraft:overworld")).is_ok());
        assert!(validate(&q("XaeroPlusOldChunks.db", "minecraft:the_nether")).is_ok());
        // Height-valued and multi-gigabyte databases stay out.
        assert!(validate(&q("XaeroPlusLavaColumns.db", "minecraft:overworld")).is_err());
        assert!(validate(&q("XaeroPlusModernChunks.db", "minecraft:overworld")).is_err());
        // A table name has to be a namespaced key, not a path or an injection.
        assert!(validate(&q("XaeroPlusPortals.db", "overworld")).is_err());
        assert!(validate(&q("XaeroPlusPortals.db", "a\"; DROP TABLE x; --")).is_err());
    }

    #[test]
    fn merges_oldest_sighting_and_writes_a_v2_database() {
        let dir = std::env::temp_dir().join(format!("xt-hl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lock = std::sync::Mutex::new(());
        let q = HighlightQuery {
            world: "Multiplayer_2b2t".into(),
            db: "XaeroPlusNewChunks.db".into(),
            dim: "minecraft:overworld".into(),
        };
        assert_eq!(store_rows(&dir, &lock, &q, &[(5, 6, 2000)]).unwrap(), 1);
        // Older sighting of the same chunk wins; the newer one changes nothing.
        assert_eq!(store_rows(&dir, &lock, &q, &[(5, 6, 1000)]).unwrap(), 1);
        assert_eq!(store_rows(&dir, &lock, &q, &[(5, 6, 3000)]).unwrap(), 0);

        let path = db_path(&dir, &q.world, &q.db);
        let db = xaero_db::open_readonly(&path).expect("reads back as a highlight db");
        assert_eq!(db.version, 2);
        assert!(db.tables.iter().any(|t| t == "minecraft:overworld"));
        let v: i64 = db
            .conn
            .query_row(
                "SELECT foundTime FROM \"minecraft:overworld\" WHERE x = 5 AND z = 6",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, 1000);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
