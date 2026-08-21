//! Region ingest — the upload half of ADR 007's live-share seam.
//!
//! `POST /ingest/v1/region` accepts a raw region container (the client's
//! `<rx>_<rz>.zip` / `.xaero` bytes) for a (world, dim, mw[, cave], rx, rz),
//! authenticated like position ingest: per-player bearer tokens for remote
//! peers, while loopback clients may skip the token and name themselves via
//! `X-XT-Player` (see `live::local_player`).
//! Every upload is validated by a full decode before anything is written, then
//! stored twice under the server-owned ingest dir:
//!
//! - `players/<player>/world-map/<world>/...` — the uploader's bytes verbatim,
//!   a per-client backup of exactly what their game saved.
//! - `merged/world-map/<world>/...` — tile-merged across every uploader
//!   (incoming tiles win, tiles it lacks survive), the shared group map.
//!
//! Both trees are ordinary Xaero layouts and are served as auto-managed roots,
//! so the viewer, merge tools, stats and doctor all work on them unchanged.
//! This is the only code in the server that writes region data, and it only
//! ever writes inside the ingest dir — scanned user roots stay read-only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use xaero_core::naming::is_multiworld_folder;

use crate::live::{bucket_allow, Bucket};
use crate::{now_ms, AppState, WorldEntry};

/// Body cap for one region upload. Real 2b2t regions run a few hundred KB to a
/// few MB; anything near this is not a region file.
pub(crate) const REGION_BODY_MAX: usize = 32 << 20;
/// Upload rate per player: full-sync clients throttle themselves, but a burst
/// of freshly-mapped regions must not have to wait a second each.
const RATE_PER_SEC: f64 = 10.0;
const RATE_BURST: f64 = 20.0;
/// |rx|/|rz| cap — the world border (|x| <= 40M blocks) is region 78,125.
const COORD_CAP: i32 = 100_000;

pub(crate) struct IngestState {
    /// Rate buckets keyed by *validated* player name.
    rate: Mutex<HashMap<String, Bucket>>,
    /// Serializes read-modify-write cycles on the merged tree: two uploads of
    /// the same region must not interleave decode/merge/rename.
    write_lock: Mutex<()>,
    /// Serializes "new layer appeared" rescans so an upload burst triggers one
    /// rescan, not one per request.
    rescan_gate: tokio::sync::Mutex<()>,
}

impl IngestState {
    pub(crate) fn new() -> IngestState {
        IngestState {
            rate: Mutex::new(HashMap::new()),
            write_lock: Mutex::new(()),
            rescan_gate: tokio::sync::Mutex::new(()),
        }
    }
}

/// The ingest-managed directories that should be served as map roots: one per
/// player that has uploaded, plus the shared merged tree.
pub(crate) fn ingest_roots(ingest_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(ingest_dir.join("players")) {
        for e in entries.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                out.push(e.path());
            }
        }
    }
    let merged = ingest_dir.join("merged");
    if merged.is_dir() {
        out.push(merged);
    }
    out.sort();
    out
}

#[derive(serde::Deserialize)]
pub(crate) struct RegionQuery {
    world: String,
    dim: String,
    mw: String,
    rx: i32,
    rz: i32,
    #[serde(default)]
    cave: Option<i32>,
}

/// One path segment as a client may name it (world folders can carry spaces,
/// `$`/`%` escapes, dots). No separators, no traversal, no hidden files.
/// Also the mint-time rule for player names (`/api/tokens`).
pub(crate) fn safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || " _-.$%()',+&@~".contains(c))
}

fn validate(q: &RegionQuery) -> Result<(), &'static str> {
    if !safe_segment(&q.world) {
        return Err("bad world folder name");
    }
    if !safe_segment(&q.dim)
        || xaero_core::naming::Dimension::from_worldmap_folder(&q.dim).is_none()
    {
        return Err("bad dim folder name");
    }
    if !safe_segment(&q.mw) || !is_multiworld_folder(&q.mw) {
        return Err("bad multiworld folder name");
    }
    if q.rx.abs() > COORD_CAP || q.rz.abs() > COORD_CAP {
        return Err("region coordinates out of range");
    }
    Ok(())
}

/// `world-map/<world>/<dim>/<mw>[/caves/<n>]` — the layer dir below a root.
fn layer_rel(q: &RegionQuery) -> PathBuf {
    let base = Path::new("world-map")
        .join(&q.world)
        .join(&q.dim)
        .join(&q.mw);
    match q.cave {
        None => base,
        Some(n) => base.join("caves").join(n.to_string()),
    }
}

fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let tmp = dir.join(format!("{name}.tmp-xt"));
    let dst = dir.join(name);
    std::fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &dst).map_err(|e| format!("rename {}: {e}", dst.display()))?;
    Ok(dst)
}

/// Writes region bytes as `<rx>_<rz>.<ext>` (ext from the container magic) and
/// drops any stale opposite-extension file so one coordinate is one file.
fn write_region_file(dir: &Path, rx: i32, rz: i32, bytes: &[u8]) -> Result<(), String> {
    let is_zip = bytes.starts_with(b"PK");
    let (ext, other) = if is_zip {
        ("zip", "xaero")
    } else {
        ("xaero", "zip")
    };
    write_atomic(dir, &format!("{rx}_{rz}.{ext}"), bytes)?;
    let _ = std::fs::remove_file(dir.join(format!("{rx}_{rz}.{other}")));
    Ok(())
}

/// The whole blocking side of one upload: decode-validate, back up verbatim,
/// merge into the shared tree. Factored off the handler so tests can drive it
/// without HTTP.
pub(crate) fn store_upload(
    ingest_dir: &Path,
    write_lock: &Mutex<()>,
    player: &str,
    q: &RegionQuery,
    bytes: &[u8],
) -> Result<(), (StatusCode, String)> {
    let bad = |m: &str| (StatusCode::BAD_REQUEST, m.to_string());
    let stream = xaero_core::read_region_container(bytes)
        .map_err(|e| bad(&format!("unreadable region container: {e}")))?;
    let incoming = xaero_core::decode_region(&stream)
        .map_err(|e| bad(&format!("region does not decode: {e}")))?;
    if incoming.truncated {
        // The client has the complete file; a short read means it caught the
        // game mid-write. Rejecting makes it retry after the file settles.
        return Err(bad("region is truncated — retry after the file settles"));
    }

    let rel = layer_rel(q);
    let backup_dir = ingest_dir.join("players").join(player).join(&rel);
    write_region_file(&backup_dir, q.rx, q.rz, bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let merged_dir = ingest_dir.join("merged").join(&rel);
    let _guard = write_lock.lock().unwrap();
    let existing = ["zip", "xaero"]
        .iter()
        .map(|ext| merged_dir.join(format!("{}_{}.{ext}", q.rx, q.rz)))
        .find(|p| p.is_file());
    let merged_from_existing = existing.and_then(|path| {
        // An unreadable merged file (torn write from a crash) must not wedge
        // the coordinate forever: fall through to a fresh verbatim copy.
        let old = std::fs::read(&path).ok()?;
        let dec =
            xaero_core::read_region_container(&old).and_then(|s| xaero_core::decode_region(&s));
        match dec {
            Ok(dec) => Some(dec),
            Err(e) => {
                eprintln!(
                    "ingest: replacing unreadable merged {}: {e}",
                    path.display()
                );
                None
            }
        }
    });
    match merged_from_existing {
        None => write_region_file(&merged_dir, q.rx, q.rz, bytes)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?,
        Some(old) => {
            // Incoming tiles are the newest observation, so they win; tiles the
            // upload lacks survive from what was already merged.
            let merged = xaero_core::merge::merge_regions(&incoming, &old);
            let stream = xaero_core::encode_region(&merged);
            xaero_core::decode_region(&stream).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("merge self-check: {e}"),
                )
            })?;
            let container = xaero_core::write_region_container(&stream)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("zip: {e}")))?;
            write_region_file(&merged_dir, q.rx, q.rz, &container)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        }
    }
    Ok(())
}

/// True when both ingest roots already carry this (world, dim, mw, cave) — the
/// watcher covers the layer dirs, so no rescan is needed for this upload.
fn layer_known(worlds: &[WorldEntry], st: &AppState, player: &str, q: &RegionQuery) -> bool {
    let roots = [
        st.ingest_dir.join("players").join(player),
        st.ingest_dir.join("merged"),
    ];
    roots.iter().all(|root| {
        let root = crate::canon(root);
        worlds.iter().any(|we| {
            we.root == root
                && we.world.id == q.world
                && we.world.dims.iter().any(|d| {
                    d.folder == q.dim
                        && d.multiworlds.iter().any(|m| {
                            m.id == q.mw
                                && match q.cave {
                                    None => true,
                                    Some(n) => m.cave_layers.contains(&n),
                                }
                        })
                })
        })
    })
}

