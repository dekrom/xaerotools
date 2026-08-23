//! Live mode: filesystem watching with targeted cache invalidation, the
//! /ws/live event socket, and the authenticated position-ingest endpoint
//! (the position half of ADR 007's live-share seam).
//!
//! This module only ever stats/readdirs inside roots — never writes there.
//! Pipeline: notify callback (or the poll loop) -> unbounded mpsc ->
//! debounce task (500 ms batches) -> classify -> surgical index updates +
//! throttled broadcasts. Every batch is stamped with the worlds epoch and
//! dropped if a roots rescan or /api/refresh happened mid-flight, because
//! MapId is positional.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use notify::Watcher;
use xaero_core::naming::parse_region_filename;
use xaero_scan::{index_regions, RegionMeta, World};

use crate::{config, now_ms, AppState, MapId, WorldEntry};

/// Events settle for this long before a batch is applied.
const DEBOUNCE_MS: u64 = 500;
/// Minimum spacing of overzoom (z<0) invalidations per map: a gen bump
/// invalidates every overzoom tile of that map, so it must stay rare —
/// native-zoom tiles stay fresh per batch via their mtime stamps.
const DEEP_FLUSH: Duration = Duration::from_secs(10);
/// A map whose accumulated change set is small flushes on this faster timer
/// instead: a traveling player touches a handful of regions at a time, and
/// the viewer refreshes only the overzoom tiles those regions intersect, so
/// the cost of the earlier flush is bounded. Bulk syncs (big sets, or the
/// "assume all" marker) keep the conservative spacing above.
const DEEP_FLUSH_FAST: Duration = Duration::from_secs(2);
/// Largest pending region set the fast timer applies to.
const FAST_FLUSH_MAX: usize = 64;
/// Minimum spacing of highlight-DB events per DB (XaeroPlus WALs are written
/// every few seconds forever; each event makes clients re-render tiles).
const DB_EVENT_MIN: Duration = Duration::from_secs(8);
/// A batch with more unique paths than this (or a kernel queue overflow)
/// degrades to reindex-and-diff of the hot maps.
const BATCH_PATH_CAP: usize = 5_000;
/// Region lists longer than this are broadcast as null ("assume all").
const EVENT_REGION_CAP: usize = 512;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Poll mode / degraded mode never reindexes more often than this — a full
/// readdir of the 1M-region dir costs ~2 s against the live archive.
const REINDEX_MIN: Duration = Duration::from_secs(30);
/// Ingest rate limit per player (4+ accounts at ~1/s must pass easily).
const RATE_PER_SEC: f64 = 5.0;
const RATE_BURST: f64 = 10.0;
/// A quiet /ws/live socket sends an "hb" data message this often. Browser JS
/// cannot observe protocol ping frames, so bounded silence is the only way a
/// client can tell a NAT/proxy-killed connection from an idle map.
const WS_HEARTBEAT: Duration = Duration::from_secs(25);

pub(crate) struct LiveState {
    /// Pre-serialized JSON events fanned out to every /ws/live client.
    pub(crate) tx: tokio::sync::broadcast::Sender<String>,
    pub(crate) positions: Mutex<HashMap<String, PlayerPos>>,
    /// Rate buckets keyed by *validated* player name (bounded by config size;
    /// never keyed by client-supplied strings).
    rate: Mutex<HashMap<String, Bucket>>,
    pub(crate) seq: AtomicU64,
    pub(crate) fs_tx: tokio::sync::mpsc::UnboundedSender<FsEvent>,
    auth_failures: AtomicU32,
    /// Total watcher callbacks seen — the liveness watchdog compares this
    /// against observed WAL mtime progress to detect a dead inotify.
    events_seen: Arc<AtomicU64>,
    /// Single-flight guards: rescans re-arm the watcher, and both loops must
    /// never run twice.
    watchdog_running: std::sync::atomic::AtomicBool,
    poll_running: std::sync::atomic::AtomicBool,
}

impl LiveState {
    /// Linear anti-guessing backoff shared by every token-authenticated ingest
    /// route: returns how long the caller should sleep before its 401.
    pub(crate) fn note_auth_failure(&self) -> u64 {
        let n = self.auth_failures.fetch_add(1, Ordering::Relaxed).min(20);
        100 * (n as u64 + 1)
    }

    pub(crate) fn note_auth_ok(&self) {
        self.auth_failures.store(0, Ordering::Relaxed);
    }

    /// Drops a live marker (roster cleanup of test/duplicate accounts) and
    /// tells every viewer. Purely in-memory — tokens and uploads are
    /// untouched, and the marker returns on that player's next report.
    pub(crate) fn remove_player(&self, name: &str) -> bool {
        let removed = self.positions.lock().unwrap().remove(name).is_some();
        if removed {
            self.rate.lock().unwrap().remove(name);
            let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
            let msg = serde_json::json!({"type": "player_removed", "player": name, "v": seq});
            let _ = self.tx.send(msg.to_string());
        }
        removed
    }

