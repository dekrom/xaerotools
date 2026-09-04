//! xaerotools-server — local web server: embedded UI + tile/waypoint APIs.
//!
//! Tile identity: one Xaero region = one native tile at z=0 (512px).
//! Negative z zooms out (a tile covers 2^-z regions per axis): z in -3..=-1
//! composes child tiles; deeper zooms render instant coverage rectangles from
//! the filename index until a persistent pyramid exists.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use axum::extract::{Path as AxPath, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use include_dir::{include_dir, Dir};
use serde::Serialize;
use xaero_core::naming::Dimension;
use xaero_core::render::{ColorTable, RenderOpts};
use xaero_core::waypoints::{parse_waypoints_file, waypoint_color_rgb};
use xaero_scan::{index_regions, layer_dir, RegionIndex, World};

pub mod config;
mod highlights;
mod ingest;
mod live;
mod preview;
mod pyramid;

static WEBUI: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../webui/dist");
static COLORTABLE: &[u8] = include_bytes!("../../../assets/colortable.bin");

const TILE: usize = 512;
/// Edge of a cached per-region thumbnail. 32px is the cell size at z=-4, so
/// every zoom from -4 outwards composes by downscaling thumbnails instead of
/// decoding regions. Deliberately keyed by the region's mtime rather than the
/// map generation: a live edit elsewhere in the map must not throw these away.
const THUMB: usize = 32;
/// Edge of the second, deep-zoom thumbnail tier. At z=-6 a region is an 8px
/// cell, so every zoom from -6 outwards composes from these. Sixteen times
/// smaller than a THUMB (256 B vs 4 KiB), which is what lets the deep-zoom
/// cache hold a whole million-region archive in RAM: the 32px tier alone
/// cannot, and evictions there were what made zoomed-out imagery flap between
/// renders and coverage rectangles.
const MIP: usize = 8;
/// Byte budget of the MIP-tier cache (~1M regions at 256 B + overhead).
const MIP_CACHE_BYTES: usize = 320 << 20;
/// Cold regions a single request will decode inline before it prefers to answer
/// with coverage rectangles and warm the rest in the background. Keeps the
/// worst-case tile latency bounded instead of blocking a viewer for a minute.
const WARM_SYNC_BUDGET: usize = 1024;
/// Ceiling on how many regions one background warm-up job will render.
const WARM_JOB_CAP: usize = 8192;
/// Warm requests remembered at once, newest first. A request made while the
/// worker is busy queues instead of being dropped, so the area on screen
/// right now always gets its turn; only the oldest views lose their slot.
const WARM_QUEUE_CAP: usize = 16;
/// Regions rendered per warm chunk. Every finished chunk bumps the map
/// generation and tells viewers to refresh exactly those regions, so imagery
/// replaces coverage rectangles progressively instead of after the whole job.
const WARM_CHUNK: usize = 256;
/// Regions per side of a spatial bucket. Zoomed-out tiles are power-of-two
/// aligned, so a tile either sits inside one bucket or spans whole buckets —
/// either way the lookup is O(regions in range), never O(regions in map).
pub(crate) const BUCKET: i32 = 64;

#[derive(Clone)]
pub struct ServerConfig {
    pub roots: Vec<PathBuf>,
    pub bind: SocketAddr,
    /// Max encoded tiles kept in the in-memory LRU.
    pub tile_cache_entries: usize,
    /// Byte ceiling for the encoded-tile LRU.
    pub tile_cache_bytes: usize,
    /// Max region thumbnails kept for the zoomed-out pyramid.
    pub thumb_cache_entries: usize,
    /// Byte ceiling for the thumbnail cache.
    pub thumb_cache_bytes: usize,
    /// Require this password (cookie session) for every request. Meant for
    /// `--lan` mode; plain HTTP, so treat it as LAN/VPN protection only.
    pub password: Option<String>,
    /// Waypoint-vault database location (None = platform default).
    pub vault_path: Option<PathBuf>,
    /// Local 2b2t Atlas tile mirror (scripts/atlas-mirror.py output).
    /// None = platform default next to the vault; served only if present.
    pub atlas_dir: Option<PathBuf>,
    /// Persistent config (runtime roots + ingest tokens); None = platform
    /// default next to the vault.
    pub config_path: Option<PathBuf>,
    /// Where region uploads land (per-player backups + the merged tree);
    /// None = `ingest/` next to the config file. Must sit outside every
    /// scanned root — it is the one place the server writes region data.
    pub ingest_dir: Option<PathBuf>,
    /// Refuse cave-layer region uploads (`cave=N`) with 403 — the ingest
    /// trees then only ever hold surface data, whatever clients send.
    pub ingest_no_caves: bool,
    /// Require a bearer token from every ingest client, loopback included.
    /// The tokenless loopback exemption keys off the TCP peer address, which a
    /// reverse proxy on the same machine turns into "everyone".
    pub ingest_require_token: bool,
    /// Force the poll fallback instead of inotify watches.
    pub live_poll: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            roots: Vec::new(),
            bind: ([127, 0, 0, 1], 45746).into(),
            tile_cache_entries: 4096,
            tile_cache_bytes: 512 << 20,
            // A thumbnail is THUMB*THUMB*4 = 4 KiB, so 256 MiB holds ~65k
            // regions — enough to keep a whole browsing session warm.
            thumb_cache_entries: 200_000,
            thumb_cache_bytes: 256 << 20,
            password: None,
            vault_path: None,
            atlas_dir: None,
            config_path: None,
            ingest_dir: None,
            ingest_no_caves: false,
            ingest_require_token: false,
            live_poll: false,
        }
    }
}

pub struct WorldEntry {
    pub root: PathBuf,
    pub world: World,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MapId {
    world: usize,
    dim: usize,
    mw: usize,
    cave: Option<i32>,
    /// See-through-roof opacities (obsidian, snow) when the viewer asked for
    /// that view. Part of the map's identity because it is different imagery
    /// of the same regions: every tile, thumbnail and store row keys on it,
    /// so the two views cannot be served for one another.
    roof: Option<(u8, u8)>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct TileKey {
    map: MapId,
    /// Set for highlight-overlay tiles: the XaeroPlus DB filename.
    db: Option<String>,
    /// Highlight-overlay tiles only: the RGB the rows were painted in. The
    /// colour is baked into the PNG, so it is part of the tile's identity —
    /// without it, recolouring an overlay would serve back the old colour.
    tint: u32,
    z: i32,
    x: i32,
    y: i32,
    stamp: u64,
}

type EncodedTile = Option<Arc<Vec<u8>>>; // None = empty/transparent (204)
type SharedDb = Arc<Mutex<xaero_db::HighlightDb>>;

/// A region's downscaled thumbnail. Keyed by mtime, so it stays valid until
/// that specific region is rewritten.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ThumbKey {
    map: MapId,
    rx: i32,
    rz: i32,
    mtime_ms: u64,
}

/// LRU with a byte budget as well as an entry cap. A 512x512 RGBA tile encodes
/// to a few hundred KB, so an entry-count-only bound let the cache reach
/// gigabytes on a real archive; each entry carries its own size so eviction
/// accounts exactly.
struct ByteLru<K: std::hash::Hash + Eq, V> {
    inner: lru::LruCache<K, (V, usize)>,
    bytes: usize,
    budget: usize,
}

impl<K: std::hash::Hash + Eq, V> ByteLru<K, V> {
    fn new(entries: usize, budget: usize) -> Self {
        ByteLru {
            inner: lru::LruCache::new(NonZeroUsize::new(entries.max(64)).unwrap()),
            bytes: 0,
            budget: budget.max(8 << 20),
        }
    }

    fn get(&mut self, k: &K) -> Option<&V> {
        self.inner.get(k).map(|(v, _)| v)
    }

    fn put(&mut self, k: K, v: V, size: usize) {
        if let Some((_, (_, old))) = self.inner.push(k, (v, size)) {
            self.bytes = self.bytes.saturating_sub(old);
        }
        self.bytes = self.bytes.saturating_add(size);
        while self.bytes > self.budget {
            match self.inner.pop_lru() {
                Some((_, (_, old))) => self.bytes = self.bytes.saturating_sub(old),
                None => break,
            }
        }
    }

    fn clear(&mut self) {
        self.inner.clear();
        self.bytes = 0;
    }

    fn len(&self) -> usize {
        self.inner.len()
    }

    fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Lock order (never hold two across a call that takes a later one):
/// config -> worlds -> indexes -> dbs -> inflight -> tiles. Slow work (index builds,
/// renders, rescans) must snapshot the worlds Arc and drop the guard first.
pub struct AppState {
    worlds: RwLock<Arc<Vec<WorldEntry>>>,
    ct: ColorTable,
    indexes: RwLock<HashMap<MapId, MapCache>>,
    tiles: Mutex<ByteLru<TileKey, EncodedTile>>,
    /// Per-region thumbnails, the level-of-detail pyramid's storage. Survives
    /// map-generation bumps because its key carries the region's own mtime.
    thumbs: Mutex<ByteLru<ThumbKey, Arc<Vec<u8>>>>,
    /// Deep-zoom tier: MIP-sized (8px) per-region thumbnails, same keying.
    mips: Mutex<ByteLru<ThumbKey, Arc<Vec<u8>>>>,
    /// On-disk thumbnail store (None = unavailable; memory-only fallback).
    pyramid: Option<pyramid::PyramidStore>,
    /// The stamp of the last overzoom tile that rendered real imagery, per
    /// tile. When a re-render would fall back to coverage rectangles (cold
    /// caches after eviction or restart), the cached imagery at this stamp is
    /// served instead while the warm queue rebuilds — regions that were on
    /// screen must never dissolve into rectangles and "come back later".
    stale_stamps: Mutex<HashMap<(MapId, i32, i32, i32), u64>>,
    /// Regions that would not decode, newest first. A bad region leaves a hole
    /// in the map rather than failing the tile, so this is the only way the
    /// user finds out something is wrong; surfaced by /api/diagnostics.
    unreadable: Mutex<VecDeque<(String, String)>>,
    /// Tiles currently being rendered. A viewer that pans back and forth asks
    /// for the same expensive tile repeatedly; without this every request
    /// re-renders it in parallel and they all burn cores for one answer.
    inflight: Mutex<HashMap<TileKey, Arc<TileSlot>>>,
    /// Region indexes currently being built, so the first zoomed-out view
    /// (six concurrent tile requests) runs one full-folder scan, not six.
    index_inflight: Mutex<HashMap<MapId, Arc<IndexSlot>>>,
    /// Store-write count as of the last bbox prefetch, per prefetch key.
    /// A repeat prefetch with an unchanged count is skipped — during a warm-up
    /// every re-fetch of a deep tile used to re-materialize the same
    /// (potentially huge) bbox from pyramid.db.
    prefetched: Mutex<HashMap<PrefetchKey, u64>>,
    /// Background pyramid warm-up queue: one worker thread, newest request
    /// first, so the zoom level being looked at right now is always the next
    /// thing rendered and a busy worker never makes a view lose its turn.
    warm: WarmQueue,
    /// Open read-only XaeroPlus DB handles, keyed by (world, db filename).
    dbs: Mutex<HashMap<(usize, String), SharedDb>>,
    /// Waypoint vault (None when it failed to open; viewer degrades).
    vault: Option<Arc<Mutex<xaero_db::vault::Vault>>>,
    /// Monotonic stamp allocator for overzoom tiles. Values never repeat, so
    /// an in-flight render inserting under an old stamp can never collide
    /// with a rebuilt map after /api/refresh or a roots rescan.
    generation: AtomicU64,
    /// Bumped by roots rescans and /api/refresh. Watcher batches classified
    /// under an older epoch are dropped — MapId is positional.
    epoch: AtomicU64,
    /// Local Atlas tile mirror root (None = not mirrored).
    atlas_dir: Option<PathBuf>,
    /// Datasets found in the mirror (dirs carrying a meta.json).
    atlas_sets: Vec<AtlasSet>,
    /// Merge tools write to disk, so they exist for the local user only:
    /// false whenever the server is password-protected (`--lan`).
    tools_enabled: bool,
    jobs: Mutex<HashMap<u64, Arc<Job>>>,
    next_job: AtomicU64,
    live: live::LiveState,
    ingest: ingest::IngestState,
    preview: preview::PreviewState,
    /// Where region uploads are stored; its `players/*` and `merged` subdirs
    /// are served as auto-managed roots.
    ingest_dir: PathBuf,
    /// Refuse `cave=N` region uploads (see ServerConfig::ingest_no_caves).
    ingest_no_caves: bool,
    /// Whether a tokenless loopback peer may ingest as a self-declared player.
    /// Only while the server itself listens on loopback — under `--lan`, or
    /// behind a reverse proxy that makes every client a loopback peer, the
    /// address proves nothing — and not under `--ingest-require-token`.
    loopback_exempt: bool,
    /// Roots from --root flags: session-only, never persisted.
    cli_roots: Vec<PathBuf>,
    config_path: PathBuf,
    config: Mutex<ConfigCache>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// Recently changed regions, replayed onto freshly built indexes to close
    /// the change-lands-during-a-slow-index-build race.
    recent: Mutex<VecDeque<(MapId, i32, i32, std::time::Instant)>>,
}

struct MapCache {
    index: Arc<RegionIndex>,
    /// Cache stamp for this map's z<0 tiles; allocated from `generation`.
    gen: u64,
    /// Regions bucketed into a BUCKET x BUCKET grid so a zoomed-out tile can
    /// find what it covers without scanning the whole index.
    buckets: Arc<RegionBuckets>,
}

/// One bbox prefetch's identity: layer dir, tile origin, span, thumb tier.
type PrefetchKey = (String, i64, i64, i64, bool);

/// Regions grouped by BUCKET-sized grid cell.
pub(crate) type RegionBuckets = HashMap<(i32, i32), Vec<(i32, i32)>>;

/// Buckets every indexed region by its BUCKET-sized grid cell.
pub(crate) fn build_buckets(index: &RegionIndex) -> RegionBuckets {
    let mut out: RegionBuckets = HashMap::new();
    for &(rx, rz) in index.entries.keys() {
        out.entry((rx.div_euclid(BUCKET), rz.div_euclid(BUCKET)))
            .or_default()
            .push((rx, rz));
    }
    out
}

struct ConfigCache {
    file: config::FileConfig,
    /// (mtime, length) of the file the cache was loaded from.
    stamp: (u64, u64),
    last_stat: std::time::Instant,
}

/// A background tool run (merge plan/apply). Polled via /api/jobs/{id}.
struct Job {
    started: std::time::Instant,
    state: Mutex<JobState>,
}

enum JobState {
    Running,
    Done(serde_json::Value),
    Failed(String),
}

fn spawn_job<F>(st: &Arc<AppState>, work: F) -> u64
where
    F: FnOnce() -> Result<serde_json::Value, String> + Send + 'static,
{
    let id = st.next_job.fetch_add(1, Ordering::Relaxed);
    let job = Arc::new(Job {
        started: std::time::Instant::now(),
        state: Mutex::new(JobState::Running),
    });
    st.jobs.lock().unwrap().insert(id, job.clone());
    tokio::task::spawn_blocking(move || {
        let result = work();
        *job.state.lock().unwrap() = match result {
            Ok(v) => JobState::Done(v),
            Err(e) => JobState::Failed(e),
        };
    });
    id
}

async fn api_job(State(st): State<Arc<AppState>>, AxPath(id): AxPath<u64>) -> Response {
    let Some(job) = st.jobs.lock().unwrap().get(&id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let elapsed_ms = job.started.elapsed().as_millis() as u64;
    let body = match &*job.state.lock().unwrap() {
        JobState::Running => serde_json::json!({ "state": "running", "elapsedMs": elapsed_ms }),
        JobState::Done(v) => {
            serde_json::json!({ "state": "done", "elapsedMs": elapsed_ms, "result": v })
        }
        JobState::Failed(e) => {
            serde_json::json!({ "state": "failed", "elapsedMs": elapsed_ms, "error": e })
        }
    };
    axum::Json(body).into_response()
}

#[derive(serde::Deserialize)]
struct MergeRequest {
    a: String,
    b: String,
    out: String,
    #[serde(default)]
    apply: bool,
    #[serde(default)]
    prefer: Option<String>,
    #[serde(default, rename = "autoAlias")]
    auto_alias: bool,
    #[serde(default)]
    aliases: Vec<(String, String)>,
}

async fn api_tools_merge(
    State(st): State<Arc<AppState>>,
    axum::Json(req): axum::Json<MergeRequest>,
) -> Response {
    if !st.tools_enabled {
        return (
            StatusCode::FORBIDDEN,
            "merge tools are local-only (disabled under --lan)",
        )
            .into_response();
    }
    if req.a.trim().is_empty() || req.b.trim().is_empty() || req.out.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "a, b and out are required").into_response();
    }
    let opts = xaero_merge::MergeOptions {
        apply: req.apply,
        prefer: match req.prefer.as_deref() {
            Some("a") => xaero_merge::Prefer::A,
            Some("b") => xaero_merge::Prefer::B,
            _ => xaero_merge::Prefer::Mtime,
        },
        // Resuming is a terminal recovery step for an interrupted run; the
        // Tools tab still requires a fresh, empty output directory.
        resume: false,
        servers: Vec::new(),
        aliases: req.aliases,
        auto_alias: req.auto_alias,
    };
    let (a, b, out) = (
        PathBuf::from(req.a),
        PathBuf::from(req.b),
        PathBuf::from(req.out),
    );
    let id = spawn_job(&st, move || {
        let report = xaero_merge::merge_to_output(&a, &b, &out, &opts)?;
        serde_json::to_value(&report).map_err(|e| e.to_string())
    });
    axum::Json(serde_json::json!({ "job": id })).into_response()
}

#[derive(serde::Deserialize)]
struct DbMergeRequest {
    base: String,
    sources: Vec<String>,
    out: String,
    #[serde(default)]
    apply: bool,
}

async fn api_tools_dbmerge(
    State(st): State<Arc<AppState>>,
    axum::Json(req): axum::Json<DbMergeRequest>,
) -> Response {
    if !st.tools_enabled {
        return (
            StatusCode::FORBIDDEN,
            "merge tools are local-only (disabled under --lan)",
        )
            .into_response();
    }
    if req.base.trim().is_empty() || req.sources.is_empty() || req.out.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "base, sources and out are required",
        )
            .into_response();
    }
    let base = PathBuf::from(req.base);
    let out = PathBuf::from(req.out);
    let sources: Vec<PathBuf> = req.sources.iter().map(PathBuf::from).collect();
    let apply = req.apply;
    let id = spawn_job(&st, move || {
        // Mirrors the CLI: sources and base are never modified; apply merges
        // into a fresh copy of base at `out`.
        let dest = if apply {
            if out.exists() {
                return Err(format!("output {} already exists", out.display()));
            }
            std::fs::copy(&base, &out).map_err(|e| format!("copy base: {e}"))?;
            out.clone()
        } else {
            base.clone()
        };
        let source_refs: Vec<&Path> = sources.iter().map(|p| p.as_path()).collect();
        // A Drawing DB has its own schema and merger; the highlight merger's
        // v2 normalization fails on it and leaves `out` a plain copy of base.
        let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let value = if xaero_db::drawing::is_drawing_db(name) {
            serde_json::to_value(xaero_db::drawing::merge_into(&dest, &source_refs, apply)?)
        } else {
            serde_json::to_value(xaero_db::merge::merge_into(&dest, &source_refs, apply)?)
        };
        value.map_err(|e| e.to_string())
    });
    axum::Json(serde_json::json!({ "job": id })).into_response()
}

