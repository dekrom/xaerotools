//! `xaerotools stats` — make an archive describe itself: regions, bytes,
//! bounds, mtime span and save-version mix per map layer, plus XaeroPlus
//! highlight-DB row counts and the waypoints found beside the map data.
//!
//! Region counts, bytes and mtimes come from one readdir+stat pass per layer.
//! The version mix and the explored-chunk figure need a decode, so they run
//! over a deterministic sample (`--sample N`, `--full` for every region).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::json;
use xaero_scan::{WaypointSourceKind, index_regions, scan_waypoint_files};

use crate::archive::{
    Layer, fmt_bytes, fmt_date, fmt_int, layers_of, matching_worlds, probe_all, sample_regions,
};

/// Regions decoded per layer when neither --sample nor --full is given.
const DEFAULT_SAMPLE: usize = 256;

struct LayerStats {
    layer: Layer,
    regions: u64,
    /// Regions that could carry data at all — `regions` minus zero-byte files.
    sampleable: u64,
    bytes: u64,
    bounds: Option<(i32, i32, i32, i32)>,
    first_ms: u64,
    last_ms: u64,
    /// Regions actually decoded for the version/chunk figures below.
    sampled: usize,
    versions: BTreeMap<String, usize>,
    truncated: usize,
    unreadable: usize,
    chunks_sampled: u64,
}

impl LayerStats {
    /// Explored Minecraft chunks, exact under --deep and extrapolated from the
    /// sample otherwise. None when no decode pass ran (`--sample 0`), which is
    /// "unknown", not "zero".
    fn chunks_estimate(&self) -> Option<u64> {
        (self.sampled > 0).then(|| {
            (self.chunks_sampled as f64 / self.sampled as f64 * self.sampleable as f64).round()
                as u64
        })
    }

    fn exact(&self) -> bool {
        self.sampled as u64 == self.sampleable
    }
}

struct DbStats {
    db: String,
    bytes: u64,
    schema: u32,
    tables: Vec<(String, u64)>,
    error: Option<String>,
}

/// Waypoints found for one world, split by whether the game still writes the
/// file they came from — an archived snapshot holds waypoints the player may
/// since have deleted, so the two must never be added together.
#[derive(Default)]
struct WaypointCount {
    files: usize,
    waypoints: usize,
    archived_files: usize,
    archived_waypoints: usize,
}

impl WaypointCount {
    fn add(&mut self, kind: WaypointSourceKind, n: usize) {
        match kind {
            WaypointSourceKind::Live => {
                self.files += 1;
                self.waypoints += n;
            }
            WaypointSourceKind::Archived => {
                self.archived_files += 1;
                self.archived_waypoints += n;
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.files == 0 && self.archived_files == 0
    }
}

pub fn stats_cmd(args: &[String]) {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut world: Option<String> = None;
    let mut json = false;
    let mut full = false;
    let mut no_dbs = false;
    let mut sample = DEFAULT_SAMPLE;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                roots.push(PathBuf::from(crate::value(args, &mut i, "--root")));
            }
            "--world" => {
                world = Some(crate::value(args, &mut i, "--world"));
            }
            "--sample" => {
                sample = crate::value(args, &mut i, "--sample")
                    .parse()
                    .unwrap_or_else(|_| {
                        eprintln!("--sample must be a number (0 skips the decode pass)");
                        std::process::exit(2);
                    });
            }
            "--full" => full = true,
            "--no-dbs" => no_dbs = true,
            "--json" => json = true,
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    if full {
        sample = 0; // 0 means "every region" to sample_regions
    }
    if roots.is_empty() {
        roots = xaero_scan::default_root_candidates();
    }
    let worlds = xaerotools_server::discover_worlds(&roots);
    if worlds.is_empty() {
        eprintln!("no Xaero data found — pass --root <path to .minecraft or xaero folder>");
        std::process::exit(1);
    }
    let selected = matching_worlds(&worlds, world.as_deref());
    if selected.is_empty() {
        eprintln!("no world matches --world {}", world.unwrap_or_default());
        std::process::exit(1);
    }
    // Waypoints live in the minimap tree, not the world map, so they come from
    // their own pass over the roots rather than from the layers below.
    let mut waypoints = scan_waypoints(&roots);

    let mut json_worlds = Vec::new();
    let mut all_regions = 0u64;
    let mut all_bytes = 0u64;
    let mut all_read = 0usize;
    let mut all_versions: BTreeMap<String, usize> = BTreeMap::new();
    let mut all_wp = WaypointCount::default();
    for w in &selected {
        let mut layer_stats = Vec::new();
        for layer in layers_of(w) {
            if progress_enabled() {
                eprint!("\r{:78}\rscanning {} {} …", "", w.world.id, layer.label());
            }
            if let Some(s) = scan_layer(&w.world.id, layer, sample, full) {
                all_regions += s.regions;
                all_bytes += s.bytes;
                all_read += s.sampled;
                for (v, n) in &s.versions {
                    *all_versions.entry(v.clone()).or_default() += n;
                }
                layer_stats.push(s);
            }
        }
        if progress_enabled() {
            eprint!("\r{:78}\r", "");
        }
        let dbs = if no_dbs { Vec::new() } else { scan_dbs(w) };
        let wp = waypoints.remove(&w.world.id).unwrap_or_default();
        all_wp.files += wp.files;
        all_wp.waypoints += wp.waypoints;
        all_wp.archived_files += wp.archived_files;
        all_wp.archived_waypoints += wp.archived_waypoints;
        if json {
            json_worlds.push(json!({
                "world": w.world.id,
                "root": w.root.display().to_string(),
                "caveLayers": layer_stats.iter().filter(|s| s.layer.cave.is_some()).count(),
                "layers": layer_stats.iter().map(layer_json).collect::<Vec<_>>(),
                "databases": dbs.iter().map(db_json).collect::<Vec<_>>(),
                "waypoints": waypoint_json(&wp),
            }));
        } else {
            print_world(&w.world.id, &w.root, &layer_stats, &dbs, &wp);
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "worlds": json_worlds,
                "totals": {
                    "regions": all_regions,
                    "bytes": all_bytes,
                    "versionHeadersRead": all_read,
                    "versions": all_versions,
                    "waypoints": waypoint_json(&all_wp),
                },
                "mode": if full { "full" } else { "sample" },
                "samplePerLayer": if full { None } else { Some(sample) },
            }))
            .unwrap()
        );
    } else {
        println!(
            "\ntotal: {} regions, {}, {} waypoints",
            fmt_int(all_regions),
            fmt_bytes(all_bytes),
            fmt_int(all_wp.waypoints as u64)
        );
        if all_read == 0 {
            println!("save versions: not read (--sample 0 skipped the decode pass)");
        } else {
            println!(
                "save versions, from {} of {} region headers ({}):",
                fmt_int(all_read as u64),
                fmt_int(all_regions),
                if full {
                    "full pass".to_string()
                } else {
                    format!("sampled up to {sample} per layer — --full reads every one")
                }
            );
            let mix: Vec<String> = all_versions
                .iter()
                .map(|(v, n)| format!("{v} {}", fmt_int(*n as u64)))
                .collect();
            println!("  {}", mix.join("   "));
        }
    }
}