    pub(crate) fn new(fs_tx: tokio::sync::mpsc::UnboundedSender<FsEvent>) -> LiveState {
        LiveState {
            tx: tokio::sync::broadcast::channel(4096).0,
            positions: Mutex::new(HashMap::new()),
            rate: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
            fs_tx,
            auth_failures: AtomicU32::new(0),
            events_seen: Arc::new(AtomicU64::new(0)),
            watchdog_running: std::sync::atomic::AtomicBool::new(false),
            poll_running: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

pub(crate) struct PlayerPos {
    pub dim: String,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub t_ms: u64,
}

pub(crate) enum FsEvent {
    Path(PathBuf),
    /// Kernel queue overflow / notify rescan request: events were lost.
    Overflow,
}

// ------------------------------------------------------------- classifier --

#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) enum Change {
    Region { map: MapId, rx: i32, rz: i32 },
    Db { w: usize, db: String },
}

/// Maps an absolute event path to what it invalidates. Pure; unit-tested.
/// Temp/partial names fall out via parse_region_filename; unknown DBs and
/// paths outside every world are ignored.
pub(crate) fn classify(worlds: &[WorldEntry], path: &Path) -> Option<Change> {
    for (w, we) in worlds.iter().enumerate() {
        let Some(wm) = &we.world.world_map_path else {
            continue;
        };
        let Ok(rel) = path.strip_prefix(wm) else {
            continue;
        };
        let comps: Vec<String> = rel
            .iter()
            .map(|c| c.to_string_lossy().to_string())
            .collect();
        return match comps.as_slice() {
            [file] => {
                let base = file
                    .strip_suffix("-wal")
                    .or_else(|| file.strip_suffix("-shm"))
                    .unwrap_or(file);
                (base.ends_with(".db") && we.world.databases.iter().any(|d| d == base)).then(|| {
                    Change::Db {
                        w,
                        db: base.to_string(),
                    }
                })
            }
            [dim, mw, file] => region_change(&we.world, w, dim, mw, None, file),
            [dim, mw, caves, layer, file] if caves == "caves" => {
                let cave: i32 = layer.parse().ok()?;
                region_change(&we.world, w, dim, mw, Some(cave), file)
            }
            _ => None,
        };
    }
    None
}

fn region_change(
    world: &World,
    w: usize,
    dim_folder: &str,
    mw_id: &str,
    cave: Option<i32>,
    file: &str,
) -> Option<Change> {
    let (rx, rz, _is_zip) = parse_region_filename(file)?;
    let d = world.dims.iter().position(|d| d.folder == dim_folder)?;
    let m = world.dims[d]
        .multiworlds
        .iter()
        .position(|m| m.id == mw_id)?;
    Some(Change::Region {
        map: MapId {
            world: w,
            dim: d,
            mw: m,
            cave,
            roof: None,
        },
        rx,
        rz,
    })
}

pub(crate) fn layer_str(cave: Option<i32>) -> String {
    match cave {
        None => "surface".into(),
        Some(n) => format!("cave-{n}"),
    }
}

/// Entries differing between two indexes (changed mtime/size/kind, added,
/// removed). Pure; unit-tested; drives poll-mode diffing.
pub(crate) fn diff_indexes(
    old: &HashMap<(i32, i32), RegionMeta>,
    new: &HashMap<(i32, i32), RegionMeta>,
) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    for (k, m) in new {
        match old.get(k) {
            None => out.push(*k),
            Some(o) if o.mtime_ms != m.mtime_ms || o.size != m.size || o.is_zip != m.is_zip => {
                out.push(*k)
            }
            _ => {}
        }
    }
    out.extend(old.keys().filter(|k| !new.contains_key(k)));
    out
}

// -------------------------------------------------------------- debouncer --

#[derive(Default)]
struct Throttle {
    last_deep: HashMap<MapId, Instant>,
    /// Regions accumulated since the last deep flush (None = "assume all").
    pending_deep: HashMap<MapId, Option<HashSet<(i32, i32)>>>,
    last_db: HashMap<(usize, String), Instant>,
    pending_db: HashSet<(usize, String)>,
    degrade_pending: bool,
    last_degrade: Option<Instant>,
}

pub(crate) async fn debounce_loop(
    st: Arc<AppState>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<FsEvent>,
) {
    let mut th = Throttle::default();
    let mut buf: Vec<FsEvent> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_millis(DEBOUNCE_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(e) => buf.push(e),
                None => return,
            },
            _ = tick.tick() => {
                if !buf.is_empty() {
                    apply_batch(&st, std::mem::take(&mut buf), &mut th).await;
                }
                if th.degrade_pending { maybe_degrade(&st, &mut th).await; }
                flush_due(&st, &mut th);
            }
        }
    }
}

async fn apply_batch(st: &Arc<AppState>, events: Vec<FsEvent>, th: &mut Throttle) {
    let epoch = st.epoch.load(Ordering::SeqCst);
    let worlds: Arc<Vec<WorldEntry>> = st.worlds.read().unwrap().clone();
    let mut overflow = false;
    let mut unique: HashSet<PathBuf> = HashSet::new();
    for ev in events {
        match ev {
            FsEvent::Path(p) => {
                unique.insert(p);
            }
            FsEvent::Overflow => overflow = true,
        }
    }
    if overflow || unique.len() > BATCH_PATH_CAP {
        th.degrade_pending = true;
        return;
    }

    let mut regions: HashMap<MapId, HashSet<(i32, i32)>> = HashMap::new();
    for path in &unique {
        match classify(&worlds, path) {
            Some(Change::Region { map, rx, rz }) => {
                regions.entry(map).or_default().insert((rx, rz));
            }
            Some(Change::Db { w, db }) => {
                th.pending_db.insert((w, db));
            }
            None => {}
        }
    }
    // Record every classified change (even for maps nobody indexed yet):
    // get_index replays this ring onto freshly built indexes to close the
    // change-races-a-slow-build window.
    {
        let now = Instant::now();
        let mut ring = st.recent.lock().unwrap();
        for (map, rs) in &regions {
            for &(rx, rz) in rs {
                ring.push_back((map.clone(), rx, rz, now));
            }
        }
        // Cap above BATCH_PATH_CAP so one giant batch can't evict its own
        // entries before an in-flight index build replays them; age out the
        // rest (any build replaying these finishes in far less than 60 s).
        while ring.len() > 2 * BATCH_PATH_CAP {
            ring.pop_front();
        }
        while ring
            .front()
            .map(|(_, _, _, t)| now.duration_since(*t) > Duration::from_secs(60))
            .unwrap_or(false)
        {
            ring.pop_front();
        }
    }
    // Only maps someone has viewed are cached; the rest rebuild lazily. But a
    // viewer sitting at z=0 never builds an index (native tiles are served by
    // stat), so changes for un-indexed maps still need a native tiles event —
    // dropping them silently would freeze exactly that viewer.
    {
        let indexed = st.indexes.read().unwrap();
        let mut unindexed: Vec<(MapId, Vec<(i32, i32)>)> = Vec::new();
        regions.retain(|m, rs| {
            if indexed.contains_key(m) {
                true
            } else {
                unindexed.push((m.clone(), rs.iter().copied().collect()));
                false
            }
        });
        drop(indexed);
        for (map, rs) in unindexed {
            emit_tiles(st, &map, Some(&rs), false);
        }
    }
    if regions.is_empty() {
        return;
    }

    // Stat + clone-and-swap off the async thread. The clone of a huge entries
    // map costs tens of ms once per >=500 ms batch — acceptable, and only
    // when a render holds the Arc (make-mut semantics via explicit clone).
    let st2 = st.clone();
    let worlds2 = worlds.clone();
    let applied: Vec<(MapId, Vec<(i32, i32)>)> =
        tokio::task::spawn_blocking(move || apply_region_changes(&st2, &worlds2, regions, epoch))
            .await
            .unwrap_or_default();

    for (map, rs) in applied {
        emit_tiles(st, &map, Some(&rs), false);
        let slot = th
            .pending_deep
            .entry(map)
            .or_insert_with(|| Some(HashSet::new()));
        if let Some(set) = slot {
            set.extend(rs.iter().copied());
            if set.len() > EVENT_REGION_CAP {
                *slot = None;
            }
        }
    }
}