/// One mirrored Atlas tile pyramid, described by its meta.json.
#[derive(Serialize, serde::Deserialize, Clone)]
struct AtlasSet {
    /// Which dimension it depicts: "overworld" | "the_nether" | "the_end".
    dim: String,
    /// URL path of the dataset below /atlas/, e.g. "Overworld/256k/day".
    #[serde(default)]
    url: String,
    /// World coordinate of the pyramid's top-left corner (blocks).
    #[serde(rename = "originX")]
    origin_x: i64,
    #[serde(rename = "originZ")]
    origin_z: i64,
    /// Blocks covered by one 256px tile at zMax.
    #[serde(rename = "bptMax")]
    bpt_max: i64,
    #[serde(rename = "zMin")]
    z_min: i32,
    #[serde(rename = "zMax")]
    z_max: i32,
}

/// Finds mirrored Atlas datasets: any directory (depth <= 4) with a meta.json.
fn scan_atlas_sets(root: &Path, rel: &str, depth: u32, out: &mut Vec<AtlasSet>) {
    let dir = root.join(rel);
    let meta = dir.join("meta.json");
    if let Ok(text) = std::fs::read_to_string(&meta) {
        match serde_json::from_str::<AtlasSet>(&text) {
            Ok(mut set) => {
                set.url = rel.replace('\\', "/");
                out.push(set);
                return;
            }
            Err(e) => eprintln!("atlas: bad {}: {e}", meta.display()),
        }
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for e in entries.filter_map(|e| e.ok()) {
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let name = e.file_name().to_string_lossy().to_string();
            let sub = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            scan_atlas_sets(root, &sub, depth - 1, out);
        }
    }
}

/// Parses every live waypoint file across the scanned worlds into vault
/// batches. Shared by the server startup sync, the sync endpoint and the CLI.
pub fn collect_vault_batches(worlds: &[WorldEntry]) -> Vec<xaero_db::vault::VaultBatch> {
    let mut batches = Vec::new();
    for we in worlds {
        for (dim_folder, path) in &we.world.waypoint_files {
            let Some(dim) = Dimension::from_minimap_folder(dim_folder) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let parsed = parse_waypoints_file(&text);
            batches.push(xaero_db::vault::VaultBatch {
                world: we.world.id.clone(),
                dim_key: dim.resource_key(),
                mw_file: path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default(),
                source: we.root.display().to_string(),
                waypoints: parsed.waypoints,
            });
        }
    }
    batches
}

pub fn vault_sync_now(
    vault: &Mutex<xaero_db::vault::Vault>,
    worlds: &[WorldEntry],
) -> Result<xaero_db::vault::VaultSyncReport, String> {
    let batches = collect_vault_batches(worlds);
    vault.lock().unwrap().sync(&batches, now_ms() as i64)
}

pub fn discover_worlds(roots: &[PathBuf]) -> Vec<WorldEntry> {
    let mut out = Vec::new();
    for root in roots {
        for world in xaero_scan::discover_root(root) {
            out.push(WorldEntry {
                root: root.clone(),
                world,
            });
        }
    }
    out
}

/// Best-effort canonicalization (falls back to the path as given) so watcher
/// event paths, config roots and CLI roots all compare equal.
pub(crate) fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

pub async fn run(config: ServerConfig) -> Result<(), String> {
    let config_path = canon(
        &config
            .config_path
            .clone()
            .unwrap_or_else(config::default_config_path),
    );
    let file_config = config::load(&config_path)?;
    let cli_roots: Vec<PathBuf> = config.roots.iter().map(|r| canon(r)).collect();
    let mut roots = cli_roots.clone();
    for r in &file_config.roots {
        let c = canon(r);
        if !roots.contains(&c) {
            roots.push(c);
        }
    }
    // The config is the one file the server writes; it must never live
    // inside a scanned root (roots are observed, never written).
    for root in &roots {
        if config_path.starts_with(root) {
            return Err(format!(
                "config {} is inside root {} — pass --config with a path outside every root",
                config_path.display(),
                root.display()
            ));
        }
    }
    // Region uploads are the one place the server writes region data; that
    // dir must stay disjoint from the user's own (read-only) roots.
    let ingest_dir = canon(&config.ingest_dir.clone().unwrap_or_else(|| {
        config_path
            .parent()
            .map(|d| d.join("ingest"))
            .unwrap_or_else(|| PathBuf::from("ingest"))
    }));
    if config_path.starts_with(&ingest_dir) {
        return Err(format!(
            "config {} is inside the ingest dir {} — pass --ingest-dir a path of its own",
            config_path.display(),
            ingest_dir.display()
        ));
    }
    for root in &roots {
        if ingest_dir.starts_with(root) || root.starts_with(&ingest_dir) {
            return Err(format!(
                "ingest dir {} overlaps root {} — pass --ingest-dir a path outside every root",
                ingest_dir.display(),
                root.display()
            ));
        }
    }
    let ingest_root_list = ingest::ingest_roots(&ingest_dir);
    if !ingest_root_list.is_empty() {
        eprintln!(
            "region ingest ({}): {} uploaded root(s)",
            ingest_dir.display(),
            ingest_root_list.len()
        );
    }
    for r in ingest_root_list {
        let c = canon(&r);
        if !roots.contains(&c) {
            roots.push(c);
        }
    }
    let worlds = discover_worlds(&roots);
    let ct = ColorTable::parse(COLORTABLE).map_err(|e| format!("color table: {e}"))?;
    let vault_path = config
        .vault_path
        .clone()
        .unwrap_or_else(xaero_db::vault::default_vault_path);
    let vault = match xaero_db::vault::Vault::open(&vault_path) {
        Ok(v) => Some(Arc::new(Mutex::new(v))),
        Err(e) => {
            eprintln!("waypoint vault unavailable ({e}) — archived waypoints disabled");
            None
        }
    };
    // Atlas mirror: explicit dir, or the platform default next to the vault.
    let atlas_dir = config
        .atlas_dir
        .clone()
        .or_else(|| {
            Some(
                xaero_db::vault::default_vault_path()
                    .parent()?
                    .join("atlas"),
            )
        })
        .filter(|d| d.is_dir());
    let mut atlas_sets = Vec::new();
    if let Some(dir) = &atlas_dir {
        scan_atlas_sets(dir, "", 4, &mut atlas_sets);
        eprintln!(
            "atlas mirror ({}): {} dataset(s)",
            dir.display(),
            atlas_sets.len()
        );
    }
    // Persistent thumbnail store next to the config (never inside a root).
    let pyramid_path = config_path
        .parent()
        .map(|d| d.join("pyramid.db"))
        .unwrap_or_else(|| PathBuf::from("pyramid.db"));
    let pyramid = match pyramid::PyramidStore::open(&pyramid_path) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("pyramid store unavailable ({e}) — zoomed-out thumbnails will not persist");
            None
        }
    };
    let (fs_tx, fs_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = Arc::new(AppState {
        worlds: RwLock::new(Arc::new(worlds)),
        ct,
        indexes: RwLock::new(HashMap::new()),
        tiles: Mutex::new(ByteLru::new(
            config.tile_cache_entries.max(64),
            config.tile_cache_bytes,
        )),
        thumbs: Mutex::new(ByteLru::new(
            config.thumb_cache_entries.max(1024),
            config.thumb_cache_bytes,
        )),
        mips: Mutex::new(ByteLru::new(2_000_000, MIP_CACHE_BYTES)),
        pyramid,
        stale_stamps: Mutex::new(HashMap::new()),
        unreadable: Mutex::new(VecDeque::new()),
        inflight: Mutex::new(HashMap::new()),
        index_inflight: Mutex::new(HashMap::new()),
        prefetched: Mutex::new(HashMap::new()),
        warm: WarmQueue {
            jobs: Mutex::new(VecDeque::new()),
            running: AtomicBool::new(false),
        },
        dbs: Mutex::new(HashMap::new()),
        vault,
        // Seeded per run: browsers keep tiles across server restarts and
        // revalidate them by ETag, which carries this stamp. A counter that
        // restarted at 1 would hand a stale tile from the last session a 304.
        generation: AtomicU64::new(now_ms() << 16),
        epoch: AtomicU64::new(1),
        atlas_dir,
        atlas_sets,
        tools_enabled: config.password.is_none(),
        jobs: Mutex::new(HashMap::new()),
        next_job: AtomicU64::new(1),
        live: live::LiveState::new(fs_tx),
        ingest: ingest::IngestState::new(),
        preview: preview::PreviewState::new(),
        ingest_dir,
        ingest_no_caves: config.ingest_no_caves,
        loopback_exempt: config.bind.ip().is_loopback()
            && config.password.is_none()
            && !config.ingest_require_token,
        cli_roots,
        config: Mutex::new(ConfigCache {
            file: file_config,
            stamp: config::file_stamp(&config_path),
            last_stat: std::time::Instant::now(),
        }),
        config_path,
        watcher: Mutex::new(None),
        recent: Mutex::new(VecDeque::new()),
    });

    // Waypoints get backed up into the vault on every start, automatically.
    if let Some(vault) = &state.vault {
        let worlds = state.worlds.read().unwrap().clone();
        match vault_sync_now(vault, &worlds) {
            Ok(r) => eprintln!(
                "waypoint vault ({}): {} live waypoints synced, {} new, {} archived total",
                vault_path.display(),
                r.seen,
                r.added,
                r.archived_total
            ),
            Err(e) => eprintln!("waypoint vault sync failed: {e}"),
        }
    }

    tokio::spawn(live::debounce_loop(state.clone(), fs_rx));
    start_watching(&state, config.live_poll);

    let mut app = Router::new()
        .route("/", get(ui_index))
        .route("/{*path}", get(ui_asset))
        .route("/api/state", get(api_state))
        .route("/api/waypoints/{w}", get(api_waypoints))
        .route("/api/vault/sync", post(api_vault_sync))
        .route("/api/refresh", post(api_refresh))
        .route("/api/diagnostics", get(api_diagnostics))
        .route(
            "/api/atlas/locations",
            get(api_atlas_locations_get).put(api_atlas_locations_put),
        )
        .route("/api/tools/merge", post(api_tools_merge))
        .route("/api/tools/dbmerge", post(api_tools_dbmerge))
        .route("/api/jobs/{id}", get(api_job))
        .route(
            "/api/roots",
            get(api_roots_get)
                .post(api_roots_add)
                .delete(api_roots_remove),
        )
        .route(
            "/api/tokens",
            get(api_tokens_get)
                .post(api_tokens_generate)
                .delete(api_tokens_revoke),
        )
        .route("/api/fs/list", get(api_fs_list))
        .route("/api/players", delete(api_players_remove))
        .route("/ws/live", get(live::ws_live))
        .route("/ingest/v1/position", post(live::ingest_position))
        .route(
            "/ingest/v1/region",
            post(ingest::ingest_region).layer(axum::extract::DefaultBodyLimit::max(
                ingest::REGION_BODY_MAX,
            )),
        )
        .route(
            "/ingest/v1/highlights",
            post(highlights::ingest_highlights).layer(axum::extract::DefaultBodyLimit::max(
                highlights::HIGHLIGHT_BODY_MAX,
            )),
        )
        .route(
            "/ingest/v1/preview",
            post(preview::ingest_preview).layer(axum::extract::DefaultBodyLimit::max(
                preview::PREVIEW_BODY_MAX,
            )),
        )
        .route("/preview/{dim}/{z}/{x}/{y}", get(preview::preview_tile))
        .route("/tiles/{w}/{d}/{m}/{layer}/{z}/{x}/{y}", get(tile))
        .route("/hl/{w}/{db}/{d}/{z}/{x}/{y}", get(highlight_tile))
        .route("/atlas/{*path}", get(atlas_asset))
        .with_state(state);

    if config.password.is_none() {
        // Local mode has no auth at all, so pin the Host header to loopback:
        // this kills DNS-rebinding pages that resolve to 127.0.0.1 and would
        // otherwise become same-origin for the whole API.
        app = app.layer(axum::middleware::from_fn(require_local_host));
    }

    if let Some(password) = &config.password {
        if password.len() < 4 {
            return Err("password must be at least 4 characters".into());
        }
        let auth = Arc::new(auth::Auth::new(password.clone()));
        app = Router::new()
            .route("/login", get(auth::login_page).post(auth::login_submit))
            .fallback_service(app.layer(axum::middleware::from_fn_with_state(
                auth.clone(),
                auth::require_session,
            )))
            .with_state(auth);
    }

    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|e| format!("bind {}: {e}", config.bind))?;
    // ConnectInfo carries the peer address into the ingest handlers, where
    // loopback connections are exempt from the bearer-token requirement.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .map_err(|e| e.to_string())
}