/// True when stderr is a terminal, so `\r` progress redraws make sense.
/// Piped or redirected, they would turn one status line into thousands.
fn progress_enabled() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

/// Waypoints per world folder, from every minimap tree under the roots.
fn scan_waypoints(roots: &[PathBuf]) -> BTreeMap<String, WaypointCount> {
    let mut out: BTreeMap<String, WaypointCount> = BTreeMap::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for root in roots {
        for f in scan_waypoint_files(root) {
            // Overlapping roots (a .minecraft and its own xaero folder) would
            // otherwise count the same file twice.
            if !seen.insert(f.path.clone()) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&f.path) else {
                continue;
            };
            let n = xaero_core::waypoints::parse_waypoints_file(&text)
                .waypoints
                .len();
            out.entry(f.world.clone()).or_default().add(f.kind, n);
        }
    }
    out
}

fn scan_layer(world: &str, layer: Layer, sample: usize, full: bool) -> Option<LayerStats> {
    let index = index_regions(&layer.dir).ok()?;
    if index.entries.is_empty() {
        return None;
    }
    let mut bytes = 0u64;
    let mut first_ms = u64::MAX;
    let mut last_ms = 0u64;
    for m in index.entries.values() {
        bytes += m.size;
        if m.mtime_ms > 0 {
            first_ms = first_ms.min(m.mtime_ms);
            last_ms = last_ms.max(m.mtime_ms);
        }
    }
    let mut out = LayerStats {
        regions: index.entries.len() as u64,
        sampleable: index.entries.values().filter(|m| m.size > 0).count() as u64,
        bytes,
        bounds: index.bounds(),
        first_ms: if first_ms == u64::MAX { 0 } else { first_ms },
        last_ms,
        sampled: 0,
        versions: BTreeMap::new(),
        truncated: 0,
        unreadable: 0,
        chunks_sampled: 0,
        layer,
    };
    if sample > 0 || full {
        let label = format!("scanning {world} {}", out.layer.label());
        let paths = sample_regions(&index, sample);
        let probes = probe_all(&paths, &label);
        out.sampled = probes.len();
        for p in probes {
            match p.version {
                Some(v) => *out.versions.entry(v.to_string()).or_default() += 1,
                None => *out.versions.entry("?".to_string()).or_default() += 1,
            }
            if p.truncated {
                out.truncated += 1;
            }
            if p.error.is_some() {
                out.unreadable += 1;
            }
            out.chunks_sampled += p.chunks as u64;
        }
    }
    Some(out)
}