/// Re-stats each changed region and swaps updated RegionIndex clones into the
/// cache. Returns the changes that actually differed. Runs in spawn_blocking.
fn apply_region_changes(
    st: &AppState,
    worlds: &[WorldEntry],
    changes: HashMap<MapId, HashSet<(i32, i32)>>,
    epoch: u64,
) -> Vec<(MapId, Vec<(i32, i32)>)> {
    let mut out = Vec::new();
    for (map, coords) in changes {
        let Some(idx) = ({
            let guard = st.indexes.read().unwrap();
            guard.get(&map).map(|c| c.index.clone())
        }) else {
            continue;
        };
        let Some(dir) = map_dir(worlds, &map) else {
            continue;
        };
        let mut new_index: Option<xaero_scan::RegionIndex> = None;
        let mut applied = Vec::new();
        // Coordinates new to the index: the overzoom compose finds regions
        // through the spatial buckets, so these must be inserted there too or
        // fresh terrain never shows up in zoomed-out tiles.
        let mut added = Vec::new();
        for (rx, rz) in coords {
            let fresh = stat_region(&dir, rx, rz);
            let old = idx.entries.get(&(rx, rz)).copied();
            if meta_eq(&old, &fresh) {
                continue;
            }
            let ni = new_index.get_or_insert_with(|| (*idx).clone());
            match fresh {
                Some(meta) => {
                    if old.is_none() {
                        added.push((rx, rz));
                    }
                    ni.entries.insert((rx, rz), meta);
                }
                None => {
                    // Stale bucket entries are harmless: the compose drops
                    // coordinates the index no longer knows.
                    ni.entries.remove(&(rx, rz));
                }
            }
            applied.push((rx, rz));
        }
        if let Some(ni) = new_index {
            let mut guard = st.indexes.write().unwrap();
            // A rescan/refresh happened mid-batch: positional MapIds are no
            // longer ours. Drop everything.
            if st.epoch.load(Ordering::SeqCst) != epoch {
                return Vec::new();
            }
            if let Some(cache) = guard.get_mut(&map) {
                cache.index = Arc::new(ni);
                if !added.is_empty() {
                    let mut buckets = (*cache.buckets).clone();
                    for (rx, rz) in added {
                        let cell = (rx.div_euclid(crate::BUCKET), rz.div_euclid(crate::BUCKET));
                        let v = buckets.entry(cell).or_default();
                        if !v.contains(&(rx, rz)) {
                            v.push((rx, rz));
                        }
                    }
                    cache.buckets = Arc::new(buckets);
                }
                out.push((map, applied));
            }
        }
    }
    out
}

/// Preferred container wins, mirroring index_regions: .zip over .xaero.
pub(crate) fn stat_region(dir: &Path, rx: i32, rz: i32) -> Option<RegionMeta> {
    for (ext, is_zip) in [("zip", true), ("xaero", false)] {
        let p = dir.join(format!("{rx}_{rz}.{ext}"));
        if let Ok(md) = std::fs::metadata(&p) {
            let mtime_ms = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            return Some(RegionMeta {
                mtime_ms,
                size: md.len(),
                is_zip,
            });
        }
    }
    None
}

fn meta_eq(a: &Option<RegionMeta>, b: &Option<RegionMeta>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.mtime_ms == b.mtime_ms && a.size == b.size && a.is_zip == b.is_zip,
        _ => false,
    }
}

pub(crate) fn map_dir(worlds: &[WorldEntry], map: &MapId) -> Option<PathBuf> {
    let we = worlds.get(map.world)?;
    let dim = we.world.dims.get(map.dim)?;
    let mw = dim.multiworlds.get(map.mw)?;
    Some(xaero_scan::layer_dir(
        we.world.world_map_path.as_ref()?,
        &dim.folder,
        &mw.id,
        map.cave,
    ))
}