/// Arms inotify watches over the known map directories with the poll loop as
/// fallback; a watchdog demotes a watcher that registers fine but never fires
/// (the ntfs3 worry). Detached: registration (a syscall per known dir, no
/// tree walk) must never delay startup or a rescan response.
fn start_watching(st: &Arc<AppState>, force_poll: bool) {
    if force_poll {
        eprintln!("live: poll mode (--live-poll)");
        tokio::spawn(live::poll_loop(st.clone()));
        return;
    }
    let st2 = st.clone();
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let worlds = st2.worlds.read().unwrap().clone();
        match live::arm_watcher(&st2, &worlds) {
            Ok(n) => {
                eprintln!("live: watching {n} map folder(s) for changes");
                handle.spawn(live::watchdog_loop(st2.clone()));
            }
            Err(e) => {
                eprintln!("live: watch setup failed ({e}) — falling back to poll mode");
                handle.spawn(live::poll_loop(st2.clone()));
            }
        }
    });
}

async fn require_local_host(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().authority().map(|a| a.to_string()));
    if host.as_deref().map(host_is_local).unwrap_or(false) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "host not allowed").into_response()
    }
}

fn host_is_local(host: &str) -> bool {
    let bare = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    matches!(bare, "127.0.0.1" | "localhost" | "::1")
}

// ------------------------------------------------------------- root config --

/// Rebuilds the world list from CLI + persisted roots, drops every cache and
/// tells the watcher and clients to resync. Epoch first: in-flight watcher
/// batches must see the change and drop themselves.
pub(crate) async fn rescan_roots(st: &Arc<AppState>) {
    st.epoch.fetch_add(1, Ordering::SeqCst);
    let mut roots = st.cli_roots.clone();
    {
        let cache = st.config.lock().unwrap();
        for r in &cache.file.roots {
            let c = canon(r);
            if !roots.contains(&c) {
                roots.push(c);
            }
        }
    }
    for r in ingest::ingest_roots(&st.ingest_dir) {
        let c = canon(&r);
        if !roots.contains(&c) {
            roots.push(c);
        }
    }
    let worlds = tokio::task::spawn_blocking(move || discover_worlds(&roots))
        .await
        .unwrap_or_default();
    *st.worlds.write().unwrap() = Arc::new(worlds);
    st.indexes.write().unwrap().clear();
    st.dbs.lock().unwrap().clear();
    st.tiles.lock().unwrap().clear();
    // MapId is positional: stale-imagery stamps from the old world list must
    // not resolve against the new one. The thumbnail tiers are positional too
    // — an mtime collision across reordered worlds (copied archives preserve
    // mtimes) would paint the wrong world's imagery — and re-warming from the
    // pyramid store is cheap, so they go as well.
    st.stale_stamps.lock().unwrap().clear();
    st.thumbs.lock().unwrap().clear();
    st.mips.lock().unwrap().clear();
    st.prefetched.lock().unwrap().clear();
    st.generation.fetch_add(1, Ordering::Relaxed);
    if st.watcher.lock().unwrap().is_some() {
        start_watching(st, false);
    }
    live::emit_state_changed(st);
}

#[derive(Serialize)]
struct RootJson {
    path: String,
    /// "cli" roots come from --root flags and survive only via those flags.
    origin: &'static str,
    worlds: usize,
}

fn roots_json(st: &AppState) -> Vec<RootJson> {
    let worlds = st.worlds.read().unwrap().clone();
    let persisted = st.config.lock().unwrap().file.roots.clone();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let ingest_roots = ingest::ingest_roots(&st.ingest_dir);
    for (root, origin) in st
        .cli_roots
        .iter()
        .map(|r| (r.clone(), "cli"))
        .chain(persisted.iter().map(|r| (canon(r), "config")))
        .chain(ingest_roots.iter().map(|r| (canon(r), "ingest")))
    {
        if !seen.insert(root.clone()) {
            continue;
        }
        out.push(RootJson {
            worlds: worlds.iter().filter(|w| w.root == root).count(),
            path: root.display().to_string(),
            origin,
        });
    }
    out
}

async fn api_roots_get(State(st): State<Arc<AppState>>) -> Response {
    axum::Json(roots_json(&st)).into_response()
}

#[derive(serde::Deserialize)]
struct RootRequest {
    path: String,
}

/// Adds a root and persists it. Mutations re-read the config from disk first
/// (read-merge-write) so a `tokens generate` between our load and this save
/// is never clobbered.
async fn api_roots_add(
    State(st): State<Arc<AppState>>,
    axum::Json(req): axum::Json<RootRequest>,
) -> Response {
    if !st.tools_enabled {
        return (
            StatusCode::FORBIDDEN,
            "root management is local-only (disabled under --lan)",
        )
            .into_response();
    }
    let p = PathBuf::from(req.path.trim());
    if !p.is_absolute() {
        return (StatusCode::BAD_REQUEST, "path must be absolute").into_response();
    }
    let p = canon(&p);
    if !p.is_dir() {
        return (StatusCode::BAD_REQUEST, "not a directory").into_response();
    }
    if st.config_path.starts_with(&p) {
        return (
            StatusCode::BAD_REQUEST,
            "refusing a root that contains the server config",
        )
            .into_response();
    }
    // The same rule startup enforces: persisting an overlapping root here
    // would make the next `serve` refuse to start.
    if st.ingest_dir.starts_with(&p) || p.starts_with(&st.ingest_dir) {
        return (
            StatusCode::BAD_REQUEST,
            "refusing a root that overlaps the ingest dir (the server would not start with it)",
        )
            .into_response();
    }
    {
        let mut cache = st.config.lock().unwrap();
        // File lock: a concurrent `tokens generate` must not be clobbered.
        let fallback = cache.file.clone();
        let saved = config::with_file_lock(&st.config_path, || {
            let mut fresh = config::load(&st.config_path).unwrap_or(fallback);
            if !fresh.roots.iter().any(|r| canon(r) == p) && !st.cli_roots.contains(&p) {
                fresh.roots.push(p.clone());
                config::save(&st.config_path, &fresh)?;
            }
            Ok::<_, String>(fresh)
        });
        let fresh = match saved {
            Ok(f) => f,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
        cache.stamp = config::file_stamp(&st.config_path);
        cache.file = fresh;
    }
    rescan_roots(&st).await;
    axum::Json(roots_json(&st)).into_response()
}

async fn api_roots_remove(
    State(st): State<Arc<AppState>>,
    axum::Json(req): axum::Json<RootRequest>,
) -> Response {
    if !st.tools_enabled {
        return (
            StatusCode::FORBIDDEN,
            "root management is local-only (disabled under --lan)",
        )
            .into_response();
    }
    let p = canon(&PathBuf::from(req.path.trim()));
    if st.cli_roots.contains(&p) {
        return (
            StatusCode::BAD_REQUEST,
            "this root comes from a --root flag; restart without it to remove it",
        )
            .into_response();
    }
    if p.starts_with(&st.ingest_dir) {
        return (
            StatusCode::BAD_REQUEST,
            "this root is managed by region ingest; delete its folder on disk to remove it",
        )
            .into_response();
    }
    let removed;
    {
        let mut cache = st.config.lock().unwrap();
        let fallback = cache.file.clone();
        let saved = config::with_file_lock(&st.config_path, || {
            let mut fresh = config::load(&st.config_path).unwrap_or(fallback);
            let before = fresh.roots.len();
            fresh.roots.retain(|r| canon(r) != p);
            let removed = fresh.roots.len() != before;
            if removed {
                config::save(&st.config_path, &fresh)?;
            }
            Ok::<_, String>((fresh, removed))
        });
        let fresh = match saved {
            Ok((f, r)) => {
                removed = r;
                f
            }
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
        cache.stamp = config::file_stamp(&st.config_path);
        cache.file = fresh;
    }
    if !removed {
        return (StatusCode::NOT_FOUND, "no such root").into_response();
    }
    rescan_roots(&st).await;
    axum::Json(roots_json(&st)).into_response()
}

// ---------------------------------------------------------- ingest tokens --

/// What the UI may know about a token: never the token itself, only the same
/// player/prefix/age triple `xaerotools tokens list` prints.
#[derive(Serialize)]
struct TokenJson {
    player: String,
    prefix: String,
    #[serde(rename = "createdMs")]
    created_ms: u64,
}

fn tokens_json(cfg: &config::FileConfig) -> Vec<TokenJson> {
    let mut out: Vec<TokenJson> = cfg
        .tokens
        .iter()
        .map(|t| TokenJson {
            player: t.player.clone(),
            prefix: t.token.chars().take(8).collect(),
            created_ms: t.created_ms,
        })
        .collect();
    out.sort_by(|a, b| a.player.cmp(&b.player));
    out
}

/// Token management is minting credentials, so it follows the roots rule:
/// local-only, disabled under --lan (use the `tokens` CLI on the server box).
fn tokens_gate(st: &AppState) -> Option<Response> {
    if st.tools_enabled {
        None
    } else {
        Some(
            (
                StatusCode::FORBIDDEN,
                "token management is local-only (disabled under --lan)",
            )
                .into_response(),
        )
    }
}

async fn api_tokens_get(State(st): State<Arc<AppState>>) -> Response {
    if let Some(resp) = tokens_gate(&st) {
        return resp;
    }
    // Pick up tokens the CLI generated since the last request.
    live::maybe_reload_config(&st);
    let cache = st.config.lock().unwrap();
    axum::Json(tokens_json(&cache.file)).into_response()
}

#[derive(serde::Deserialize)]
struct TokenRequest {
    player: String,
}

/// Generates (or replaces — one token per player) and returns the token once,
/// exactly like `xaerotools tokens generate`. Same lock + read-merge-write as
/// the roots mutations so a concurrent CLI call is never clobbered.
async fn api_tokens_generate(
    State(st): State<Arc<AppState>>,
    axum::Json(req): axum::Json<TokenRequest>,
) -> Response {
    if let Some(resp) = tokens_gate(&st) {
        return resp;
    }
    let player = req.player.trim().to_string();
    // The same segment rule region ingest enforces, checked at mint time so a
    // token generated in the UI always works for map upload too.
    if !ingest::safe_segment(&player) {
        return (
            StatusCode::BAD_REQUEST,
            "player name must be a plain name (letters, digits, space, _-. and similar; max 128)",
        )
            .into_response();
    }
    let token;
    {
        let mut cache = st.config.lock().unwrap();
        let fallback = cache.file.clone();
        let saved = config::with_file_lock(&st.config_path, || {
            let mut fresh = config::load(&st.config_path).unwrap_or(fallback);
            let token = fresh.set_token(&player, now_ms());
            config::save(&st.config_path, &fresh)?;
            Ok::<_, String>((fresh, token))
        });
        let fresh = match saved {
            Ok((f, t)) => {
                token = t;
                f
            }
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
        cache.stamp = config::file_stamp(&st.config_path);
        cache.file = fresh;
    }
    let tokens = tokens_json(&st.config.lock().unwrap().file);
    axum::Json(serde_json::json!({
        "player": player,
        "token": token,
        "tokens": tokens,
    }))
    .into_response()
}

async fn api_tokens_revoke(
    State(st): State<Arc<AppState>>,
    axum::Json(req): axum::Json<TokenRequest>,
) -> Response {
    if let Some(resp) = tokens_gate(&st) {
        return resp;
    }
    let player = req.player.trim().to_string();
    let removed;
    {
        let mut cache = st.config.lock().unwrap();
        let fallback = cache.file.clone();
        let saved = config::with_file_lock(&st.config_path, || {
            let mut fresh = config::load(&st.config_path).unwrap_or(fallback);
            let removed = fresh.revoke_token(&player);
            if removed {
                config::save(&st.config_path, &fresh)?;
            }
            Ok::<_, String>((fresh, removed))
        });
        let fresh = match saved {
            Ok((f, r)) => {
                removed = r;
                f
            }
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
        cache.stamp = config::file_stamp(&st.config_path);
        cache.file = fresh;
    }
    if !removed {
        return (StatusCode::NOT_FOUND, "no token for that player").into_response();
    }
    axum::Json(tokens_json(&st.config.lock().unwrap().file)).into_response()
}

// ------------------------------------------------------------ live players --

#[derive(serde::Deserialize)]
struct PlayerRequest {
    player: String,
}

/// Removes a live player marker for every viewer — roster cleanup for test or
/// duplicated accounts. Purely in-memory: tokens, backups and uploads are
/// untouched, and the marker returns the moment that account reports again.
async fn api_players_remove(
    State(st): State<Arc<AppState>>,
    axum::Json(req): axum::Json<PlayerRequest>,
) -> Response {
    if st.live.remove_player(req.player.trim()) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "no live marker for that player").into_response()
    }
}

#[derive(serde::Deserialize)]
struct FsListQuery {
    path: Option<String>,
}

/// Lists subdirectories for the root picker. Local-only: server-side folder
/// browsing stays disabled under --lan.
async fn api_fs_list(State(st): State<Arc<AppState>>, Query(q): Query<FsListQuery>) -> Response {
    if !st.tools_enabled {
        return (
            StatusCode::FORBIDDEN,
            "folder browsing is local-only (disabled under --lan)",
        )
            .into_response();
    }
    let path = q
        .path
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));
    let path = canon(&path);
    let Ok(entries) = std::fs::read_dir(&path) else {
        return (StatusCode::BAD_REQUEST, "cannot read directory").into_response();
    };
    let mut dirs: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    // Dot-directories must be listed: the folder this browser exists to find
    // is `.minecraft` on every platform. They sort last so the noise of a
    // home directory's dotfiles stays out of the way.
    dirs.sort_by_key(|a| (a.starts_with('.'), a.to_lowercase()));
    axum::Json(serde_json::json!({
        "path": path.display().to_string(),
        "parent": path.parent().map(|p| p.display().to_string()),
        "dirs": dirs,
    }))
    .into_response()
}

mod auth {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use axum::extract::{Request, State};
    use axum::http::{header, StatusCode};
    use axum::middleware::Next;
    use axum::response::{Html, IntoResponse, Redirect, Response};
    use axum::Form;

    pub struct Auth {
        password: String,
        token: String,
        failures: AtomicU32,
        /// One password check at a time: the backoff sleep only limits guessing
        /// throughput if attempts cannot simply run in parallel.
        gate: tokio::sync::Semaphore,
    }

    /// Equality that takes the same time for any two inputs up to 256 bytes,
    /// so neither a password nor a session token leaks its length or a
    /// matching prefix through timing.
    fn fixed_eq(a: &[u8], b: &[u8]) -> bool {
        let mut diff = a.len() ^ b.len();
        for i in 0..a.len().max(b.len()).max(256) {
            let x = a.get(i).copied().unwrap_or(0);
            let y = b.get(i).copied().unwrap_or(0);
            diff |= (x ^ y) as usize;
        }
        diff == 0
    }

    impl Auth {
        pub fn new(password: String) -> Auth {
            let mut raw = [0u8; 32];
            getrandom::fill(&mut raw).expect("os rng");
            let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
            Auth {
                password,
                token,
                failures: AtomicU32::new(0),
                gate: tokio::sync::Semaphore::new(1),
            }
        }

        fn check_password(&self, attempt: &str) -> bool {
            fixed_eq(attempt.as_bytes(), self.password.as_bytes())
        }
    }

    fn has_session(auth: &Auth, req: &Request) -> bool {
        req.headers()
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(|cookies| {
                cookies.split(';').any(|c| {
                    c.trim()
                        .strip_prefix("xt_session=")
                        .is_some_and(|v| fixed_eq(v.as_bytes(), auth.token.as_bytes()))
                })
            })
            .unwrap_or(false)
    }