fn scan_dbs(w: &xaerotools_server::WorldEntry) -> Vec<DbStats> {
    let Some(wm) = &w.world.world_map_path else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in &w.world.databases {
        let path = wm.join(name);
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        match xaero_db::open_readonly(&path) {
            Err(e) => out.push(DbStats {
                db: name.clone(),
                bytes,
                schema: 0,
                tables: Vec::new(),
                error: Some(e),
            }),
            Ok(db) => {
                let mut tables = Vec::new();
                for t in &db.tables {
                    if !db.is_highlight_table(t) {
                        continue;
                    }
                    // WITHOUT ROWID + PRIMARY KEY(x,z): COUNT(*) is an index
                    // scan, sub-second even on the 87M-row tables.
                    if let Ok(n) = db.count(t) {
                        tables.push((t.clone(), n));
                    }
                }
                out.push(DbStats {
                    db: name.clone(),
                    bytes,
                    schema: db.version,
                    tables,
                    error: None,
                });
            }
        }
    }
    out
}

fn print_world(
    id: &str,
    root: &std::path::Path,
    layers: &[LayerStats],
    dbs: &[DbStats],
    wp: &WaypointCount,
) {
    println!("\n{id}  ({})", root.display());
    if layers.is_empty() {
        println!("  (no regions)");
    } else {
        let dims: BTreeSet<&str> = layers.iter().map(|s| s.layer.dim.as_str()).collect();
        let caves = layers.iter().filter(|s| s.layer.cave.is_some()).count();
        println!(
            "  {} dimension(s), {} layer(s) of which {caves} cave",
            dims.len(),
            layers.len()
        );
        println!(
            "  {:<34} {:>11} {:>10}  {:<10} {:<10}",
            "layer", "regions", "bytes", "first", "last"
        );
    }
    for s in layers {
        println!(
            "  {:<34} {:>11} {:>10}  {:<10} {:<10}",
            s.layer.label(),
            fmt_int(s.regions),
            fmt_bytes(s.bytes),
            fmt_date(s.first_ms),
            fmt_date(s.last_ms)
        );
        if let Some((x0, z0, x1, z1)) = s.bounds {
            println!(
                "      blocks {}..{} x {}..{}",
                x0 as i64 * 512,
                (x1 as i64 + 1) * 512 - 1,
                z0 as i64 * 512,
                (z1 as i64 + 1) * 512 - 1
            );
        }
        if s.sampled > 0 {
            let mix: Vec<String> = s
                .versions
                .iter()
                .map(|(v, n)| format!("{v} {:.0}%", 100.0 * *n as f64 / s.sampled as f64))
                .collect();
            println!(
                "      versions ({} {}): {}",
                if s.exact() { "all" } else { "sampled" },
                fmt_int(s.sampled as u64),
                mix.join("  ")
            );
            println!(
                "      chunks explored: {}{}{}",
                if s.exact() { "" } else { "~" },
                fmt_int(s.chunks_estimate().unwrap_or(0)),
                match (s.truncated, s.unreadable) {
                    (0, 0) => String::new(),
                    (t, u) => format!("   truncated {t}, unreadable {u} (of the sample)"),
                }
            );
        }
    }
    if !dbs.is_empty() {
        println!("  XaeroPlus databases");
    }
    for d in dbs {
        match &d.error {
            Some(e) => println!("    {:<38} {:>10}  ERROR {e}", d.db, fmt_bytes(d.bytes)),
            None => {
                let rows: u64 = d.tables.iter().map(|(_, n)| n).sum();
                println!(
                    "    {:<38} {:>10}  v{}  {} rows",
                    d.db,
                    fmt_bytes(d.bytes),
                    d.schema,
                    fmt_int(rows)
                );
                for (t, n) in &d.tables {
                    println!("        {:<34} {:>14}", t, fmt_int(*n));
                }
            }
        }
    }
    if !wp.is_empty() {
        println!(
            "  waypoints: {} in {} file(s){}",
            fmt_int(wp.waypoints as u64),
            wp.files,
            match wp.archived_files {
                0 => String::new(),
                n => format!(
                    ", plus {} in {n} archived file(s)",
                    fmt_int(wp.archived_waypoints as u64)
                ),
            }
        );
    }
}

fn layer_json(s: &LayerStats) -> serde_json::Value {
    json!({
        "dim": s.layer.dim,
        "mw": s.layer.mw,
        "layer": s.layer.layer_name(),
        "path": s.layer.dir.display().to_string(),
        "regions": s.regions,
        "emptyFiles": s.regions - s.sampleable,
        "bytes": s.bytes,
        "bounds": s.bounds.map(|(x0, z0, x1, z1)| json!([x0, z0, x1, z1])),
        "mtimeMs": { "first": s.first_ms, "last": s.last_ms },
        "sampled": s.sampled,
        "sampleIsEverything": s.exact(),
        "versions": s.versions,
        "truncatedInSample": s.truncated,
        "unreadableInSample": s.unreadable,
        "chunksExplored": s.chunks_estimate(),
    })
}

fn waypoint_json(w: &WaypointCount) -> serde_json::Value {
    json!({
        "files": w.files,
        "waypoints": w.waypoints,
        "archivedFiles": w.archived_files,
        "archivedWaypoints": w.archived_waypoints,
    })
}

fn db_json(d: &DbStats) -> serde_json::Value {
    json!({
        "db": d.db,
        "bytes": d.bytes,
        "schema": d.schema,
        "error": d.error,
        "tables": d.tables.iter()
            .map(|(t, n)| json!({ "table": t, "rows": n }))
            .collect::<Vec<_>>(),
    })
}