/// Overflow/huge-batch fallback: reindex every hot map, diff, swap, and tell
/// clients to refresh those maps wholesale. Rate-limited hard — this is the
/// expensive path against the live archive.
async fn maybe_degrade(st: &Arc<AppState>, th: &mut Throttle) {
    if th
        .last_degrade
        .map(|t| t.elapsed() < REINDEX_MIN)
        .unwrap_or(false)
    {
        return;
    }
    th.degrade_pending = false;
    th.last_degrade = Some(Instant::now());
    let epoch = st.epoch.load(Ordering::SeqCst);
    let worlds: Arc<Vec<WorldEntry>> = st.worlds.read().unwrap().clone();
    let maps: Vec<MapId> = st.indexes.read().unwrap().keys().cloned().collect();
    eprintln!(
        "live: event overflow — reindexing {} hot map(s)",
        maps.len()
    );
    for map in maps {
        let Some(dir) = map_dir(&worlds, &map) else {
            continue;
        };
        let fresh = tokio::task::spawn_blocking(move || index_regions(&dir))
            .await
            .ok()
            .and_then(|r| r.ok());
        let Some(fresh) = fresh else { continue };
        let mut guard = st.indexes.write().unwrap();
        if st.epoch.load(Ordering::SeqCst) != epoch {
            return;
        }
        if let Some(cache) = guard.get_mut(&map) {
            let changed = diff_indexes(&cache.index.entries, &fresh.entries);
            if changed.is_empty() {
                continue;
            }
            cache.buckets = Arc::new(crate::build_buckets(&fresh));
            cache.index = Arc::new(fresh);
            cache.gen = st.generation.fetch_add(1, Ordering::Relaxed) + 1;
            drop(guard);
            th.last_deep.insert(map.clone(), Instant::now());
            th.pending_deep.remove(&map);
            emit_tiles(st, &map, None, true);
        }
    }
    // No way to know which DBs were in the lost events: queue them all.
    for (w, we) in worlds.iter().enumerate() {
        for db in &we.world.databases {
            th.pending_db.insert((w, db.clone()));
        }
    }
}

/// Whether a map's accumulated overzoom invalidation should broadcast now.
/// `elapsed` None = never flushed before; `pending` None = "assume all".
fn deep_flush_due(elapsed: Option<Duration>, pending: Option<usize>) -> bool {
    let Some(elapsed) = elapsed else { return true };
    if elapsed >= DEEP_FLUSH {
        return true;
    }
    elapsed >= DEEP_FLUSH_FAST && pending.is_some_and(|n| n <= FAST_FLUSH_MAX)
}