    pub async fn require_session(
        State(auth): State<Arc<Auth>>,
        req: Request,
        next: Next,
    ) -> Response {
        // Ingest authenticates with its own bearer tokens (ADR 007), never
        // the viewer session: exact routes only, and every other /ingest*
        // path 404s rather than falling through to the UI.
        if req.uri().path().starts_with("/ingest") {
            if req.method() == axum::http::Method::POST
                && matches!(
                    req.uri().path(),
                    "/ingest/v1/position"
                        | "/ingest/v1/region"
                        | "/ingest/v1/preview"
                        | "/ingest/v1/highlights"
                )
            {
                return next.run(req).await;
            }
            return StatusCode::NOT_FOUND.into_response();
        }
        if has_session(&auth, &req) {
            return next.run(req).await;
        }
        if req.uri().path().starts_with("/api")
            || req.uri().path().starts_with("/tiles")
            || req.uri().path().starts_with("/hl")
            || req.uri().path().starts_with("/atlas")
            || req.uri().path().starts_with("/preview")
            || req.uri().path().starts_with("/ws")
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Redirect::to("/login").into_response()
    }

    pub async fn login_page() -> Html<&'static str> {
        Html(
            r#"<!doctype html><meta charset="utf-8"><title>XaeroTools login</title>
<style>body{background:#14161a;color:#d7dae0;font-family:system-ui;display:grid;place-items:center;height:100vh;margin:0}
form{background:#242833;padding:2em;border-radius:10px;display:flex;gap:10px;flex-direction:column;min-width:260px}
input,button{padding:9px;border-radius:6px;border:1px solid #2c313d;background:#14161a;color:#d7dae0;font:inherit}
button{background:#4f8ef7;color:#fff;border:none;cursor:pointer}</style>
<form method="post" action="/login"><b>XaeroTools</b>
<input type="password" name="password" placeholder="password" autofocus>
<button>Open map</button></form>"#,
        )
    }

    #[derive(serde::Deserialize)]
    pub struct LoginForm {
        password: String,
    }

    pub async fn login_submit(
        State(auth): State<Arc<Auth>>,
        Form(form): Form<LoginForm>,
    ) -> Response {
        // Serialized: a thousand parallel guesses would otherwise each be
        // answered after one backoff, not one after another.
        let _permit = auth.gate.acquire().await;
        if !auth.check_password(&form.password) {
            // Linear backoff against guessing.
            let n = auth.failures.fetch_add(1, Ordering::Relaxed).min(20);
            tokio::time::sleep(std::time::Duration::from_millis(300 * (n as u64 + 1))).await;
            return (
                StatusCode::UNAUTHORIZED,
                Html("wrong password — <a href='/login' style='color:#4f8ef7'>retry</a>"),
            )
                .into_response();
        }
        auth.failures.store(0, Ordering::Relaxed);
        (
            [(
                header::SET_COOKIE,
                format!(
                    "xt_session={}; HttpOnly; SameSite=Lax; Path=/; Max-Age=604800",
                    auth.token
                ),
            )],
            Redirect::to("/"),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------- UI --

async fn ui_index() -> Response {
    serve_ui("index.html")
}

async fn ui_asset(AxPath(path): AxPath<String>) -> Response {
    if path.starts_with("api/")
        || path.starts_with("tiles/")
        || path.starts_with("ingest/")
        || path.starts_with("ws/")
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    serve_ui(&path)
}

fn serve_ui(path: &str) -> Response {
    // Live-dev override: serve from a directory instead of the embedded
    // build. Reads from disk, so it gets the same segment filter as
    // atlas_asset — no traversal, no absolute components.
    if let Some(dir) = std::env::var_os("XT_WEBUI_DIR") {
        if path.split('/').any(|seg| {
            seg.is_empty() || seg == "." || seg == ".." || seg.contains('\\') || seg.contains(':')
        }) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let full = PathBuf::from(dir).join(path);
        if let Ok(bytes) = std::fs::read(&full) {
            return ([(header::CONTENT_TYPE, guess_mime(path))], bytes).into_response();
        }
    }
    match WEBUI.get_file(path) {
        Some(f) => ([(header::CONTENT_TYPE, guess_mime(path))], f.contents()).into_response(),
        None if path != "index.html" => serve_ui("index.html"), // SPA fallback
        None => (StatusCode::NOT_FOUND, "webui not built").into_response(),
    }
}

fn guess_mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript",
        "css" => "text/css",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

// --------------------------------------------------------------------- API --

#[derive(Serialize)]
struct StateJson {
    worlds: Vec<WorldJson>,
    /// Locally mirrored Atlas tile pyramids (empty = mirror absent).
    atlas: Vec<AtlasSet>,
    /// Merge tools are local-only; false under --lan.
    #[serde(rename = "toolsEnabled")]
    tools_enabled: bool,
    /// Cache occupancy, so a slow map can be diagnosed without a profiler.
    caches: CacheStatsJson,
    /// Root of the local Atlas mirror behind `/atlas/`, if one is configured.
    #[serde(rename = "atlasDir")]
    atlas_dir: Option<String>,
    /// Where region uploads land (per-player backups + merged tree).
    #[serde(rename = "ingestDir")]
    ingest_dir: String,
    /// The overlay palette, in match order (first `pattern` a DB file name
    /// contains wins). The viewer needs it for each overlay's label, its
    /// description and its default colour; it used to keep its own copy,
    /// which drifted from what the server actually painted.
    #[serde(rename = "hlPalette")]
    hl_palette: Vec<HlPaletteJson>,
}

#[derive(Serialize)]
struct HlPaletteJson {
    /// Substring matched against the DB file name.
    pattern: &'static str,
    label: &'static str,
    detection: &'static str,
    /// `#rrggbb` — what the server paints when the tile URL carries no `c=`.
    color: String,
    /// False only for LavaColumns, whose value column is a column height: its
    /// rows fade by height instead of drawing a flat highlight.
    #[serde(rename = "isTimestamp")]
    is_timestamp: bool,
    /// True when a companion client can stream this module's finds live
    /// (`POST /ingest/v1/highlights`).
    syncable: bool,
}

fn hl_palette_json() -> Vec<HlPaletteJson> {
    xaero_db::HL_PALETTE
        .iter()
        .map(|i| HlPaletteJson {
            pattern: i.pattern,
            label: i.label,
            detection: i.detection,
            color: format!("#{:02x}{:02x}{:02x}", i.color[0], i.color[1], i.color[2]),
            is_timestamp: !i.semantics.prefers_max(),
            syncable: highlights::SYNCABLE
                .iter()
                .any(|db| xaero_db::highlight_db_info(db).map(|e| e.pattern) == Some(i.pattern)),
        })
        .collect()
}

#[derive(serde::Serialize)]
struct CacheStatsJson {
    #[serde(rename = "tileEntries")]
    tile_entries: usize,
    #[serde(rename = "tileBytes")]
    tile_bytes: usize,
    /// Region thumbnails held for the zoomed-out pyramid. A high count here is
    /// why a zoomed-out view that was slow once is fast afterwards.
    #[serde(rename = "thumbEntries")]
    thumb_entries: usize,
    #[serde(rename = "thumbBytes")]
    thumb_bytes: usize,
    #[serde(rename = "indexedMaps")]
    indexed_maps: usize,
}

/// One Atlas point of interest, exactly the fields the viewer draws.
///
/// Re-parsed server-side rather than stored as opaque JSON so a client cannot
/// turn this endpoint into arbitrary persisted content.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct AtlasLocation {
    name: String,
    description: String,
    tags: Option<String>,
    dimension: i32,
    x: f64,
    y: f64,
    z: f64,
    wiki: Option<String>,
    #[serde(rename = "videoUrl")]
    video_url: Option<String>,
    #[serde(rename = "dateAddedUtc")]
    date_added_utc: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct AtlasStore {
    #[serde(rename = "fetchedMs")]
    fetched_ms: u64,
    locations: Vec<AtlasLocation>,
}

/// Cap on an uploaded POI payload. The real list is ~311 KB.
const ATLAS_STORE_MAX: usize = 2 << 20;

fn atlas_store_path(st: &AppState) -> PathBuf {
    st.config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("atlas-locations.json")
}

/// Returns the stored Atlas POI list.
///
/// The server never contacts the Atlas API itself and never expires this:
/// the whole point is that the data is downloaded once, by the user, and then
/// served locally forever. Refreshing is an explicit action in the UI.
async fn api_atlas_locations_get(State(st): State<Arc<AppState>>) -> Response {
    let path = atlas_store_path(&st);
    let Ok(text) = tokio::fs::read_to_string(&path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match serde_json::from_str::<AtlasStore>(&text) {
        Ok(store) => axum::Json(serde_json::json!({
            "fetchedMs": store.fetched_ms,
            "count": store.locations.len(),
            "locations": store.locations,
        }))
        .into_response(),
        Err(e) => {
            eprintln!("atlas: bad {}: {e}", path.display());
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// Stores a POI list the browser downloaded, so no later session needs to.
async fn api_atlas_locations_put(
    State(st): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Response {
    if !st.tools_enabled {
        return (
            StatusCode::FORBIDDEN,
            "atlas storage is local-only (disabled under --lan)",
        )
            .into_response();
    }
    if body.len() > ATLAS_STORE_MAX {
        return (StatusCode::PAYLOAD_TOO_LARGE, "atlas payload too large").into_response();
    }
    let locations: Vec<AtlasLocation> = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("bad atlas payload: {e}")).into_response()
        }
    };
    let store = AtlasStore {
        fetched_ms: now_ms(),
        locations,
    };
    let path = atlas_store_path(&st);
    let text = match serde_json::to_string(&store) {
        Ok(t) => t,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    match config::save_sidecar(&path, &text) {
        Ok(()) => (
            StatusCode::NO_CONTENT,
            [(header::CACHE_CONTROL, "no-store")],
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// Serves a file from the local Atlas mirror. Only plain, relative path
/// segments are accepted — no parent traversal, no absolute components.
async fn atlas_asset(State(st): State<Arc<AppState>>, AxPath(path): AxPath<String>) -> Response {
    let Some(root) = &st.atlas_dir else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if path.split('/').any(|seg| {
        seg.is_empty() || seg == "." || seg == ".." || seg.contains('\\') || seg.contains(':')
    }) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let file = root.join(&path);
    let Ok(bytes) = tokio::fs::read(&file).await else {
        // Atlas pyramids are sparse, so misses are normal and endless. Let the
        // browser remember them briefly; without this the same absent tiles are
        // re-requested on every reload.
        return (
            StatusCode::NOT_FOUND,
            [(header::CACHE_CONTROL, "public, max-age=300")],
        )
            .into_response();
    };
    let mime = match file.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("json") => "application/json",
        Some("xml") => "text/xml",
        _ => "application/octet-stream",
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            // The mirror is immutable in practice; let the browser keep tiles.
            (header::CACHE_CONTROL, "public, max-age=604800"),
        ],
        bytes,
    )
        .into_response()
}

#[derive(Serialize)]
struct WorldJson {
    id: String,
    root: String,
    /// The world's world-map folder on disk (tools tab path pickers).
    #[serde(rename = "mapPath")]
    map_path: Option<String>,
    /// "user" root, or an ingest-managed tree: "ingestMerged" (the shared
    /// upload map) / "ingestPlayer" (one uploader's verbatim backup, `player`
    /// set). Lets the UI label duplicate world ids and find the merged tree
    /// to overlay on the world being viewed.
    origin: &'static str,
    player: Option<String>,
    dims: Vec<DimJson>,
    databases: Vec<String>,
    #[serde(rename = "hasWaypoints")]
    has_waypoints: bool,
}

#[derive(Serialize)]
struct DimJson {
    folder: String,
    /// "overworld" | "the_nether" | "the_end" | full custom id | null.
    /// A behaviour hint (Nether scaling, cave defaults), NOT an identity: five
    /// different custom dimensions all report "overworld" here.
    #[serde(rename = "dimType")]
    dim_type: Option<String>,
    /// Decoded resource key, e.g. "minecraft:worlds/2b2t/2b2t_1".
    #[serde(rename = "dimId")]
    dim_id: Option<String>,
    /// Human-readable name for the dimension picker. This is what keeps six
    /// custom dimensions from all rendering as "Overworld".
    label: String,
    mws: Vec<MwJson>,
}

#[derive(Serialize)]
struct MwJson {
    id: String,
    display: String,
    #[serde(rename = "caveLayers")]
    cave_layers: Vec<i32>,
    /// Display name per entry of `cave_layers`, same order. The sentinel layer
    /// is "Cave (full column)", not "Cave layer -2147483648".
    #[serde(rename = "caveLabels")]
    cave_labels: Vec<String>,
}

fn dim_type_string(d: &Dimension) -> String {
    match d {
        Dimension::Overworld => "overworld".into(),
        Dimension::Nether => "the_nether".into(),
        Dimension::End => "the_end".into(),
        Dimension::Custom(id) => id.clone(),
    }
}

async fn api_state(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let caches = {
        let (tile_entries, tile_bytes) = {
            let t = st.tiles.lock().unwrap();
            (t.len(), t.bytes())
        };
        let (thumb_entries, thumb_bytes) = {
            let t = st.thumbs.lock().unwrap();
            (t.len(), t.bytes())
        };
        CacheStatsJson {
            tile_entries,
            tile_bytes,
            thumb_entries,
            thumb_bytes,
            indexed_maps: st.indexes.read().unwrap().len(),
        }
    };
    let snapshot = st.worlds.read().unwrap().clone();
    let merged_root = canon(&st.ingest_dir.join("merged"));
    let players_root = canon(&st.ingest_dir.join("players"));
    let worlds = snapshot
        .iter()
        .map(|we| {
            let (origin, player) = if we.root == merged_root {
                ("ingestMerged", None)
            } else if we.root.parent() == Some(players_root.as_path()) {
                (
                    "ingestPlayer",
                    we.root.file_name().map(|s| s.to_string_lossy().to_string()),
                )
            } else {
                ("user", None)
            };
            WorldJson {
                id: we.world.id.clone(),
                root: we.root.display().to_string(),
                map_path: we
                    .world
                    .world_map_path
                    .as_ref()
                    .map(|p| p.display().to_string()),
                origin,
                player,
                dims: we
                    .world
                    .dims
                    .iter()
                    .map(|d| DimJson {
                        folder: d.folder.clone(),
                        dim_type: d.dimension_type().as_ref().map(dim_type_string),
                        dim_id: d.dimension_id(),
                        label: d.label(),
                        mws: d
                            .multiworlds
                            .iter()
                            .map(|m| MwJson {
                                id: m.id.clone(),
                                display: m.display.clone(),
                                cave_labels: m
                                    .cave_layers
                                    .iter()
                                    .map(|n| xaero_core::naming::cave_layer_label(*n))
                                    .collect(),
                                cave_layers: m.cave_layers.clone(),
                            })
                            .collect(),
                    })
                    .collect(),
                databases: we.world.databases.clone(),
                has_waypoints: !we.world.waypoint_files.is_empty(),
            }
        })
        .collect();
    axum::Json(StateJson {
        worlds,
        atlas: st.atlas_sets.clone(),
        tools_enabled: st.tools_enabled,
        caches,
        atlas_dir: st.atlas_dir.as_ref().map(|p| p.display().to_string()),
        ingest_dir: st.ingest_dir.display().to_string(),
        hl_palette: hl_palette_json(),
    })
}

#[derive(Serialize)]
struct WaypointFileJson {
    #[serde(rename = "dimFolder")]
    dim_folder: String,
    #[serde(rename = "dimKey")]
    dim_key: Option<String>,
    file: String,
    waypoints: Vec<WaypointJson>,
}

#[derive(Serialize)]
struct WaypointJson {
    name: String,
    initials: String,
    x: i32,
    y: Option<i32>,
    z: i32,
    color: u8,
    rgb: String,
    disabled: bool,
    purpose: i32,
    set: String,
    /// True when this waypoint no longer exists in any live game file and is
    /// preserved only by the vault.
    archived: bool,
}

async fn api_waypoints(State(st): State<Arc<AppState>>, AxPath(w): AxPath<usize>) -> Response {
    let worlds = st.worlds.read().unwrap().clone();
    let Some(we) = worlds.get(w) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut files = Vec::new();
    for (dim_folder, path) in &we.world.waypoint_files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let parsed = parse_waypoints_file(&text);
        let dim_key = Dimension::from_minimap_folder(dim_folder).map(|d| d.resource_key());
        files.push(WaypointFileJson {
            dim_folder: dim_folder.clone(),
            dim_key,
            file: path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            waypoints: parsed
                .waypoints
                .into_iter()
                .map(|wp| {
                    let [r, g, b] = waypoint_color_rgb(wp.color);
                    WaypointJson {
                        rgb: format!("#{r:02x}{g:02x}{b:02x}"),
                        name: wp.name,
                        initials: wp.initials,
                        x: wp.x,
                        y: wp.y,
                        z: wp.z,
                        color: wp.color,
                        disabled: wp.disabled,
                        purpose: wp.purpose,
                        set: wp.set,
                        archived: false,
                    }
                })
                .collect(),
        });
    }

    // Vault-only (archived) waypoints: rows the game files no longer contain.
    if let Some(vault) = &st.vault {
        let archived = vault
            .lock()
            .unwrap()
            .waypoints_for_world(&we.world.id, true)
            .unwrap_or_default();
        if !archived.is_empty() {
            // Group per dimension so the client can filter like live files.
            let mut by_dim: HashMap<String, Vec<WaypointJson>> = HashMap::new();
            for wp in archived {
                let [r, g, b] = waypoint_color_rgb(wp.color);
                by_dim
                    .entry(wp.dim_key.clone())
                    .or_default()
                    .push(WaypointJson {
                        rgb: format!("#{r:02x}{g:02x}{b:02x}"),
                        name: wp.name,
                        initials: wp.initials,
                        x: wp.x,
                        y: wp.y,
                        z: wp.z,
                        color: wp.color,
                        disabled: false,
                        purpose: wp.purpose,
                        set: wp.set,
                        archived: true,
                    });
            }
            for (dim_key, waypoints) in by_dim {
                files.push(WaypointFileJson {
                    dim_folder: "vault".into(),
                    dim_key: Some(dim_key),
                    file: "vault".into(),
                    waypoints,
                });
            }
        }
    }
    axum::Json(files).into_response()
}

#[derive(Serialize)]
struct VaultSyncResponse {
    report: xaero_db::vault::VaultSyncReport,
}

async fn api_vault_sync(State(st): State<Arc<AppState>>) -> Response {
    let Some(vault) = st.vault.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "vault unavailable").into_response();
    };
    let worlds = st.worlds.read().unwrap().clone();
    let result = tokio::task::spawn_blocking(move || vault_sync_now(&vault, &worlds))
        .await
        .unwrap_or(Err("sync task failed".into()));
    match result {
        Ok(report) => axum::Json(VaultSyncResponse { report }).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn api_refresh(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    // Epoch first, so watcher batches classified before the clear drop
    // themselves instead of resurrecting a stale index.
    st.epoch.fetch_add(1, Ordering::SeqCst);
    st.indexes.write().unwrap().clear();
    st.tiles.lock().unwrap().clear();
    st.stale_stamps.lock().unwrap().clear();
    st.generation.fetch_add(1, Ordering::Relaxed);
    StatusCode::NO_CONTENT
}

// ------------------------------------------------------------------- tiles --

fn parse_layer(layer: &str) -> Option<Option<i32>> {
    if layer == "surface" {
        return Some(None);
    }
    layer.strip_prefix("cave-")?.parse().ok().map(Some)
}

/// Query of a tile request. `v` is the live updater's cache-buster and is
/// deliberately ignored here; `roof` selects the see-through-roof view.
#[derive(serde::Deserialize)]
struct TileQuery {
    #[serde(default)]
    roof: Option<String>,
}

/// `roof=<obsidian>,<snow>`, each 0..=255. Anything else is ignored rather
/// than refused: a tile request is not the place to argue about a query.
fn parse_roof(q: &TileQuery) -> Option<(u8, u8)> {
    let raw = q.roof.as_deref()?;
    let (o, s) = raw.split_once(',')?;
    Some((o.trim().parse().ok()?, s.trim().parse().ok()?))
}

async fn tile(
    State(st): State<Arc<AppState>>,
    headers: header::HeaderMap,
    Query(q): Query<TileQuery>,
    AxPath((w, d, m, layer, z, x, y)): AxPath<(usize, usize, usize, String, i32, i32, i32)>,
) -> Response {
    let Some(cave) = parse_layer(&layer) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let map = MapId {
        world: w,
        dim: d,
        mw: m,
        cave,
        roof: parse_roof(&q),
    };
    if !(-16..=0).contains(&z) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // Tiles are hundreds of KB and a pan re-requests most of the screen, so
    // revalidation matters: the cache stamp (a region's mtime, or the map
    // generation for composed tiles) is exactly the identity of the bytes,
    // and a matching If-None-Match answers 304 *before* any rendering.
    let inm: Option<String> = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let result = tokio::task::spawn_blocking(move || tile_blocking(&st, map, z, x, y, inm))
        .await
        .unwrap_or(Err("render task failed".into()));
    match result {
        Ok(TileResp::NotModified(etag)) => {
            (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response()
        }
        Ok(TileResp::Full(tile, etag)) => {
            let body = match tile {
                Some(png) => png.as_ref().clone(),
                // A real transparent PNG, not 204: browsers render bodyless
                // image responses as garbage/broken tiles.
                None => empty_tile_png().to_vec(),
            };
            (
                [
                    (header::CONTENT_TYPE, "image/png".to_string()),
                    (header::CACHE_CONTROL, "no-cache".to_string()),
                    (header::ETAG, etag),
                ],
                body,
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// What `tile_blocking` answers: a body with its ETag, or just the ETag when
/// the client's If-None-Match already matches (no render happened).
enum TileResp {
    NotModified(String),
    Full(EncodedTile, String),
}

/// The browser-facing tile identity. Besides the coordinates and content
/// stamp it carries the layer directory's hash: MapIds are positional, so
/// after a roots change the same URL can point at a different world — an
/// mtime collision must not let a stale browser entry revalidate.
fn tile_etag(dir_hash: u64, z: i32, x: i32, y: i32, stamp: u64) -> String {
    format!("\"{z}.{x}.{y}.{stamp}.{dir_hash:x}\"")
}

fn inm_matches(inm: &Option<String>, etag: &str) -> bool {
    inm.as_deref()
        .is_some_and(|v| v.split(',').any(|c| c.trim() == etag))
}

fn hash_path(p: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.hash(&mut h);
    h.finish()
}

/// Full-size fully transparent tile, encoded once. Full-size (not a stretched
/// 1x1) so browsers never draw scaling/outline artifacts for empty areas.
fn empty_tile_png() -> &'static [u8] {
    static PNG: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    PNG.get_or_init(|| encode_png(&vec![0u8; TILE * TILE * 4]).expect("empty tile"))
}

type IndexHandle = (
    Arc<RegionIndex>,
    u64,
    Arc<HashMap<(i32, i32), Vec<(i32, i32)>>>,
);

/// A region-index build in progress; later requests for the same map wait on
/// it instead of scanning the same million-file folder in parallel.
struct IndexSlot {
    done: Mutex<bool>,
    ready: std::sync::Condvar,
}

fn get_index(st: &AppState, map: &MapId) -> Result<IndexHandle, String> {
    loop {
        if let Some(cache) = st.indexes.read().unwrap().get(map) {
            return Ok((cache.index.clone(), cache.gen, cache.buckets.clone()));
        }
        let waiter = {
            let mut infl = st.index_inflight.lock().unwrap();
            match infl.get(map) {
                Some(slot) => Some(slot.clone()),
                None => {
                    infl.insert(
                        map.clone(),
                        Arc::new(IndexSlot {
                            done: Mutex::new(false),
                            ready: std::sync::Condvar::new(),
                        }),
                    );
                    None
                }
            }
        };
        match waiter {
            Some(slot) => {
                // Follower: wait for the leader, then re-check the cache. If
                // the leader failed, the loop makes this request the next
                // leader and the error (if persistent) surfaces here too.
                let mut done = slot.done.lock().unwrap();
                while !*done {
                    done = slot.ready.wait(done).unwrap();
                }
                continue;
            }
            None => {
                // Leader. The guard releases waiters even if the build panics.
                struct Release<'a> {
                    st: &'a AppState,
                    map: &'a MapId,
                }
                impl Drop for Release<'_> {
                    fn drop(&mut self) {
                        if let Some(slot) = self.st.index_inflight.lock().unwrap().remove(self.map)
                        {
                            *slot.done.lock().unwrap() = true;
                            slot.ready.notify_all();
                        }
                    }
                }
                let _release = Release { st, map };
                return build_index(st, map);
            }
        }
    }
}

fn build_index(st: &AppState, map: &MapId) -> Result<IndexHandle, String> {
    let worlds = st.worlds.read().unwrap().clone();
    let we = worlds.get(map.world).ok_or("no such world")?;
    let dim = we.world.dims.get(map.dim).ok_or("no such dimension")?;
    let mw = dim.multiworlds.get(map.mw).ok_or("no such multiworld")?;
    let wm = we
        .world
        .world_map_path
        .as_ref()
        .ok_or("world has no map data")?;
    let build_start = std::time::Instant::now();
    let dir = layer_dir(wm, &dim.folder, &mw.id, map.cave);
    let idx = index_regions(&dir).map_err(|e| format!("index {}: {e}", dir.display()))?;
    let index = Arc::new(idx);
    let gen = st.generation.fetch_add(1, Ordering::Relaxed) + 1;
    // Insert BEFORE replaying the recent-changes ring: from here on a
    // concurrent batch either sees this map in `indexes` (and applies its
    // changes there itself), or pushed its ring entries before the read
    // below — either way nothing raced past both.
    let buckets = Arc::new(build_buckets(&index));
    st.indexes.write().unwrap().insert(
        map.clone(),
        MapCache {
            index: index.clone(),
            gen,
            buckets: buckets.clone(),
        },
    );
    let replay: Vec<(i32, i32)> = {
        let ring = st.recent.lock().unwrap();
        ring.iter()
            .filter(|(m, _, _, t)| m == map && *t >= build_start)
            .map(|(_, rx, rz, _)| (*rx, *rz))
            .collect()
    };
    if !replay.is_empty() {
        let mut fixed = (*index).clone();
        for (rx, rz) in replay {
            match live::stat_region(&dir, rx, rz) {
                Some(meta) => {
                    fixed.entries.insert((rx, rz), meta);
                }
                None => {
                    fixed.entries.remove(&(rx, rz));
                }
            }
        }
        let fixed = Arc::new(fixed);
        let fixed_buckets = Arc::new(build_buckets(&fixed));
        if let Some(cache) = st.indexes.write().unwrap().get_mut(map) {
            cache.index = fixed.clone();
            cache.buckets = fixed_buckets.clone();
        }
        return Ok((fixed, gen, fixed_buckets));
    }
    Ok((index, gen, buckets))
}

fn tile_blocking(
    st: &Arc<AppState>,
    map: MapId,
    z: i32,
    x: i32,
    y: i32,
    inm: Option<String>,
) -> Result<TileResp, String> {
    // A native tile is one region, and its filename is derivable from the tile
    // coordinates, so it does not need the region index at all. Building that
    // index means stat()ing every file in the folder — 1M+ on a real 2b2t
    // archive, tens of seconds — which used to be charged to the very first
    // tile the viewer asked for. Serve the pixels now and let the index build
    // when something actually needs it.
    if z == 0 {
        let indexed = st
            .indexes
            .read()
            .unwrap()
            .get(&map)
            .map(|c| c.index.clone());
        let (dir, meta) = match &indexed {
            Some(index) => match index.entries.get(&(x, y)) {
                None => {
                    let etag = tile_etag(hash_path(&index.dir), z, x, y, 0);
                    if inm_matches(&inm, &etag) {
                        return Ok(TileResp::NotModified(etag));
                    }
                    return Ok(TileResp::Full(None, etag));
                }
                Some(meta) => (index.dir.clone(), *meta),
            },
            None => {
                let dir = layer_dir_for(st, &map)?;
                match live::stat_region(&dir, x, y) {
                    None => {
                        let etag = tile_etag(hash_path(&dir), z, x, y, 0);
                        if inm_matches(&inm, &etag) {
                            return Ok(TileResp::NotModified(etag));
                        }
                        return Ok(TileResp::Full(None, etag));
                    }
                    Some(meta) => (dir, meta),
                }
            }
        };
        let etag = tile_etag(hash_path(&dir), z, x, y, meta.mtime_ms);
        if inm_matches(&inm, &etag) {
            return Ok(TileResp::NotModified(etag));
        }
        let key = TileKey {
            map: map.clone(),
            db: None,
            tint: 0, // not a highlight tile
            z,
            x,
            y,
            stamp: meta.mtime_ms,
        };
        if let Some(hit) = st.tiles.lock().unwrap().get(&key) {
            return Ok(TileResp::Full(hit.clone(), etag));
        }
        let path = dir.join(format!(
            "{x}_{y}.{}",
            if meta.is_zip { "zip" } else { "xaero" }
        ));
        return single_flight(st, &key, || {
            // Another request may have finished this tile while we queued.
            if let Some(hit) = st.tiles.lock().unwrap().get(&key) {
                return Ok((hit.clone(), meta.mtime_ms));
            }
            let encoded = match render_native(st, &map, &path)? {
                None => None,
                Some(buf) => Some(Arc::new(encode_png(&buf)?)),
            };
            let size = encoded.as_ref().map(|p| p.len()).unwrap_or(0) + 64;
            st.tiles
                .lock()
                .unwrap()
                .put(key.clone(), encoded.clone(), size);
            Ok((encoded, meta.mtime_ms))
        })
        .map(|(tile, _)| TileResp::Full(tile, etag));
    }

    let (index, gen, buckets) = get_index(st, &map)?;
    let dir_hash = hash_path(&index.dir);
    // Mid-zoom tiles decode every covered region on each compose, and their
    // content depends only on those regions — never on cache warmth (the cold
    // budget can't be exceeded at <= 64 regions). Stamping them by the
    // covered regions instead of the map generation keeps them cached (and
    // lets browsers revalidate) across the generation bumps that live play
    // and warm-up produce for unrelated areas.
    let span = 1i64 << (-z);
    let full_tier = (TILE >> (-z)) > THUMB;
    let stamp = if full_tier {
        let mut in_range =
            regions_in_range(&index, &buckets, x as i64 * span, y as i64 * span, span);
        // Bucket order is HashMap order, which changes with every index
        // rebuild; the stamp must not, or every live change anywhere would
        // re-decode every mid-zoom tile. Sort so unchanged content keeps its
        // ETag.
        in_range.sort_unstable();
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        in_range.len().hash(&mut h);
        for &((rx, rz), mtime) in &in_range {
            (rx, rz, mtime).hash(&mut h);
        }
        h.finish()
    } else {
        gen
    };
    let etag = tile_etag(dir_hash, z, x, y, stamp);
    if inm_matches(&inm, &etag) {
        return Ok(TileResp::NotModified(etag));
    }
    let key = TileKey {
        map: map.clone(),
        db: None,
        tint: 0, // not a highlight tile
        z,
        x,
        y,
        stamp,
    };
    if let Some(hit) = st.tiles.lock().unwrap().get(&key) {
        return Ok(TileResp::Full(hit.clone(), etag));
    }
    single_flight(st, &key, || {
        // A zoomed-out tile is the expensive kind, so re-check the cache: the
        // request we queued behind may have been rendering exactly this.
        if let Some(hit) = st.tiles.lock().unwrap().get(&key) {
            return Ok((hit.clone(), stamp));
        }
        let composed = tile_rgba(st, &index, &buckets, &map, z, x, y)?;
        let (encoded, imagery): (EncodedTile, bool) = match composed {
            Composed::Empty => (None, false),
            Composed::Imagery(buf) => (Some(Arc::new(encode_png(&buf)?)), true),
            Composed::Rects(buf) => (Some(Arc::new(encode_png(&buf)?)), false),
            // Cached imagery from before the caches went cold; still imagery.
            Composed::Stale(png) => (Some(png), true),
        };
        let size = encoded.as_ref().map(|p| p.len()).unwrap_or(0) + 64;
        st.tiles
            .lock()
            .unwrap()
            .put(key.clone(), encoded.clone(), size);
        if imagery {
            let mut stale = st.stale_stamps.lock().unwrap();
            if stale.len() > 200_000 {
                stale.clear(); // safety valve; repopulates as tiles render
            }
            stale.insert((map.clone(), z, x, y), stamp);
        }
        Ok((encoded, stamp))
    })
    .map(|(tile, _)| TileResp::Full(tile, etag))
}

/// What an overzoom compose produced.
enum Composed {
    /// No regions in range.
    Empty,
    /// Real imagery from thumbnails/decodes.
    Imagery(Vec<u8>),
    /// Coverage rectangles (index-only fallback).
    Rects(Vec<u8>),
    /// A previously-encoded imagery PNG served while the caches re-warm.
    Stale(Arc<Vec<u8>>),
}

/// Which per-region source a compose draws from at this zoom.
#[derive(PartialEq, Clone, Copy)]
enum Tier {
    /// cell <= MIP px: the deep-zoom 8px thumbnails.
    Mip,
    /// cell <= THUMB px: the 32px thumbnails.
    Thumb,
    /// Bigger cells: full region decodes.
    Full,
}

/// The subset of `in_range` whose tier source is not already in RAM (each
/// costs a decode — or a store read after `prefetch_store`). Returning the
/// regions themselves lets the warm queue work on exactly these instead of an
/// arbitrary prefix of the range, which is what makes warm-up converge.
fn cold_regions(
    st: &AppState,
    map: &MapId,
    in_range: &[((i32, i32), u64)],
    tier: Tier,
) -> Vec<((i32, i32), u64)> {
    match tier {
        Tier::Full => in_range.to_vec(),
        Tier::Thumb => {
            let mut thumbs = st.thumbs.lock().unwrap();
            in_range
                .iter()
                .filter(|&&((rx, rz), mtime_ms)| {
                    thumbs.get(&thumb_key(map, rx, rz, mtime_ms)).is_none()
                })
                .copied()
                .collect()
        }
        Tier::Mip => {
            // A 32px thumbnail in RAM counts as warm too: deriving the 8px
            // mip from it is a downscale, not a decode.
            let mut cold: Vec<((i32, i32), u64)> = {
                let mut mips = st.mips.lock().unwrap();
                in_range
                    .iter()
                    .filter(|&&((rx, rz), mtime_ms)| {
                        mips.get(&thumb_key(map, rx, rz, mtime_ms)).is_none()
                    })
                    .copied()
                    .collect()
            };
            let mut thumbs = st.thumbs.lock().unwrap();
            cold.retain(|&((rx, rz), mtime_ms)| {
                thumbs.get(&thumb_key(map, rx, rz, mtime_ms)).is_none()
            });
            cold
        }
    }
}

/// The thumbnail store's key for a layer directory. A see-through-roof view
/// is different imagery of the same regions, so it gets its own rows instead
/// of overwriting the plain ones. The separator is a control character, which
/// no real path carries.
fn store_dir(index: &RegionIndex, map: &MapId) -> String {
    /// No real path carries a control character, so a variant can never be
    /// confused with a directory that happens to be named like one.
    const SEP: char = '\u{1}';
    let dir = index.dir.display().to_string();
    match map.roof {
        Some((o, s)) => format!("{dir}{SEP}roof{o},{s}"),
        None => dir,
    }
}

/// Drops the bbox-prefetch memo for one tile (see the compose path).
fn forget_prefetch(
    st: &AppState,
    index: &RegionIndex,
    map: &MapId,
    x0: i64,
    y0: i64,
    span: i64,
    tier: Tier,
) {
    let key = (store_dir(index, map), x0, y0, span, tier == Tier::Thumb);
    lock_ok(&st.prefetched).remove(&key);
}

fn thumb_key(map: &MapId, rx: i32, rz: i32, mtime_ms: u64) -> ThumbKey {
    ThumbKey {
        map: map.clone(),
        rx,
        rz,
        mtime_ms,
    }
}

/// Loads every stored thumbnail the tile needs into the RAM tier in one bbox
/// query, so the cold count afterwards reflects true decodes only. This is
/// what keeps a warmed archive warm across evictions and restarts.
fn prefetch_store(
    st: &AppState,
    index: &RegionIndex,
    map: &MapId,
    x0: i64,
    y0: i64,
    span: i64,
    tier: Tier,
) {
    let Some(store) = &st.pyramid else { return };
    let (Ok(rx0), Ok(rz0)) = (i32::try_from(x0), i32::try_from(y0)) else {
        return;
    };
    let rx1 = i32::try_from(x0 + span - 1).unwrap_or(i32::MAX);
    let rz1 = i32::try_from(y0 + span - 1).unwrap_or(i32::MAX);
    let dir = store_dir(index, map);
    // A deep tile's bbox can cover most of the store; loading it again when
    // nothing has been written since the last time cannot find anything new.
    let pf_key = (dir.clone(), x0, y0, span, tier == Tier::Thumb);
    let writes_now = store.writes();
    {
        let mut seen = st.prefetched.lock().unwrap();
        if seen.get(&pf_key) == Some(&writes_now) {
            return;
        }
        if seen.len() > 4096 {
            seen.clear();
        }
        seen.insert(pf_key, writes_now);
    }
    let rows = store.load_bbox(&dir, rx0, rz0, rx1, rz1, tier == Tier::Thumb);
    if rows.is_empty() {
        return;
    }
    match tier {
        Tier::Thumb => {
            let mut thumbs = st.thumbs.lock().unwrap();
            for (rx, rz, mtime, blob) in rows {
                let live = index.entries.get(&(rx, rz)).map(|m| m.mtime_ms);
                if live != Some(mtime) || blob.len() != THUMB * THUMB * 4 {
                    continue;
                }
                let key = thumb_key(map, rx, rz, mtime);
                if thumbs.get(&key).is_none() {
                    let size = blob.len();
                    thumbs.put(key, Arc::new(blob), size);
                }
            }
        }
        Tier::Mip => {
            let mut mips = st.mips.lock().unwrap();
            for (rx, rz, mtime, blob) in rows {
                let live = index.entries.get(&(rx, rz)).map(|m| m.mtime_ms);
                if live != Some(mtime) || blob.len() != MIP * MIP * 4 {
                    continue;
                }
                let key = thumb_key(map, rx, rz, mtime);
                if mips.get(&key).is_none() {
                    let size = blob.len();
                    mips.put(key, Arc::new(blob), size);
                }
            }
        }
        Tier::Full => {}
    }
}

/// The last imagery PNG this tile rendered, if it is still in the tile cache.
fn stale_tile(st: &AppState, map: &MapId, z: i32, x: i32, y: i32) -> Option<Arc<Vec<u8>>> {
    let stamp = st
        .stale_stamps
        .lock()
        .unwrap()
        .get(&(map.clone(), z, x, y))
        .copied()?;
    let key = TileKey {
        map: map.clone(),
        db: None,
        tint: 0, // not a highlight tile
        z,
        x,
        y,
        stamp,
    };
    st.tiles.lock().unwrap().get(&key).cloned().flatten()
}

fn tile_rgba(
    st: &Arc<AppState>,
    index: &RegionIndex,
    buckets: &RegionBuckets,
    map: &MapId,
    z: i32,
    x: i32,
    y: i32,
) -> Result<Composed, String> {
    if z == 0 {
        let Some(path) = index.region_path(x, y) else {
            return Ok(Composed::Empty);
        };
        return Ok(match render_native(st, map, &path)? {
            Some(buf) => Composed::Imagery(buf),
            None => Composed::Empty,
        });
    }

    let span = 1i64 << (-z); // regions per tile axis
    let x0 = x as i64 * span;
    let y0 = y as i64 * span;
    let in_range = regions_in_range(index, buckets, x0, y0, span);
    if in_range.is_empty() {
        return Ok(Composed::Empty);
    }

    // Real imagery for as long as the work is affordable. Regions whose
    // thumbnail is already cached (RAM, or the persistent store after a
    // prefetch) are nearly free, so only the ones that still need decoding
    // count against the budget: a warmed pyramid keeps drawing imagery at
    // zoom levels a cold one cannot afford. `cell == 0` (a region smaller
    // than a pixel) composes sub-pixel from the mip tier — the whole-archive
    // zooms must show real terrain colors, not just coverage rectangles.
    let cell = if -z < 10 { TILE >> (-z) } else { 0 };
    {
        let tier = if cell == 0 || cell <= MIP {
            Tier::Mip
        } else if cell <= THUMB {
            Tier::Thumb
        } else {
            Tier::Full
        };
        let mut cold = cold_regions(st, map, &in_range, tier);
        // A bbox prefetch is one big query; worth it only when the compose
        // would otherwise be refused. Small cold sets are served by the
        // per-region store lookups inside the compose itself.
        if cold.len() > WARM_SYNC_BUDGET && tier != Tier::Full {
            prefetch_store(st, index, map, x0, y0, span, tier);
            cold = cold_regions(st, map, &cold, tier);
            if cold.len() > WARM_SYNC_BUDGET {
                // The memo assumes what the prefetch loaded is still in the
                // RAM tier. Still cold means it is not (evicted, or never
                // stored): the next request must ask the store again rather
                // than skip on an unchanged write count.
                forget_prefetch(st, index, map, x0, y0, span, tier);
            }
        }
        // Decoding thousands of cold regions inline means a viewer staring at
        // nothing for a minute. Past the synchronous budget, answer now — with
        // the tile's previous imagery if it is still cached, else coverage
        // rectangles — and build the thumbnails in the background; the live
        // socket tells the viewer to re-fetch once they exist.
        if cold.len() > WARM_SYNC_BUDGET {
            schedule_warm(st, map, &cold);
            if let Some(png) = stale_tile(st, map, z, x, y) {
                return Ok(Composed::Stale(png));
            }
        } else {
            let opts = render_opts_for(st, map);
            use rayon::prelude::*;
            if cell == 0 {
                // Sub-pixel: average each region's mip to one color and
                // accumulate it (alpha-weighted) into its output pixel.
                let regions_per_px = ((span as usize) / TILE) * ((span as usize) / TILE);
                let dots: Vec<(usize, [f32; 4])> = in_range
                    .par_iter()
                    .filter_map(|&((rx, rz), mtime_ms)| {
                        let mip = region_mip(st, index, map, rx, rz, mtime_ms, &opts)?;
                        let (mut r, mut g, mut b, mut a) = (0f32, 0f32, 0f32, 0f32);
                        for i in (0..mip.len()).step_by(4) {
                            let al = mip[i + 3] as f32 / 255.0;
                            r += mip[i] as f32 * al;
                            g += mip[i + 1] as f32 * al;
                            b += mip[i + 2] as f32 * al;
                            a += al;
                        }
                        if a <= 0.0 {
                            return None;
                        }
                        let px_x = ((rx as i64 - x0) as usize * TILE) / span as usize;
                        let px_y = ((rz as i64 - y0) as usize * TILE) / span as usize;
                        Some((px_y * TILE + px_x, [r, g, b, a]))
                    })
                    .collect();
                let mut acc = vec![0f32; TILE * TILE * 4];
                for (i, [r, g, b, a]) in dots {
                    acc[i * 4] += r;
                    acc[i * 4 + 1] += g;
                    acc[i * 4 + 2] += b;
                    acc[i * 4 + 3] += a;
                }
                let mut out = vec![0u8; TILE * TILE * 4];
                let full = (MIP * MIP) as f32; // per-region alpha sum of a fully opaque mip
                for i in 0..TILE * TILE {
                    let a = acc[i * 4 + 3];
                    if a <= 0.0 {
                        continue;
                    }
                    out[i * 4] = (acc[i * 4] / a).min(255.0) as u8;
                    out[i * 4 + 1] = (acc[i * 4 + 1] / a).min(255.0) as u8;
                    out[i * 4 + 2] = (acc[i * 4 + 2] / a).min(255.0) as u8;
                    // Coverage: how much of the pixel's region slots hold
                    // opaque terrain. Sparse exploration stays translucent.
                    let coverage = a / (full * regions_per_px as f32);
                    out[i * 4 + 3] = (coverage.min(1.0) * 255.0).max(64.0) as u8;
                }
                return Ok(Composed::Imagery(out));
            }
            let cells: Vec<((usize, usize), Vec<u8>)> = in_range
                .par_iter()
                .filter_map(|&((rx, rz), mtime_ms)| {
                    let small = match tier {
                        Tier::Mip => {
                            let mip = region_mip(st, index, map, rx, rz, mtime_ms, &opts)?;
                            if cell == MIP {
                                mip.as_ref().clone()
                            } else {
                                downscale_box_from(&mip, MIP, cell)
                            }
                        }
                        Tier::Thumb => {
                            let thumb = region_thumb(st, index, map, rx, rz, mtime_ms, &opts)?;
                            if cell == THUMB {
                                thumb.as_ref().clone()
                            } else {
                                downscale_box_from(&thumb, THUMB, cell)
                            }
                        }
                        Tier::Full => {
                            let rgba = render_region_at(st, index, rx, rz, &opts)?;
                            downscale_box_from(&rgba, TILE, cell)
                        }
                    };
                    Some((
                        ((rx as i64 - x0) as usize, (rz as i64 - y0) as usize),
                        small,
                    ))
                })
                .collect();
            let mut out = vec![0u8; TILE * TILE * 4];
            for ((gx, gy), small) in &cells {
                for cy in 0..cell {
                    let dst = ((gy * cell + cy) * TILE + gx * cell) * 4;
                    let src = cy * cell * 4;
                    out[dst..dst + cell * 4].copy_from_slice(&small[src..src + cell * 4]);
                }
            }
            return Ok(Composed::Imagery(out));
        }
    }

    // Beyond the imagery budget: coverage rectangles straight from the index.
    let mut out = vec![0u8; TILE * TILE * 4];
    let px_per_region = (TILE as f64) / span as f64;
    let cell = px_per_region.ceil().max(1.0) as usize;
    for &((rx, rz), mtime_ms) in &in_range {
        let (rx, rz) = (rx as i64, rz as i64);
        let px = ((rx - x0) as f64 * px_per_region) as usize;
        let py = ((rz - y0) as f64 * px_per_region) as usize;
        // Age tint: newer = brighter steel blue, older = darker slate.
        let age_days = now_ms().saturating_sub(mtime_ms) / 86_400_000;
        let t = (age_days as f32 / 720.0).min(1.0); // 2 years fade
        let color = [
            (110.0 - 40.0 * t) as u8,
            (150.0 - 60.0 * t) as u8,
            (190.0 - 80.0 * t) as u8,
            230,
        ];
        for dy in 0..cell {
            for dx in 0..cell {
                let (ax, ay) = (px + dx, py + dy);
                if ax < TILE && ay < TILE {
                    let i = (ay * TILE + ax) * 4;
                    out[i..i + 4].copy_from_slice(&color);
                }
            }
        }
    }
    Ok(Composed::Rects(out))
}

/// Render options for one map layer: nether-type dimensions get their ambient
/// light and logical height, and the selected cave layer forces every tile's
/// cave mode so legacy tiles without a stored value follow the layer they sit
/// in. Falls back to overworld defaults when the map cannot be resolved.
fn render_opts_for(st: &AppState, map: &MapId) -> RenderOpts {
    let mut opts = RenderOpts {
        cave_override: map.cave,
        roof: map
            .roof
            .map(|(obsidian, snow)| xaero_core::render::RoofAlpha { obsidian, snow }),
        ..Default::default()
    };
    let worlds = st.worlds.read().unwrap().clone();
    let nether = worlds
        .get(map.world)
        .and_then(|we| we.world.dims.get(map.dim))
        .is_some_and(|dim| dim.dimension_type() == Some(Dimension::Nether));
    if nether {
        opts.dim_ambient = 0.1;
        opts.logical_height = 128;
    }
    opts
}

/// The on-disk folder holding a map's region files, without touching (or
/// building) the region index.
fn layer_dir_for(st: &AppState, map: &MapId) -> Result<PathBuf, String> {
    let worlds = st.worlds.read().unwrap().clone();
    let we = worlds.get(map.world).ok_or("no such world")?;
    let dim = we.world.dims.get(map.dim).ok_or("no such dimension")?;
    let mw = dim.multiworlds.get(map.mw).ok_or("no such multiworld")?;
    let wm = we
        .world
        .world_map_path
        .as_ref()
        .ok_or("world has no map data")?;
    Ok(layer_dir(wm, &dim.folder, &mw.id, map.cave))
}

/// Renders one region file at native resolution. `Ok(None)` when the region is
/// missing or unusable — never an error, so a single bad file cannot fail the
/// request.
fn render_native(
    st: &AppState,
    map: &MapId,
    path: &std::path::Path,
) -> Result<Option<Vec<u8>>, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        // Region vanished mid-rename; treat as empty rather than erroring.
        Err(_) => return Ok(None),
    };
    let dec = match xaero_core::read_region_container(&bytes)
        .and_then(|stream| xaero_core::decode_region(&stream))
    {
        Ok(dec) => dec,
        Err(e) => {
            note_unreadable(st, path, &e.to_string());
            return Ok(None);
        }
    };
    if dec.truncated {
        note_unreadable(
            st,
            path,
            &format!("truncated region ({} bytes unread)", dec.trailing),
        );
    }
    let opts = render_opts_for(st, map);
    Ok(Some(xaero_core::render::render_region(&dec, &st.ct, &opts)))
}

/// Regions covered by a zoomed-out tile, found through the bucket grid.
///
/// Tiles are power-of-two sized and aligned, so a tile either sits within one
/// bucket or covers whole buckets; both cases visit only buckets that can
/// contain a hit, which is what keeps this off the full index.
fn regions_in_range(
    index: &RegionIndex,
    buckets: &RegionBuckets,
    x0: i64,
    y0: i64,
    span: i64,
) -> Vec<((i32, i32), u64)> {
    let bucket = BUCKET as i64;
    let bx0 = x0.div_euclid(bucket);
    let bz0 = y0.div_euclid(bucket);
    let bx1 = (x0 + span - 1).div_euclid(bucket);
    let bz1 = (y0 + span - 1).div_euclid(bucket);
    let mut out = Vec::new();
    for bx in bx0..=bx1 {
        for bz in bz0..=bz1 {
            let Ok(bx) = i32::try_from(bx) else { continue };
            let Ok(bz) = i32::try_from(bz) else { continue };
            let Some(list) = buckets.get(&(bx, bz)) else {
                continue;
            };
            for &(rx, rz) in list {
                if (rx as i64) >= x0
                    && (rx as i64) < x0 + span
                    && (rz as i64) >= y0
                    && (rz as i64) < y0 + span
                {
                    if let Some(meta) = index.entries.get(&(rx, rz)) {
                        out.push(((rx, rz), meta.mtime_ms));
                    }
                }
            }
        }
    }
    out
}

/// Why a region produced no imagery.
enum RegionMiss {
    /// Not in the index, or vanished/unreadable on disk — normal on a live
    /// archive mid-rename, and not worth remembering.
    Gone,
    /// Read fine but will not decode: reported to diagnostics, and worth
    /// remembering so it is not re-read on every warm round.
    Undecodable,
}

/// Decodes and renders one region at full size.
fn render_region_checked(
    st: &AppState,
    index: &RegionIndex,
    rx: i32,
    rz: i32,
    opts: &RenderOpts,
) -> Result<Vec<u8>, RegionMiss> {
    let path = index.region_path(rx, rz).ok_or(RegionMiss::Gone)?;
    let bytes = std::fs::read(&path).map_err(|_| RegionMiss::Gone)?;
    match xaero_core::read_region_container(&bytes)
        .and_then(|stream| xaero_core::decode_region(&stream))
    {
        Ok(dec) => Ok(xaero_core::render::render_region(&dec, &st.ct, opts)),
        Err(e) => {
            note_unreadable(st, &path, &e.to_string());
            Err(RegionMiss::Undecodable)
        }
    }
}

/// Decodes and renders one region at full size. `None` when the file is gone,
/// unreadable or undecodable — a bad region must leave a hole, never fail the
/// whole tile.
fn render_region_at(
    st: &AppState,
    index: &RegionIndex,
    rx: i32,
    rz: i32,
    opts: &RenderOpts,
) -> Option<Vec<u8>> {
    render_region_checked(st, index, rx, rz, opts).ok()
}

/// Background thumbnail work, one entry per tile request that ran past the
/// synchronous budget. Newest first — the current view has priority.
struct WarmQueue {
    jobs: Mutex<VecDeque<WarmJob>>,
    running: AtomicBool,
}

struct WarmJob {
    map: MapId,
    work: Vec<((i32, i32), u64)>,
}

/// Queues a tile's cold regions for background thumbnailing and makes sure a
/// worker is draining the queue. Newest request first: the zoom level being
/// looked at right now always warms before areas the viewer already left, and
/// a request that arrives while the worker is busy queues instead of being
/// dropped. One worker thread total, so a 1M-region archive can never have a
/// job per pan all competing for the same cores. Callers pass only regions
/// that are actually cold, and coordinates already queued for the map are not
/// queued twice — every job makes real progress, so warm-up converges.
fn schedule_warm(st: &Arc<AppState>, map: &MapId, cold: &[((i32, i32), u64)]) {
    {
        let mut jobs = st.warm.jobs.lock().unwrap();
        let queued: std::collections::HashSet<(i32, i32)> = jobs
            .iter()
            .filter(|j| j.map == *map)
            .flat_map(|j| j.work.iter().map(|w| w.0))
            .collect();
        let work: Vec<((i32, i32), u64)> = cold
            .iter()
            .filter(|w| !queued.contains(&w.0))
            .take(WARM_JOB_CAP)
            .copied()
            .collect();
        if !work.is_empty() {
            jobs.push_front(WarmJob {
                map: map.clone(),
                work,
            });
            while jobs.len() > WARM_QUEUE_CAP {
                jobs.pop_back(); // the oldest view loses its slot, never the newest
            }
        } else if jobs.is_empty() {
            return; // nothing new and nothing queued: no worker needed
        }
    }
    if st
        .warm
        .running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return; // the running worker picks the new job up next
    }
    let st = st.clone();
    std::thread::spawn(move || {
        // Clear the flag even if a render panics.
        struct Done {
            st: Arc<AppState>,
            armed: bool,
        }
        impl Drop for Done {
            fn drop(&mut self) {
                if self.armed {
                    self.st.warm.running.store(false, Ordering::Release);
                }
            }
        }
        let mut done = Done {
            st: st.clone(),
            armed: true,
        };
        loop {
            let job = st.warm.jobs.lock().unwrap().pop_front();
            let Some(job) = job else {
                // Queue drained. Release the flag, then close the race with an
                // enqueue that lost the CAS in the gap: retake the flag and
                // keep draining if something arrived.
                st.warm.running.store(false, Ordering::Release);
                done.armed = false;
                if !st.warm.jobs.lock().unwrap().is_empty()
                    && st
                        .warm
                        .running
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    done.armed = true;
                    continue;
                }
                return;
            };
            warm_job(&st, job);
        }
    });
}

/// Renders one job's thumbnails in WARM_CHUNK batches. Every finished chunk
/// bumps the map generation and tells viewers to refresh exactly those
/// regions, so imagery replaces coverage rectangles progressively instead of
/// after the whole job. Between chunks the worker yields to newer requests
/// (the remainder goes to the back of the queue): the current view always
/// renders first. Cached thumbnails make requeued work nearly free.
fn warm_job(st: &Arc<AppState>, job: WarmJob) {
    let Ok((index, _, _)) = get_index(st, &job.map) else {
        return;
    };
    let opts = render_opts_for(st, &job.map);
    use rayon::prelude::*;
    let mut rest = job.work.as_slice();
    while !rest.is_empty() {
        let (chunk, tail) = rest.split_at(rest.len().min(WARM_CHUNK));
        rest = tail;
        let built: Vec<(i32, i32)> = chunk
            .par_iter()
            .filter(|&&((rx, rz), mtime_ms)| {
                region_thumb(st, &index, &job.map, rx, rz, mtime_ms, &opts).is_some()
            })
            .map(|&((rx, rz), _)| (rx, rz))
            .collect();
        if !built.is_empty() {
            // Composed tiles are keyed by the map generation, so the cached
            // coverage-rectangle answers have to be retired before telling the
            // viewer to re-fetch — otherwise it would just get them again.
            // Re-rendering is cheap now: the thumbnails are warm.
            if let Some(cache) = st.indexes.write().unwrap().get_mut(&job.map) {
                cache.gen = st.generation.fetch_add(1, Ordering::Relaxed) + 1;
            }
            live::emit_tiles(st, &job.map, Some(&built), true);
        }
        if !rest.is_empty() {
            let mut jobs = st.warm.jobs.lock().unwrap();
            if !jobs.is_empty() {
                let work = rest.to_vec();
                jobs.push_back(WarmJob {
                    map: job.map.clone(),
                    work,
                });
                while jobs.len() > WARM_QUEUE_CAP {
                    jobs.pop_back();
                }
                return;
            }
        }
    }
}

/// A tile render in progress, awaited by any later request for the same tile.
struct TileSlot {
    done: Mutex<Option<Result<(EncodedTile, u64), String>>>,
    ready: std::sync::Condvar,
}

/// Runs `render` once per tile key, with concurrent callers for the same key
/// waiting on the result instead of repeating the work.
///
/// The leader publishes its result even if `render` panics (the guard's `Drop`
/// turns a panic into an error for the waiters), so a follower can never block
/// forever.
fn single_flight<F>(st: &AppState, key: &TileKey, render: F) -> Result<(EncodedTile, u64), String>
where
    F: FnOnce() -> Result<(EncodedTile, u64), String>,
{
    let slot = {
        let mut inflight = st.inflight.lock().unwrap();
        match inflight.get(key) {
            Some(existing) => {
                let existing = existing.clone();
                drop(inflight);
                // Follower: wait for the leader to publish.
                let mut done = existing.done.lock().unwrap();
                while done.is_none() {
                    done = existing.ready.wait(done).unwrap();
                }
                return done
                    .clone()
                    .unwrap_or_else(|| Err("render abandoned".into()));
            }
            None => {
                let slot = Arc::new(TileSlot {
                    done: Mutex::new(None),
                    ready: std::sync::Condvar::new(),
                });
                inflight.insert(key.clone(), slot.clone());
                slot
            }
        }
    };

    struct Guard<'a> {
        st: &'a AppState,
        key: TileKey,
        slot: Arc<TileSlot>,
        published: bool,
    }
    impl Drop for Guard<'_> {
        fn drop(&mut self) {
            self.st.inflight.lock().unwrap().remove(&self.key);
            if !self.published {
                // The render panicked; unblock the waiters rather than
                // leaving them on the condvar forever.
                *self.slot.done.lock().unwrap() = Some(Err("render failed".into()));
            }
            self.slot.ready.notify_all();
        }
    }
    let mut guard = Guard {
        st,
        key: key.clone(),
        slot: slot.clone(),
        published: false,
    };

    let result = render();
    *slot.done.lock().unwrap() = Some(result.clone());
    guard.published = true;
    result
}

/// Most recent distinct unreadable regions to remember.
const UNREADABLE_CAP: usize = 200;

/// Records a region we could not use, for /api/diagnostics. Deduplicated by
/// path so one bad file re-requested by every pan cannot flood the list.
fn note_unreadable(st: &AppState, path: &std::path::Path, reason: &str) {
    let key = path.display().to_string();
    let mut list = st.unreadable.lock().unwrap();
    if list.iter().any(|(p, _)| p == &key) {
        return;
    }
    if list.len() >= UNREADABLE_CAP {
        list.pop_back();
    }
    eprintln!("region unreadable: {key}: {reason}");
    list.push_front((key, reason.to_string()));
}

/// Everything the server knows is wrong with the data it is serving.
async fn api_diagnostics(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let unreadable: Vec<_> = st
        .unreadable
        .lock()
        .unwrap()
        .iter()
        .map(|(path, reason)| serde_json::json!({ "path": path, "reason": reason }))
        .collect();
    let (tile_entries, tile_bytes) = {
        let t = st.tiles.lock().unwrap();
        (t.len(), t.bytes())
    };
    let (thumb_entries, thumb_bytes) = {
        let t = st.thumbs.lock().unwrap();
        (t.len(), t.bytes())
    };
    let (mip_entries, mip_bytes) = {
        let t = st.mips.lock().unwrap();
        (t.len(), t.bytes())
    };
    axum::Json(serde_json::json!({
        "unreadable": unreadable,
        "unreadableCapped": UNREADABLE_CAP,
        "caches": {
            "tileEntries": tile_entries,
            "tileBytes": tile_bytes,
            "thumbEntries": thumb_entries,
            "thumbBytes": thumb_bytes,
            "mipEntries": mip_entries,
            "mipBytes": mip_bytes,
            "pyramidStore": st.pyramid.is_some(),
            "previewChunks": st.preview.chunk_count(),
        },
    }))
}

/// A region's THUMB x THUMB thumbnail, rendering and caching it on first use.
///
/// This is the level-of-detail pyramid: every zoom from -4 outwards is composed
/// by downscaling these instead of decoding regions again. The key carries the
/// region's mtime, so an unrelated edit elsewhere in the map cannot evict it.
/// A decode also derives the 8px mip and persists both to the pyramid store,
/// so each on-disk region version costs at most one decode, ever.
fn region_thumb(
    st: &AppState,
    index: &RegionIndex,
    map: &MapId,
    rx: i32,
    rz: i32,
    mtime_ms: u64,
    opts: &RenderOpts,
) -> Option<Arc<Vec<u8>>> {
    let key = thumb_key(map, rx, rz, mtime_ms);
    if let Some(hit) = st.thumbs.lock().unwrap().get(&key) {
        return Some(hit.clone());
    }
    if let Some(store) = &st.pyramid {
        let dir = store_dir(index, map);
        if let Some((t32, t8)) = store.load_one(&dir, rx, rz, mtime_ms) {
            if t32.len() == THUMB * THUMB * 4 && t8.len() == MIP * MIP * 4 {
                let thumb = Arc::new(t32);
                st.thumbs
                    .lock()
                    .unwrap()
                    .put(key.clone(), thumb.clone(), thumb.len());
                let mip = Arc::new(t8);
                st.mips.lock().unwrap().put(key, mip.clone(), mip.len());
                return Some(thumb);
            }
        }
    }
    let (thumb, mip) = match render_region_checked(st, index, rx, rz, opts) {
        Ok(rgba) => {
            let thumb = Arc::new(downscale_box_from(&rgba, TILE, THUMB));
            let mip = Arc::new(downscale_box_from(&thumb, THUMB, MIP));
            (thumb, mip)
        }
        Err(RegionMiss::Gone) => return None,
        // A hole, remembered under this on-disk version like any other
        // thumbnail: the file is read once per version instead of on every
        // warm round, and the tile stops counting it as cold.
        Err(RegionMiss::Undecodable) => (
            Arc::new(vec![0u8; THUMB * THUMB * 4]),
            Arc::new(vec![0u8; MIP * MIP * 4]),
        ),
    };
    st.thumbs
        .lock()
        .unwrap()
        .put(key.clone(), thumb.clone(), thumb.len());
    st.mips.lock().unwrap().put(key, mip.clone(), mip.len());
    if let Some(store) = &st.pyramid {
        store.put(pyramid::ThumbRow {
            dir: store_dir(index, map),
            rx,
            rz,
            mtime_ms,
            t32: thumb.as_ref().clone(),
            t8: mip.as_ref().clone(),
        });
    }
    Some(thumb)
}

/// A region's MIP x MIP (8px) thumbnail — the deep-zoom tier. Derived from the
/// 32px thumbnail when that is at hand (RAM or store), decoded otherwise.
fn region_mip(
    st: &AppState,
    index: &RegionIndex,
    map: &MapId,
    rx: i32,
    rz: i32,
    mtime_ms: u64,
    opts: &RenderOpts,
) -> Option<Arc<Vec<u8>>> {
    let key = thumb_key(map, rx, rz, mtime_ms);
    if let Some(hit) = st.mips.lock().unwrap().get(&key) {
        return Some(hit.clone());
    }
    if let Some(thumb) = st.thumbs.lock().unwrap().get(&key).cloned() {
        let mip = Arc::new(downscale_box_from(&thumb, THUMB, MIP));
        st.mips.lock().unwrap().put(key, mip.clone(), mip.len());
        return Some(mip);
    }
    // region_thumb fills both tiers (store hit or fresh decode).
    region_thumb(st, index, map, rx, rz, mtime_ms, opts)?;
    st.mips.lock().unwrap().get(&key).cloned()
}

/// Alpha-weighted box downscale of a square RGBA buffer to `dst` x `dst`.
fn downscale_box_from(src: &[u8], src_size: usize, dst: usize) -> Vec<u8> {
    if dst == src_size {
        return src.to_vec();
    }
    let f = src_size / dst;
    let mut out = vec![0u8; dst * dst * 4];
    for cy in 0..dst {
        for cx in 0..dst {
            let mut acc = [0u64; 4];
            for sy in 0..f {
                let row = ((cy * f + sy) * src_size + cx * f) * 4;
                for sx in 0..f {
                    let si = row + sx * 4;
                    let a = src[si + 3] as u64;
                    acc[0] += src[si] as u64 * a;
                    acc[1] += src[si + 1] as u64 * a;
                    acc[2] += src[si + 2] as u64 * a;
                    acc[3] += a;
                }
            }
            let di = (cy * dst + cx) * 4;
            let n = (f * f) as u64;
            // Fully transparent source area contributes no colour at all.
            match std::num::NonZeroU64::new(acc[3]) {
                None => out[di..di + 4].copy_from_slice(&[0, 0, 0, 0]),
                Some(alpha) => {
                    out[di] = (acc[0] / alpha) as u8;
                    out[di + 1] = (acc[1] / alpha) as u8;
                    out[di + 2] = (acc[2] / alpha) as u8;
                    out[di + 3] = (acc[3] / n) as u8;
                }
            }
        }
    }
    out
}

// -------------------------------------------------------- highlight tiles --

/// Overlay color per XaeroPlus database (checked in order: first match wins).
const HL_ALPHA: u8 = 110;

/// `c=RRGGBB` overrides the overlay colour (a leading `#` is tolerated).
/// Absent or malformed falls back to the module's palette entry, so a stray
/// query never costs you the overlay. A *repeated* `c=` is a different matter:
/// the `Query` extractor rejects it with 400 before the handler runs.
#[derive(serde::Deserialize)]
struct HighlightTileQuery {
    c: Option<String>,
}

/// Parses `RRGGBB` (a leading `#` tolerated) into a packed 0x00RRGGBB.
fn parse_tint(raw: Option<&str>) -> Option<u32> {
    let hex = raw?.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

async fn highlight_tile(
    State(st): State<Arc<AppState>>,
    headers: header::HeaderMap,
    Query(q): Query<HighlightTileQuery>,
    AxPath((w, db, d, z, x, y)): AxPath<(usize, String, usize, i32, i32, i32)>,
) -> Response {
    if !(-16..=0).contains(&z) || db.contains('/') || db.contains('\\') || !db.ends_with(".db") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let tint = parse_tint(q.c.as_deref());
    let inm: Option<String> = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let result = tokio::task::spawn_blocking(move || {
        highlight_tile_blocking(&st, w, &db, d, z, x, y, tint, inm)
    })
    .await
    .unwrap_or(Err("render task failed".into()));
    match result {
        Ok(TileResp::NotModified(etag)) => {
            (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response()
        }
        Ok(TileResp::Full(tile, etag)) => {
            let body = match tile {
                Some(png) => png.as_ref().clone(),
                None => empty_tile_png().to_vec(),
            };
            (
                [
                    (header::CONTENT_TYPE, "image/png".to_string()),
                    (header::CACHE_CONTROL, "no-cache".to_string()),
                    (header::ETAG, etag),
                ],
                body,
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// An overlay tile with nothing to draw: this world has no folder for that
/// dimension, or no copy of that database.
///
/// That is data, not a failure. The merged ingest tree gains a dimension only
/// when someone uploads a *region* for it, while highlight rows for the same
/// dimension arrive on their own five-second sweep — so the rows routinely
/// land first. A per-player backup tree may never hold every dimension at all.
/// Answering those with 500 leaves the overlay dark and the log full; a hole
/// is what the rest of the map does with data it cannot read.
///
/// Stamped with the roots epoch, which is bumped by exactly the rescans that
/// could supply the missing dimension or database — so the browser revalidates
/// when that changes and never in between.
///
/// The ETag carries no world/dim/db identity on purpose: every empty tile at a
/// coordinate is the same transparent PNG, so one browser cache entry serves
/// all of them. `dir_hash = 0` is that shared namespace, and a tile with
/// something to draw cannot fall into it — its stamp is a file mtime in
/// epoch-milliseconds while this one is the roots epoch, a counter that starts
/// at 1 and only advances a step per rescan; the hash of a real database path
/// is a second, independent separator. If this ever returns anything but
/// `empty_tile_png()`, it has to hash its identity in the way the drawn path
/// does. `the_empty_overlay_etag_namespace_stays_separate` guards both halves.
fn empty_highlight_tile(st: &AppState, z: i32, x: i32, y: i32, inm: &Option<String>) -> TileResp {
    let etag = tile_etag(0, z, x, y, st.epoch.load(Ordering::Relaxed));
    if inm_matches(inm, &etag) {
        TileResp::NotModified(etag)
    } else {
        TileResp::Full(None, etag)
    }
}

/// This world's handle on one XaeroPlus database, with its freshness stamp.
///
/// `Ok(None)` is "this world has no copy of it" — see [`empty_highlight_tile`].
/// `Err` is kept for a database that is there and will not open, which is a
/// real fault worth reporting.
///
/// The caller passes the `WorldEntry` it already resolved rather than us taking
/// the worlds lock again: `w` is a *positional* index, so a second snapshot
/// could name a different world than the one whose identity goes into the tile's
/// ETag, and it costs a lock and an Arc clone per tile to get there.
fn get_db(
    st: &AppState,
    w: usize,
    we: &WorldEntry,
    db_name: &str,
) -> Result<Option<(SharedDb, u64)>, String> {
    if !we.world.databases.iter().any(|d| d == db_name) {
        return Ok(None);
    }
    let Some(wm) = we.world.world_map_path.as_ref() else {
        return Ok(None);
    };
    let path = wm.join(db_name);
    // Live XaeroPlus DBs are WAL-mode: commits touch the -wal file long before
    // the .db, so the freshness stamp must consider both.
    let mtime = config::mtime_ms(&path).max(config::mtime_ms(&wm.join(format!("{db_name}-wal"))));
    let key = (w, db_name.to_string());
    if let Some(db) = st.dbs.lock().unwrap().get(&key) {
        return Ok(Some((db.clone(), mtime)));
    }
    let db = Arc::new(Mutex::new(xaero_db::open_readonly(&path)?));
    st.dbs.lock().unwrap().insert(key, db.clone());
    Ok(Some((db, mtime)))
}

#[allow(clippy::too_many_arguments)]
fn highlight_tile_blocking(
    st: &AppState,
    w: usize,
    db_name: &str,
    d: usize,
    z: i32,
    x: i32,
    y: i32,
    tint: Option<u32>,
    inm: Option<String>,
) -> Result<TileResp, String> {
    let worlds = st.worlds.read().unwrap().clone();
    let Some(we) = worlds.get(w) else {
        return Ok(empty_highlight_tile(st, z, x, y, &inm));
    };
    let Some(dim_key) = we
        .world
        .dims
        .get(d)
        .and_then(|dim| dim.dimension.as_ref())
        .map(|dd| dd.resource_key())
    else {
        return Ok(empty_highlight_tile(st, z, x, y, &inm));
    };

    let Some((db, mtime)) = get_db(st, w, we, db_name)? else {
        return Ok(empty_highlight_tile(st, z, x, y, &inm));
    };
    // Resolve the colour before it reaches either cache, so an explicit
    // `c=` that happens to equal the default shares the default's entries
    // instead of doubling them.
    let color = tint
        .map(|c| [(c >> 16) as u8, (c >> 8) as u8, c as u8])
        .or_else(|| xaero_db::highlight_db_info(db_name).map(|i| i.color))
        .unwrap_or([0xAA, 0xAA, 0xAA]);
    let tint = ((color[0] as u32) << 16) | ((color[1] as u32) << 8) | color[2] as u32;
    // The db path hash keeps a positional world index from revalidating
    // another world's overlay after a roots change; the dim index is in the
    // hash input because the same db serves several dimensions, and the tint
    // because it is painted into the bytes.
    let dir_hash = we
        .world
        .world_map_path
        .as_ref()
        .map(|wm| {
            hash_path(
                &wm.join(db_name)
                    .join(d.to_string())
                    .join(format!("{tint:06x}")),
            )
        })
        .unwrap_or(0);
    let etag = tile_etag(dir_hash, z, x, y, mtime);
    if inm_matches(&inm, &etag) {
        return Ok(TileResp::NotModified(etag));
    }
    let key = TileKey {
        map: MapId {
            world: w,
            dim: d,
            mw: 0,
            cave: None,
            roof: None,
        },
        db: Some(db_name.to_string()),
        tint,
        z,
        x,
        y,
        stamp: mtime,
    };
    if let Some(hit) = st.tiles.lock().unwrap().get(&key) {
        return Ok(TileResp::Full(hit.clone(), etag));
    }

    // Tile spans 2^-z regions = 2^-z * 32 chunks per axis.
    let chunks_per_tile = (1i64 << (-z)) * 32;
    let cx0 = x as i64 * chunks_per_tile;
    let cz0 = y as i64 * chunks_per_tile;
    let grid = {
        let db = db.lock().unwrap();
        match db.table_for_dimension(&dim_key) {
            None => None,
            Some(table) => Some(db.tile_grid(
                &table,
                &xaero_db::TileQuery::new(db_name, cx0, cz0, chunks_per_tile, TILE),
            )?),
        }
    };
    // The old path pulled every matching row into memory: one world-zoom tile
    // over the real NewChunks DB materialised 14 million of them (~336 MB).
    // tile_grid aggregates per output cell inside SQLite instead, so a tile
    // costs the same no matter how many chunks it covers.
    let encoded = match grid {
        Some(g) if !g.is_empty() => {
            let mut rgba = vec![0u8; TILE * TILE * 4];
            let lava = xaero_db::LavaColumnStyle::default();
            // LavaColumns stores a column height in the `foundTime` column, not
            // a timestamp; drawing every row would paint 92% of the map that
            // the game deliberately hides.
            let by_height = xaero_db::highlight_semantics(db_name).prefers_max();
            for cz in 0..g.cells {
                for cx in 0..g.cells {
                    if g.count_at(cx, cz) == 0 {
                        continue;
                    }
                    let alpha = if by_height {
                        match g.value_at(cx, cz).and_then(|h| lava.alpha(h)) {
                            Some(a) => a,
                            None => continue,
                        }
                    } else {
                        HL_ALPHA
                    };
                    for dy in 0..g.cell_px {
                        for dx in 0..g.cell_px {
                            let (ax, ay) = (cx * g.cell_px + dx, cz * g.cell_px + dy);
                            if ax < TILE && ay < TILE {
                                let i = (ay * TILE + ax) * 4;
                                rgba[i] = color[0];
                                rgba[i + 1] = color[1];
                                rgba[i + 2] = color[2];
                                rgba[i + 3] = alpha;
                            }
                        }
                    }
                }
            }
            Some(Arc::new(encode_png(&rgba)?))
        }
        _ => None,
    };
    let size = encoded.as_ref().map(|p| p.len()).unwrap_or(0) + 64;
    st.tiles.lock().unwrap().put(key, encoded.clone(), size);
    Ok(TileResp::Full(encoded, etag))
}

/// Locks a std mutex, taking the data back from a poisoned one: a panic in
/// some earlier holder must not turn every later request into a 500.
pub(crate) fn lock_ok<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn encode_png(rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(64 * 1024);
    {
        let mut enc = png::Encoder::new(&mut out, TILE as u32, TILE as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut w = enc.write_header().map_err(|e| e.to_string())?;
        w.write_image_data(rgba).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_overlay_tint_overrides() {
        assert_eq!(parse_tint(Some("ff3b30")), Some(0xff_3b_30));
        assert_eq!(parse_tint(Some("#ff3b30")), Some(0xff_3b_30));
        assert_eq!(parse_tint(Some(" #FF3B30 ")), Some(0xff_3b_30));
        assert_eq!(parse_tint(Some("000000")), Some(0));
        // Anything that is not exactly six hex digits falls back to the
        // palette colour rather than costing the viewer its overlay.
        assert_eq!(parse_tint(None), None);
        assert_eq!(parse_tint(Some("")), None);
        assert_eq!(parse_tint(Some("fff")), None);
        assert_eq!(parse_tint(Some("ff3b30a")), None);
        assert_eq!(parse_tint(Some("gg3b30")), None);
        assert_eq!(parse_tint(Some("#")), None);
    }

    #[test]
    fn the_empty_overlay_etag_namespace_stays_separate() {
        // `empty_highlight_tile` hashes no world, dimension or database into
        // its ETag: every empty tile at a coordinate is the same transparent
        // PNG, so they share one browser cache entry on purpose. Two fields
        // keep a tile that has something to draw out of that namespace, and
        // each is checked here on its own, because either alone is enough.
        let (z, x, y) = (-4, 1, 2);
        let stamp = 1_750_000_000_000;
        // 1. dir_hash. Empty tiles use 0; a drawn tile hashes the db path.
        for info in xaero_db::HL_PALETTE {
            let path = Path::new("world-map/Multiplayer_2b2t")
                .join(format!("XaeroPlus{}.db", info.pattern))
                .join("0")
                .join("ff3b30");
            let dir_hash = hash_path(&path);
            assert_ne!(dir_hash, 0, "{} lands in the empty namespace", info.label);
            assert_ne!(
                tile_etag(0, z, x, y, stamp),
                tile_etag(dir_hash, z, x, y, stamp),
                "{} shares an ETag with an empty tile of the same stamp",
                info.label
            );
        }
        // 2. The stamp. An empty tile carries the roots epoch, which starts at
        // 1 and takes one step per rescan; a drawn tile carries a file mtime
        // in epoch-milliseconds. Reaching a real mtime would take ~1.7e12
        // rescans, and the other value an mtime can take — 0, for a file that
        // vanished under a cached handle — is below the epoch's floor.
        assert_ne!(tile_etag(0, z, x, y, 1), tile_etag(0, z, x, y, stamp));
        assert_ne!(tile_etag(0, z, x, y, 1), tile_etag(0, z, x, y, 0));
    }

    #[test]
    fn the_palette_the_viewer_gets_is_the_one_it_can_colour_and_sync() {
        let palette = hl_palette_json();
        assert_eq!(palette.len(), xaero_db::HL_PALETTE.len());
        for entry in &palette {
            // The viewer feeds `color` straight back as `?c=`, which
            // `parse_tint` reads — so the two have to agree on the format.
            assert!(parse_tint(Some(&entry.color)).is_some(), "{}", entry.color);
            // A client pages its finds by a watermark over `foundTime`, so a
            // module is syncable exactly when that column is a timestamp. The
            // two `highlights` tests hold SYNCABLE to that from both ends;
            // this is the flag the viewer actually reads.
            assert_eq!(
                entry.syncable, entry.is_timestamp,
                "{} advertises the wrong sync state",
                entry.label
            );
        }
        // LavaColumns is the one height-valued module, so it is the one the
        // viewer must not offer to sync.
        assert!(palette
            .iter()
            .any(|e| e.pattern == "LavaColumns" && !e.syncable && !e.is_timestamp));
        assert_eq!(
            palette.iter().filter(|e| e.syncable).count(),
            palette.len() - 1
        );
    }
}