// ------------------------------------------------- POST /ingest/v1/region --

pub(crate) async fn ingest_region(
    State(st): State<Arc<AppState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Query(q): Query<RegionQuery>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Tokenless loopback clients name themselves here; ignored when a valid
    // token names the player (the token is the stronger claim).
    let declared = headers.get("x-xt-player").and_then(|v| v.to_str().ok());
    let player = match crate::live::ingest_player(&st, &headers, peer, declared).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    if !safe_segment(&player) {
        // Player names become directory names; a token generated for an unsafe
        // one is a server-side misconfiguration, not a client error.
        return (
            StatusCode::FORBIDDEN,
            "player name is not filesystem-safe — regenerate the token for a simpler name",
        )
            .into_response();
    }
    if let Err(msg) = validate(&q) {
        return (StatusCode::BAD_REQUEST, msg).into_response();
    }
    if q.cave.is_some() && st.ingest_no_caves {
        return (
            StatusCode::FORBIDDEN,
            "cave-layer uploads are disabled on this server (--ingest-no-caves)",
        )
            .into_response();
    }
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty body").into_response();
    }
    if body.len() > REGION_BODY_MAX {
        return (StatusCode::PAYLOAD_TOO_LARGE, "region too large").into_response();
    }
    {
        let mut rate = st.ingest.rate.lock().unwrap();
        let bucket = rate
            .entry(player.clone())
            .or_insert_with(|| Bucket::new(RATE_BURST, now_ms()));
        if !bucket_allow(bucket, now_ms(), RATE_PER_SEC, RATE_BURST) {
            return (StatusCode::TOO_MANY_REQUESTS, "rate limited").into_response();
        }
    }

    let known_before = layer_known(&st.worlds.read().unwrap().clone(), &st, &player, &q);
    let st2 = st.clone();
    let player2 = player.clone();
    let stored = tokio::task::spawn_blocking(move || {
        let out = store_upload(&st2.ingest_dir, &st2.ingest.write_lock, &player2, &q, &body);
        (out, q)
    })
    .await;
    let (stored, q) = match stored {
        Ok(v) => v,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "store task failed").into_response(),
    };
    if let Err((code, msg)) = stored {
        return (code, msg).into_response();
    }

    // Nudge the invalidation pipeline directly instead of waiting for inotify
    // to notice our own writes — the upload is visible in the viewer within
    // one debounce interval even on filesystems with unreliable watches. The
    // debouncer re-stats, so the synthesized extension does not matter.
    let rel = layer_rel(&q);
    let file = format!("{}_{}.zip", q.rx, q.rz);
    for root in [
        st.ingest_dir.join("players").join(&player),
        st.ingest_dir.join("merged"),
    ] {
        let _ = st
            .live
            .fs_tx
            .send(crate::live::FsEvent::Path(root.join(&rel).join(&file)));
    }
    // The authoritative region replaces any live-preview chunks it covers —
    // but only after the fresh tiles have had time to reach viewers (debounce
    // plus a refresh round-trip). The preview layer draws on top, so clearing
    // it immediately flashed the *old* map for a second before the new
    // imagery landed underneath; cleared late, the swap is invisible because
    // near-identical imagery is already there. Until then the canvas briefly
    // keeps chunks the upload also covers, which depict the same terrain.
    if let Some(dim) = xaero_core::naming::Dimension::from_worldmap_folder(&q.dim) {
        let dim_key = dim.resource_key();
        let st3 = st.clone();
        let (rx, rz) = (q.rx, q.rz);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
            if st3.preview.evict_region(&dim_key, rx, rz) {
                let seq = st3
                    .live
                    .seq
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                let msg = serde_json::json!({
                    "type": "preview", "dim": dim_key,
                    "regions": [[rx, rz]], "v": seq,
                });
                let _ = st3.live.tx.send(msg.to_string());
            }
        });
    }

    if !known_before {
        // First region of a new world/dim/mw (or a brand-new player root): the
        // world list has to grow and the watcher re-arm. Serialized so a burst
        // of such uploads costs one rescan.
        let _gate = st.ingest.rescan_gate.lock().await;
        if !layer_known(&st.worlds.read().unwrap().clone(), &st, &player, &q) {
            crate::rescan_roots(&st).await;
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(world: &str, dim: &str, mw: &str) -> RegionQuery {
        RegionQuery {
            world: world.into(),
            dim: dim.into(),
            mw: mw.into(),
            rx: 1,
            rz: -2,
            cave: None,
        }
    }

    #[test]
    fn validates_path_segments() {
        assert!(validate(&q("Multiplayer_2b2t", "null", "mw$default")).is_ok());
        assert!(validate(&q("Multiplayer_2b2t.org", "DIM-1", "mw$-542221765")).is_ok());
        assert!(validate(&q(
            "Multiplayer_Minecraft Server",
            "minecraft$worlds%2b2t%2b2t_1",
            "cm$converted"
        ))
        .is_ok());
        // Traversal, separators, hidden files, foreign folders.
        assert!(validate(&q("../evil", "null", "mw$default")).is_err());
        assert!(validate(&q("a/b", "null", "mw$default")).is_err());
        assert!(validate(&q("w", "not-a-dim", "mw$default")).is_err());
        assert!(validate(&q("w", "null", "region-files")).is_err());
        assert!(validate(&q(".hidden", "null", "mw$default")).is_err());
        assert!(validate(&q("", "null", "mw$default")).is_err());
        let mut far = q("w", "null", "mw$default");
        far.rx = COORD_CAP + 1;
        assert!(validate(&far).is_err());
    }

    #[test]
    fn layer_rel_shapes() {
        let mut r = q("W", "null", "mw$default");
        assert_eq!(layer_rel(&r), Path::new("world-map/W/null/mw$default"));
        r.cave = Some(7);
        assert_eq!(
            layer_rel(&r),
            Path::new("world-map/W/null/mw$default/caves/7")
        );
    }

    fn corpus_root() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("XAERO_CORPUS") {
            let p = PathBuf::from(p);
            return p.is_dir().then_some(p);
        }
        let fallback = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../sample data");
        fallback.is_dir().then(|| fallback.canonicalize().unwrap())
    }

    #[test]
    fn stores_backup_and_merges_shared_from_corpus() {
        let Some(root) = corpus_root() else {
            eprintln!("corpus not found; skipping");
            return;
        };
        // Two real copies of the same coordinate: major-6 (1.21.4) and
        // major-7 (1.21.8) — a genuine cross-version merge.
        let a = std::fs::read(
            root.join("xaero1.21.4/world-map/Multiplayer_2b2t/DIM-1/mw$default/0_-559.zip"),
        )
        .unwrap();
        let b = std::fs::read(
            root.join("xaero1.21.8/world-map/Multiplayer_2b2t/DIM-1/mw$default/0_-559.zip"),
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("xt-ingest-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lock = Mutex::new(());
        let mut req = q("Multiplayer_2b2t", "DIM-1", "mw$default");
        req.rx = 0;
        req.rz = -559;

        store_upload(&dir, &lock, "Alice", &req, &a).unwrap();
        let backup =
            dir.join("players/Alice/world-map/Multiplayer_2b2t/DIM-1/mw$default/0_-559.zip");
        assert_eq!(std::fs::read(&backup).unwrap(), a, "backup is verbatim");
        let merged = dir.join("merged/world-map/Multiplayer_2b2t/DIM-1/mw$default/0_-559.zip");
        assert_eq!(
            std::fs::read(&merged).unwrap(),
            a,
            "first write is verbatim"
        );

        // A second client uploads its own copy: backup separate, merged re-encoded.
        store_upload(&dir, &lock, "Bob", &req, &b).unwrap();
        let bob = dir.join("players/Bob/world-map/Multiplayer_2b2t/DIM-1/mw$default/0_-559.zip");
        assert_eq!(std::fs::read(&bob).unwrap(), b);
        assert_eq!(
            std::fs::read(&backup).unwrap(),
            a,
            "Alice's backup untouched"
        );
        let out = std::fs::read(&merged).unwrap();
        assert_ne!(out, a);
        let dec = xaero_core::read_region_container(&out)
            .and_then(|s| xaero_core::decode_region(&s))
            .unwrap();
        assert_eq!((dec.version.major, dec.version.minor), (7, 8));
        assert!(!dec.truncated);

        // Garbage is rejected outright and writes nothing new.
        let mut junk = q("Multiplayer_2b2t", "DIM-1", "mw$default");
        junk.rx = 9;
        junk.rz = 9;
        assert!(store_upload(&dir, &lock, "Alice", &junk, b"not a region").is_err());
        assert!(!dir
            .join("players/Alice/world-map/Multiplayer_2b2t/DIM-1/mw$default/9_9.zip")
            .exists());

        assert_eq!(ingest_roots(&dir).len(), 3, "Alice, Bob, merged");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