fn flush_due(st: &Arc<AppState>, th: &mut Throttle) {
    let now = Instant::now();
    let due: Vec<MapId> = th
        .pending_deep
        .iter()
        .filter(|(m, regions)| {
            let elapsed = th.last_deep.get(*m).map(|t| now.duration_since(*t));
            deep_flush_due(elapsed, regions.as_ref().map(|s| s.len()))
        })
        .map(|(m, _)| m.clone())
        .collect();
    for map in due {
        let regions = th.pending_deep.remove(&map).flatten();
        {
            let mut guard = st.indexes.write().unwrap();
            if let Some(cache) = guard.get_mut(&map) {
                cache.gen = st.generation.fetch_add(1, Ordering::Relaxed) + 1;
            }
        }
        th.last_deep.insert(map.clone(), now);
        let list: Option<Vec<(i32, i32)>> = regions.map(|s| s.into_iter().collect());
        emit_tiles(st, &map, list.as_deref(), true);
    }

    let due: Vec<(usize, String)> = th
        .pending_db
        .iter()
        .filter(|k| {
            th.last_db
                .get(*k)
                .map(|t| now.duration_since(*t) >= DB_EVENT_MIN)
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    for key in due {
        th.pending_db.remove(&key);
        th.last_db.insert(key.clone(), now);
        let seq = st.live.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let msg = serde_json::json!({"type": "db", "w": key.0, "db": key.1, "v": seq});
        let _ = st.live.tx.send(msg.to_string());
    }
}

/// `deep:false` = native-zoom freshness (mtime stamps already updated);
/// `deep:true` = overzoom stamps were bumped too. regions None = "assume all".
pub(crate) fn emit_tiles(
    st: &Arc<AppState>,
    map: &MapId,
    regions: Option<&[(i32, i32)]>,
    deep: bool,
) {
    let seq = st.live.seq.fetch_add(1, Ordering::Relaxed) + 1;
    let regions_json = match regions {
        Some(list) if list.len() <= EVENT_REGION_CAP => serde_json::json!(list),
        _ => serde_json::Value::Null,
    };
    let msg = serde_json::json!({
        "type": "tiles",
        "w": map.world,
        "d": map.dim,
        "m": map.mw,
        "layer": layer_str(map.cave),
        "regions": regions_json,
        "deep": deep,
        "v": seq,
    });
    let _ = st.live.tx.send(msg.to_string());
}

pub(crate) fn emit_state_changed(st: &Arc<AppState>) {
    let seq = st.live.seq.fetch_add(1, Ordering::Relaxed) + 1;
    let _ = st
        .live
        .tx
        .send(serde_json::json!({"type": "state", "v": seq}).to_string());
}

// ---------------------------------------------------------------- watcher --

/// (Re)arms the watcher. Every directory that matters is already known from
/// the world scan (world roots for DBs, layer dirs for regions), so each is
/// watched NON-recursively: no recursive registration walk — which would
/// readdir the ~1M-file dims of the live archive — ever happens. Dirs that
/// appear later (new dim/mw) are picked up by the next roots rescan.
pub(crate) fn arm_watcher(st: &Arc<AppState>, worlds: &[WorldEntry]) -> Result<usize, String> {
    // Drop the previous watcher first so stale watches stop firing.
    *st.watcher.lock().unwrap() = None;
    let tx = st.live.fs_tx.clone();
    let seen = st.live.events_seen.clone();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            seen.fetch_add(1, Ordering::Relaxed);
            match res {
                Ok(ev) => {
                    if ev.need_rescan() {
                        let _ = tx.send(FsEvent::Overflow);
                        return;
                    }
                    use notify::EventKind;
                    if matches!(
                        ev.kind,
                        EventKind::Create(_)
                            | EventKind::Modify(_)
                            | EventKind::Remove(_)
                            | EventKind::Any
                    ) {
                        for p in ev.paths {
                            let _ = tx.send(FsEvent::Path(p));
                        }
                    }
                }
                Err(_) => {
                    let _ = tx.send(FsEvent::Overflow);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    let mut dirs: Vec<PathBuf> = Vec::new();
    for we in worlds {
        let Some(wm) = &we.world.world_map_path else {
            continue;
        };
        dirs.push(wm.clone());
        for dim in &we.world.dims {
            for mw in &dim.multiworlds {
                dirs.push(xaero_scan::layer_dir(wm, &dim.folder, &mw.id, None));
                for cave in &mw.cave_layers {
                    dirs.push(xaero_scan::layer_dir(wm, &dim.folder, &mw.id, Some(*cave)));
                }
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    let total = dirs.len();
    let mut watched = 0usize;
    for dir in &dirs {
        match watcher.watch(dir, notify::RecursiveMode::NonRecursive) {
            Ok(()) => watched += 1,
            Err(e) => eprintln!("live: cannot watch {}: {e}", dir.display()),
        }
    }
    if watched == 0 && total > 0 {
        return Err("no directory could be watched".into());
    }
    *st.watcher.lock().unwrap() = Some(watcher);
    Ok(watched)
}

/// Detects a registered-but-dead watcher (the ntfs3 worry): if known WAL
/// files advance for two consecutive minutes with zero watcher callbacks,
/// switch to poll mode.
pub(crate) async fn watchdog_loop(st: Arc<AppState>) {
    if st.live.watchdog_running.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut wal_mtimes: HashMap<PathBuf, u64> = HashMap::new();
    let mut last_seen = st.live.events_seen.load(Ordering::Relaxed);
    let mut strikes = 0u32;
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.tick().await;
    loop {
        tick.tick().await;
        if st.watcher.lock().unwrap().is_none() {
            st.live.watchdog_running.store(false, Ordering::SeqCst);
            return; // already in poll mode
        }
        let worlds = st.worlds.read().unwrap().clone();
        let mut advanced = false;
        for we in worlds.iter() {
            let Some(wm) = &we.world.world_map_path else {
                continue;
            };
            for db in &we.world.databases {
                let wal = wm.join(format!("{db}-wal"));
                let m = config::mtime_ms(&wal);
                if m == 0 {
                    continue;
                }
                let e = wal_mtimes.entry(wal).or_insert(m);
                if m > *e {
                    advanced = true;
                    *e = m;
                }
            }
        }
        let seen = st.live.events_seen.load(Ordering::Relaxed);
        if advanced && seen == last_seen {
            strikes += 1;
        } else {
            strikes = 0;
        }
        last_seen = seen;
        if strikes >= 2 {
            eprintln!(
                "live: files are changing but the watcher sees nothing — switching to poll mode"
            );
            *st.watcher.lock().unwrap() = None;
            tokio::spawn(poll_loop(st.clone()));
            st.live.watchdog_running.store(false, Ordering::SeqCst);
            return;
        }
    }
}

struct PollDir {
    mtime: u64,
    dirty: bool,
    last_reindex: Option<Instant>,
}

/// Fallback when inotify is unavailable or dead: re-stat only hot state —
/// the layer dirs of currently-indexed maps (dir mtime bumps on Xaero's
/// temp+rename) and the known DB/WAL files. A changed dir is reindexed only
/// after its mtime settles and at most every REINDEX_MIN; the diff is fed
/// through the normal event pipeline.
pub(crate) async fn poll_loop(st: Arc<AppState>) {
    if st.live.poll_running.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut dirs: HashMap<PathBuf, PollDir> = HashMap::new();
    let mut db_mtimes: HashMap<PathBuf, u64> = HashMap::new();
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    loop {
        tick.tick().await;
        let worlds = st.worlds.read().unwrap().clone();
        let maps: Vec<(MapId, PathBuf)> = {
            let guard = st.indexes.read().unwrap();
            guard
                .keys()
                .filter_map(|m| Some((m.clone(), map_dir(&worlds, m)?)))
                .collect()
        };
        for (map, dir) in maps {
            let mtime = config::mtime_ms(&dir);
            let entry = dirs.entry(dir.clone()).or_insert(PollDir {
                mtime,
                dirty: false,
                last_reindex: None,
            });
            if mtime != entry.mtime {
                entry.mtime = mtime;
                entry.dirty = true; // wait for the mtime to settle first
                continue;
            }
            if !entry.dirty
                || entry
                    .last_reindex
                    .map(|t| t.elapsed() < REINDEX_MIN)
                    .unwrap_or(false)
            {
                continue;
            }
            entry.dirty = false;
            entry.last_reindex = Some(Instant::now());
            let Some(current) = ({
                let guard = st.indexes.read().unwrap();
                guard.get(&map).map(|c| c.index.clone())
            }) else {
                continue;
            };
            let dir2 = dir.clone();
            let fresh = tokio::task::spawn_blocking(move || index_regions(&dir2))
                .await
                .ok()
                .and_then(|r| r.ok());
            let Some(fresh) = fresh else { continue };
            for (rx, rz) in diff_indexes(&current.entries, &fresh.entries) {
                // The debouncer re-stats, so the synthesized extension is moot.
                let _ = st
                    .live
                    .fs_tx
                    .send(FsEvent::Path(dir.join(format!("{rx}_{rz}.zip"))));
            }
        }
        for we in worlds.iter() {
            let Some(wm) = &we.world.world_map_path else {
                continue;
            };
            for db in &we.world.databases {
                for suffix in ["", "-wal"] {
                    let p = wm.join(format!("{db}{suffix}"));
                    let m = config::mtime_ms(&p);
                    let e = db_mtimes.entry(p).or_insert(m);
                    if m > *e {
                        *e = m;
                        let _ = st.live.fs_tx.send(FsEvent::Path(wm.join(db)));
                    }
                }
            }
        }
    }
}

// --------------------------------------------------------------- /ws/live --

pub(crate) async fn ws_live(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // WebSockets bypass the same-origin policy: a hostile page could open
    // ws://127.0.0.1 and stream live 2b2t positions. Browser clients must be
    // same-origin; non-browser clients send no Origin and pass.
    if !origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin websocket rejected").into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(st, socket))
}

pub(crate) fn origin_ok(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .map(|h| !host.is_empty() && h == host)
        .unwrap_or(false)
}

async fn handle_socket(st: Arc<AppState>, socket: WebSocket) {
    // Subscribe before snapshotting so nothing lands in the gap; a duplicate
    // pos straddling the snapshot is idempotent on the client.
    let mut rx = st.live.tx.subscribe();
    let hello = {
        let positions = st.live.positions.lock().unwrap();
        let players: Vec<serde_json::Value> = positions
            .iter()
            .map(|(name, p)| pos_value(name, p))
            .collect();
        serde_json::json!({
            "type": "hello",
            "players": players,
            "v": st.live.seq.load(Ordering::Relaxed),
        })
        .to_string()
    };
    let (mut sink, mut stream) = socket.split();
    if sink.send(Message::Text(hello.into())).await.is_err() {
        return;
    }
    let mut send_task = tokio::spawn(async move {
        // A lagged receiver skips what it missed and tells the client to
        // refresh its layers in place — dropping the socket made every
        // reconnecting viewer blank and redraw the whole screen instead.
        let mut hb = tokio::time::interval(WS_HEARTBEAT);
        hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        hb.reset(); // the first tick fires immediately; the hello just went out
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(msg) => {
                        if sink.send(Message::Text(msg.into())).await.is_err() {
                            break;
                        }
                        hb.reset(); // real traffic proves liveness on its own
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let resync = serde_json::json!({"type": "resync"}).to_string();
                        if sink.send(Message::Text(resync.into())).await.is_err() {
                            break;
                        }
                        hb.reset();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = hb.tick() => {
                    let msg = serde_json::json!({"type": "hb"}).to_string();
                    if sink.send(Message::Text(msg.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut recv_task = tokio::spawn(async move { while let Some(Ok(_)) = stream.next().await {} });
    tokio::select! {
        _ = &mut send_task => recv_task.abort(),
        _ = &mut recv_task => send_task.abort(),
    }
}

fn pos_value(player: &str, p: &PlayerPos) -> serde_json::Value {
    serde_json::json!({
        "type": "pos",
        "player": player,
        "dim": p.dim,
        "x": p.x,
        "y": p.y,
        "z": p.z,
        "yaw": p.yaw,
        "t": p.t_ms,
    })
}

// ------------------------------------------------- POST /ingest/v1/position --

#[derive(serde::Deserialize)]
pub(crate) struct PositionReq {
    player: String,
    dim: String,
    x: f64,
    y: f64,
    z: f64,
    yaw: f32,
}

/// Tokenless ingest: a loopback peer may declare its own player name. Tokens
/// gate *remote* connections — a local process already owns this server (it
/// can read the config file the tokens live in), so demanding one of it adds
/// setup without adding security. Browser pages don't get the exemption:
/// cross-origin requests are rejected, and both ingest routes require
/// non-simple content types (JSON / octet-stream) anyway, which no hostile
/// page can send to us without a CORS preflight we never answer.
pub(crate) fn local_player(
    headers: &HeaderMap,
    peer: SocketAddr,
    declared: Option<&str>,
) -> Result<String, (StatusCode, &'static str)> {
    if !peer.ip().to_canonical().is_loopback() {
        return Err((StatusCode::UNAUTHORIZED, "missing bearer token"));
    }
    if !origin_ok(headers) {
        return Err((StatusCode::FORBIDDEN, "cross-origin ingest rejected"));
    }
    let name = declared.unwrap_or("").trim();
    if name.is_empty() {
        return Err((
            StatusCode::UNAUTHORIZED,
            "missing player name — declare one (X-XT-Player for region uploads) or send a bearer token",
        ));
    }
    if !crate::ingest::safe_segment(name) {
        return Err((
            StatusCode::BAD_REQUEST,
            "player name is not filesystem-safe",
        ));
    }
    Ok(name.to_string())
}

/// Resolves the acting player for an ingest request. A presented token must
/// be valid even from loopback — a revoked or mistyped token fails loudly
/// instead of silently falling back; only a request with *no* token at all
/// takes the loopback path.
// The Err is a ready axum Response, built only on rejection — cold enough
// that boxing it away would be noise (clippy 1.98 result_large_err).
#[allow(clippy::result_large_err)]
pub(crate) async fn ingest_player(
    st: &AppState,
    headers: &HeaderMap,
    peer: SocketAddr,
    declared: Option<&str>,
) -> Result<String, Response> {
    // Hot-reload the config on every attempt (stat throttled to 1/s) so
    // `tokens generate` works immediately and `tokens revoke` actually revokes.
    maybe_reload_config(st);
    let Some(token) = bearer_token(headers) else {
        return local_player(headers, peer, declared).map_err(|e| e.into_response());
    };
    let player = st
        .config
        .lock()
        .unwrap()
        .file
        .player_for_token(token)
        .map(str::to_string);
    let Some(player) = player else {
        // Linear backoff against token guessing, mirroring the login path.
        let backoff = st.live.note_auth_failure();
        tokio::time::sleep(Duration::from_millis(backoff)).await;
        return Err((StatusCode::UNAUTHORIZED, "unknown token").into_response());
    };
    st.live.note_auth_ok();
    Ok(player)
}

pub(crate) async fn ingest_position(
    State(st): State<Arc<AppState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: axum::Json<PositionReq>,
) -> Response {
    let player = match ingest_player(&st, &headers, peer, Some(&body.player)).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    if body.player != player {
        return (StatusCode::FORBIDDEN, "token belongs to a different player").into_response();
    }
    let Some(dim) = normalize_dim(&body.dim) else {
        return (StatusCode::BAD_REQUEST, "unrecognized dim").into_response();
    };
    if !coords_ok(&body) {
        return (StatusCode::BAD_REQUEST, "coordinates out of range").into_response();
    }
    {
        let mut rate = st.live.rate.lock().unwrap();
        let bucket = rate.entry(player.clone()).or_insert(Bucket {
            tokens: RATE_BURST,
            last_ms: now_ms(),
        });
        if !bucket_allow(bucket, now_ms(), RATE_PER_SEC, RATE_BURST) {
            return (StatusCode::TOO_MANY_REQUESTS, "rate limited").into_response();
        }
    }
    let pos = PlayerPos {
        dim,
        x: body.x,
        y: body.y,
        z: body.z,
        yaw: body.yaw,
        t_ms: now_ms(),
    };
    let msg = pos_value(&player, &pos).to_string();
    st.live.positions.lock().unwrap().insert(player, pos);
    let _ = st.live.tx.send(msg);
    StatusCode::NO_CONTENT.into_response()
}

pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

pub(crate) fn maybe_reload_config(st: &AppState) {
    let mut cache = st.config.lock().unwrap();
    if cache.last_stat.elapsed() < Duration::from_secs(1) {
        return;
    }
    cache.last_stat = Instant::now();
    let mtime = config::mtime_ms(&st.config_path);
    if mtime != cache.mtime {
        match config::load(&st.config_path) {
            Ok(file) => {
                cache.file = file;
                cache.mtime = mtime;
            }
            Err(e) => eprintln!("config reload failed: {e}"),
        }
    }
}

pub(crate) fn normalize_dim(d: &str) -> Option<String> {
    match d.trim() {
        "overworld" | "minecraft:overworld" => Some("minecraft:overworld".into()),
        "nether" | "the_nether" | "minecraft:the_nether" => Some("minecraft:the_nether".into()),
        "end" | "the_end" | "minecraft:the_end" => Some("minecraft:the_end".into()),
        other => {
            let (ns, path) = other.split_once(':')?;
            let ok = |s: &str| {
                !s.is_empty()
                    && s.len() <= 128
                    && s.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-./".contains(c))
            };
            (ok(ns) && ok(path)).then(|| other.to_string())
        }
    }
}

fn coords_ok(p: &PositionReq) -> bool {
    p.x.is_finite()
        && p.z.is_finite()
        && p.y.is_finite()
        && p.yaw.is_finite()
        && p.x.abs() <= 40_000_000.0
        && p.z.abs() <= 40_000_000.0
        && (-1024.0..=4096.0).contains(&p.y)
}

pub(crate) struct Bucket {
    tokens: f64,
    last_ms: u64,
}

impl Bucket {
    pub(crate) fn new(burst: f64, now_ms: u64) -> Bucket {
        Bucket {
            tokens: burst,
            last_ms: now_ms,
        }
    }
}

pub(crate) fn bucket_allow(b: &mut Bucket, now_ms: u64, rate: f64, burst: f64) -> bool {
    let dt = now_ms.saturating_sub(b.last_ms) as f64 / 1000.0;
    b.tokens = (b.tokens + dt * rate).min(burst);
    b.last_ms = now_ms;
    if b.tokens >= 1.0 {
        b.tokens -= 1.0;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xaero_scan::{DimEntry, MwEntry};

    fn test_worlds() -> Vec<WorldEntry> {
        let world = World {
            id: "Multiplayer_test".into(),
            world_map_path: Some(PathBuf::from("/data/world-map/Multiplayer_test")),
            minimap_path: None,
            dims: vec![
                DimEntry {
                    folder: "null".into(),
                    dimension: None,
                    config: Default::default(),
                    multiworlds: vec![
                        MwEntry {
                            id: "mw$default".into(),
                            display: "mw$default".into(),
                            cave_layers: vec![],
                        },
                        MwEntry {
                            id: "mw$123".into(),
                            display: "mw$123".into(),
                            cave_layers: vec![],
                        },
                    ],
                },
                DimEntry {
                    folder: "DIM-1".into(),
                    dimension: None,
                    config: Default::default(),
                    multiworlds: vec![MwEntry {
                        id: "mw$default".into(),
                        display: "mw$default".into(),
                        cave_layers: vec![7],
                    }],
                },
            ],
            databases: vec!["XaeroPlusNewChunks.db".into()],
            waypoint_files: vec![],
        };
        vec![WorldEntry {
            root: PathBuf::from("/data"),
            world,
        }]
    }

    #[test]
    fn classify_paths() {
        let worlds = test_worlds();
        let base = Path::new("/data/world-map/Multiplayer_test");

        let region = classify(&worlds, &base.join("null/mw$default/12_-34.zip"));
        assert_eq!(
            region,
            Some(Change::Region {
                map: MapId {
                    world: 0,
                    dim: 0,
                    mw: 0,
                    cave: None,
                    roof: None
                },
                rx: 12,
                rz: -34
            })
        );

        let cave = classify(&worlds, &base.join("DIM-1/mw$default/caves/7/1_2.xaero"));
        assert_eq!(
            cave,
            Some(Change::Region {
                map: MapId {
                    world: 0,
                    dim: 1,
                    mw: 0,
                    cave: Some(7),
                    roof: None
                },
                rx: 1,
                rz: 2
            })
        );

        let second_mw = classify(&worlds, &base.join("null/mw$123/0_0.zip"));
        assert!(matches!(
            second_mw,
            Some(Change::Region {
                map: MapId { mw: 1, .. },
                ..
            })
        ));

        for (name, want_db) in [
            ("XaeroPlusNewChunks.db", true),
            ("XaeroPlusNewChunks.db-wal", true),
            ("XaeroPlusNewChunks.db-shm", true),
            ("SomeUnknown.db", false),
        ] {
            let got = classify(&worlds, &base.join(name));
            if want_db {
                assert_eq!(
                    got,
                    Some(Change::Db {
                        w: 0,
                        db: "XaeroPlusNewChunks.db".into()
                    }),
                    "{name}"
                );
            } else {
                assert_eq!(got, None, "{name}");
            }
        }

        // Temp/partial names and foreign paths are ignored.
        assert_eq!(
            classify(&worlds, &base.join("null/mw$default/12_-34.zip.temp")),
            None
        );
        assert_eq!(
            classify(&worlds, &base.join("null/mw$default/cache_1.zip")),
            None
        );
        assert_eq!(classify(&worlds, Path::new("/elsewhere/1_1.zip")), None);
        assert_eq!(
            classify(&worlds, &base.join("null/unknown-mw/1_1.zip")),
            None
        );
    }

    #[test]
    fn diff_detects_add_change_remove() {
        let meta = |mtime| RegionMeta {
            mtime_ms: mtime,
            size: 10,
            is_zip: true,
        };
        let old: HashMap<_, _> = [((0, 0), meta(1)), ((1, 1), meta(1)), ((2, 2), meta(1))].into();
        let new: HashMap<_, _> = [((0, 0), meta(1)), ((1, 1), meta(2)), ((3, 3), meta(1))].into();
        let mut diff = diff_indexes(&old, &new);
        diff.sort();
        assert_eq!(diff, vec![(1, 1), (2, 2), (3, 3)]);
    }

    #[test]
    fn bucket_limits_burst_and_recovers() {
        let mut b = Bucket {
            tokens: RATE_BURST,
            last_ms: 0,
        };
        let allowed = (0..20)
            .filter(|_| bucket_allow(&mut b, 1000, 5.0, 10.0))
            .count();
        assert_eq!(allowed, 10); // refill is capped at the burst size
        assert!(!bucket_allow(&mut b, 1000, 5.0, 10.0));
        assert!(bucket_allow(&mut b, 2000, 5.0, 10.0)); // refilled
    }

    #[test]
    fn local_player_gate() {
        let lo: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let lo6: SocketAddr = "[::1]:5000".parse().unwrap();
        let lan: SocketAddr = "192.168.1.50:5000".parse().unwrap();
        let none = HeaderMap::new();

        assert_eq!(
            local_player(&none, lo, Some("Account1")).unwrap(),
            "Account1"
        );
        assert_eq!(
            local_player(&none, lo6, Some("Account1")).unwrap(),
            "Account1"
        );
        // Remote peers must present a token; the declared name buys nothing.
        assert_eq!(
            local_player(&none, lan, Some("Account1")).unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        // No usable name, or one that can't become a directory.
        assert!(local_player(&none, lo, None).is_err());
        assert!(local_player(&none, lo, Some("  ")).is_err());
        assert!(local_player(&none, lo, Some("../evil")).is_err());
        // A browser page on another origin is not "local".
        let mut cross = HeaderMap::new();
        cross.insert(header::ORIGIN, "http://evil.example".parse().unwrap());
        cross.insert(header::HOST, "127.0.0.1:45746".parse().unwrap());
        assert_eq!(
            local_player(&cross, lo, Some("Account1")).unwrap_err().0,
            StatusCode::FORBIDDEN
        );
        // The viewer's own origin passes, as does a non-browser client (no Origin).
        let mut same = HeaderMap::new();
        same.insert(header::ORIGIN, "http://127.0.0.1:45746".parse().unwrap());
        same.insert(header::HOST, "127.0.0.1:45746".parse().unwrap());
        assert!(local_player(&same, lo, Some("Account1")).is_ok());
    }

    #[test]
    fn deep_flush_fast_path() {
        let s = Duration::from_secs;
        // Never flushed before: due immediately.
        assert!(deep_flush_due(None, Some(1)));
        // Small pending set rides the fast timer.
        assert!(!deep_flush_due(Some(s(1)), Some(5)));
        assert!(deep_flush_due(Some(s(3)), Some(5)));
        // Big or "assume all" sets wait the full interval.
        assert!(!deep_flush_due(Some(s(3)), Some(FAST_FLUSH_MAX + 1)));
        assert!(!deep_flush_due(Some(s(3)), None));
        assert!(deep_flush_due(Some(s(10)), None));
    }

    #[test]
    fn remove_player_clears_and_broadcasts() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let st = LiveState::new(tx);
        st.positions.lock().unwrap().insert(
            "Test".into(),
            PlayerPos {
                dim: "minecraft:overworld".into(),
                x: 0.0,
                y: 64.0,
                z: 0.0,
                yaw: 0.0,
                t_ms: 1,
            },
        );
        let mut sub = st.tx.subscribe();
        assert!(st.remove_player("Test"));
        assert!(st.positions.lock().unwrap().is_empty());
        let msg = sub.try_recv().unwrap();
        assert!(msg.contains("player_removed") && msg.contains("Test"));
        // Already gone: not an event, not a success.
        assert!(!st.remove_player("Test"));
        assert!(sub.try_recv().is_err());
    }

    #[test]
    fn dim_normalization() {
        assert_eq!(
            normalize_dim("overworld").as_deref(),
            Some("minecraft:overworld")
        );
        assert_eq!(
            normalize_dim("minecraft:the_nether").as_deref(),
            Some("minecraft:the_nether")
        );
        assert_eq!(
            normalize_dim("mymod:custom_world").as_deref(),
            Some("mymod:custom_world")
        );
        assert_eq!(normalize_dim("no-colon"), None);
        assert_eq!(normalize_dim("bad:UPPER"), None);
        assert_eq!(normalize_dim(""), None);
    }
}
