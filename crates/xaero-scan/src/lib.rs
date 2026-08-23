//! xaero-scan — discovery of Xaero save roots on disk and cheap
//! filename-only region indexing. Native-only (filesystem) counterpart to the
//! WASM-clean xaero-core.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use xaero_core::dimconfig::{parse_dimension_config, parse_minimap_config, DimensionConfig};
use xaero_core::naming::{
    is_minimap_backup_dir_name, is_multiworld_folder, parse_backup_dir_name, parse_region_filename,
    parse_sync_conflict_filename, parse_waypoint_filename, Dimension,
};

#[derive(Debug, Clone)]
pub struct World {
    /// Folder name, e.g. "Multiplayer_2b2t".
    pub id: String,
    pub world_map_path: Option<PathBuf>,
    pub minimap_path: Option<PathBuf>,
    pub dims: Vec<DimEntry>,
    /// XaeroPlus SQLite files present at the world root (filenames).
    pub databases: Vec<String>,
    /// Minimap waypoint files: (dim folder like "dim%0", file path).
    pub waypoint_files: Vec<(String, PathBuf)>,
}

#[derive(Debug, Clone)]
pub struct DimEntry {
    /// Folder name: "null", "DIM-1", "DIM1", "DIM0", "minecraft$..".
    pub folder: String,
    pub dimension: Option<Dimension>,
    pub config: DimensionConfig,
    pub multiworlds: Vec<MwEntry>,
}

impl DimEntry {
    /// The vanilla dimension this behaves like (drives nether 1:8 pairing and
    /// default light mode). Falls back to the folder-derived dimension.
    pub fn dimension_type(&self) -> Option<Dimension> {
        match self.config.dimension_type_id.as_deref() {
            Some("minecraft:overworld") => Some(Dimension::Overworld),
            Some("minecraft:the_nether") => Some(Dimension::Nether),
            Some("minecraft:the_end") => Some(Dimension::End),
            _ => self.dimension.clone(),
        }
    }

    /// The dimension's own resource id ("minecraft:worlds/2b2t/2b2t_1"), as
    /// opposed to `dimension_type()`, which is the vanilla dimension it merely
    /// behaves like. Every custom dimension of a server reports the same
    /// `dimensionTypeId`, so this is the only field that tells them apart.
    pub fn dimension_id(&self) -> Option<String> {
        self.dimension.as_ref().map(|d| d.resource_key())
    }

    /// Short label for a dimension picker: the vanilla name, or the last
    /// segment of a custom dimension's resource id.
    pub fn label(&self) -> String {
        match (&self.dimension, self.dimension_type()) {
            (Some(d), _) => d.display_name(),
            (None, Some(t)) => t.display_name(),
            (None, None) => self.folder.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MwEntry {
    /// Folder name: "mw$default", "mw$-542221765".
    pub id: String,
    /// Display name from dimension_config MWName, else the id.
    pub display: String,
    /// Cave layer indices present under caves/<n>/.
    pub cave_layers: Vec<i32>,
}

/// Interprets `path` as one of: a dir containing `xaero/` or `config/xaero/`,
/// a dir containing `world-map/`, a `world-map` dir itself, or a single world
/// folder. The minimap tree is located separately and joined onto whichever
/// world-map candidate wins, so a world the player only ever had the minimap
/// for (waypoints, no map data) still shows up — with `world_map_path: None`.
/// Returns discovered worlds (possibly empty).
pub fn discover_root(path: &Path) -> Vec<World> {
    // The minimap and the world map are separate mods with separate configs, so
    // the two trees do not have to sit beside each other — probe for the
    // minimap one independently of which world-map candidate wins.
    let mm_root = [
        path.join("xaero").join("minimap"),
        path.join("config").join("xaero").join("minimap"),
        path.join("minimap"),
        // `--root <...>/world-map`: the minimap tree is its sibling.
        path.parent().unwrap_or(path).join("minimap"),
    ]
    .into_iter()
    .find(|p| p.is_dir());
    let candidates = [
        path.join("xaero").join("world-map"),
        path.join("config").join("xaero").join("world-map"),
        path.join("world-map"),
        path.to_path_buf(),
    ];
    // Only map data settles which candidate wins — a minimap tree on its own
    // must not stop the search, or an instance that splits the two mods would
    // lose its map.
    let mut worlds = Vec::new();
    for wm_root in candidates {
        if !wm_root.is_dir() {
            continue;
        }
        worlds = scan_world_map_root(&wm_root, mm_root.as_deref());
        // A single world folder given directly?
        if worlds.is_empty() && looks_like_world_dir(&wm_root) {
            worlds.extend(scan_world(&wm_root, mm_root.as_deref()));
        }
        if !worlds.is_empty() {
            break;
        }
    }
    if let Some(mm) = &mm_root {
        append_minimap_only_worlds(mm, &mut worlds);
    }
    worlds.sort_by(|a, b| a.id.cmp(&b.id));
    worlds
}

fn scan_world_map_root(wm_root: &Path, mm_root: Option<&Path>) -> Vec<World> {
    let Ok(entries) = std::fs::read_dir(wm_root) else {
        return Vec::new();
    };
    let mut worlds: Vec<World> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let p = e.path();
            looks_like_world_dir(&p)
                .then(|| scan_world(&p, mm_root))
                .flatten()
        })
        .collect();
    worlds.sort_by(|a, b| a.id.cmp(&b.id));
    worlds
}

/// Adds a `World` for every minimap world folder that has no world-map
/// counterpart. The mod's own `backup*` snapshots are skipped exactly as
/// `MinimapWorldManagerIO.loadAllWorlds` skips them: they hold waypoints the
/// player may since have deleted.
fn append_minimap_only_worlds(mm_root: &Path, worlds: &mut Vec<World>) {
    let Ok(entries) = std::fs::read_dir(mm_root) else {
        return;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let id = e.file_name().to_string_lossy().to_string();
        if is_minimap_backup_dir_name(&id) || id == "temp_to_add" {
            continue;
        }
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if worlds.iter().any(|w| w.id == id) {
            continue;
        }
        let waypoint_files = collect_waypoint_files(&e.path());
        if waypoint_files.is_empty() {
            continue;
        }
        worlds.push(World {
            id,
            world_map_path: None,
            minimap_path: Some(e.path()),
            dims: Vec::new(),
            databases: Vec::new(),
            waypoint_files,
        });
    }
}

/// Every waypoint file under one minimap world folder, as (dimension folder,
/// path). Mirrors `MinimapWorldManagerIO.loadWorldFolder`: any subdirectory is
/// a dimension folder, any `<mwId>_<name>.txt` (or the legacy `waypoints.txt`)
/// inside it is a waypoint file. The pre-dimension-folder layout — the same
/// files sitting directly in the world folder — is reachable through
/// [`scan_waypoint_files`], which does not have to name a dimension for them.
fn collect_waypoint_files(mm_world_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(mm_world_dir) else {
        return out;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let dname = e.file_name().to_string_lossy().to_string();
        if is_minimap_backup_dir_name(&dname) || dname == "temp_to_add" {
            continue;
        }
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(e.path()) else {
            continue;
        };
        for f in files.filter_map(|f| f.ok()) {
            let fname = f.file_name().to_string_lossy().to_string();
            if parse_waypoint_filename(&fname).is_some() {
                out.push((dname.clone(), f.path()));
            }
        }
    }
    out.sort();
    out
}

fn looks_like_world_dir(p: &Path) -> bool {
    if p.join("server_config.txt").is_file() {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(p) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let Ok(ft) = e.file_type() else { return false };
        if ft.is_dir() {
            return Dimension::from_worldmap_folder(&name).is_some();
        }
        // A world can hold nothing but XaeroPlus highlight databases: that is
        // how a shared server's world starts when a client sends chunk finds
        // before it has uploaded a region. `scan_world` already handles the
        // shape — without this the folder never reaches it.
        ft.is_file() && name.starts_with("XaeroPlus") && name.ends_with(".db")
    })
}

fn scan_world(world_dir: &Path, mm_root: Option<&Path>) -> Option<World> {
    let id = world_dir.file_name()?.to_string_lossy().to_string();

    // Companion minimap data lives at <...>/minimap/<worldId>/ next to
    // <...>/world-map/<worldId>/. Its config.txt is the only place the game
    // records the dimension type of a custom dimension whose world-map folder
    // never got a dimension_config.txt.
    let minimap_path = mm_root
        .map(|mm| mm.join(&id))
        .or_else(|| {
            world_dir
                .parent()
                .and_then(|wm| wm.parent())
                .map(|x| x.join("minimap").join(&id))
        })
        .filter(|p| p.is_dir());
    let minimap_config = minimap_path.as_ref().and_then(|mm| {
        std::fs::read_to_string(mm.join("config.txt"))
            .ok()
            .map(|t| parse_minimap_config(&t))
    });

    let mut dims = Vec::new();
    let mut databases = Vec::new();
    for entry in std::fs::read_dir(world_dir).ok()?.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let dimension = Dimension::from_worldmap_folder(&name);
            let has_config = path.join("dimension_config.txt").is_file();
            if dimension.is_none() && !has_config {
                continue;
            }
            let mut config = std::fs::read_to_string(path.join("dimension_config.txt"))
                .map(|t| parse_dimension_config(&t))
                .unwrap_or_default();
            if config.dimension_type_id.is_none() {
                if let (Some(mc), Some(d)) = (&minimap_config, &dimension) {
                    config.dimension_type_id =
                        mc.dimension_type_of(&d.resource_key()).map(String::from);
                }
            }
            let multiworlds = scan_multiworlds(&path, &config);
            if multiworlds.is_empty() && dimension.is_none() {
                continue;
            }
            dims.push(DimEntry {
                folder: name,
                dimension,
                config,
                multiworlds,
            });
        } else if ft.is_file() && name.ends_with(".db") {
            databases.push(name);
        }
    }
    if dims.is_empty() && databases.is_empty() {
        return None;
    }
    dims.sort_by_key(|a| dim_sort_key(&a.folder));
    databases.sort();

    let waypoint_files = minimap_path
        .as_deref()
        .map(collect_waypoint_files)
        .unwrap_or_default();

    Some(World {
        id,
        world_map_path: Some(world_dir.to_path_buf()),
        minimap_path,
        dims,
        databases,
        waypoint_files,
    })
}

fn dim_sort_key(folder: &str) -> (u8, String) {
    match folder {
        "null" | "DIM0" => (0, String::new()),
        "DIM-1" => (1, String::new()),
        "DIM1" => (2, String::new()),
        other => (3, other.to_string()),
    }
}

fn scan_multiworlds(dim_dir: &Path, config: &DimensionConfig) -> Vec<MwEntry> {
    let Ok(entries) = std::fs::read_dir(dim_dir) else {
        return Vec::new();
    };
    let mut mws: Vec<MwEntry> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let id = e.file_name().to_string_lossy().to_string();
            if !is_multiworld_folder(&id) {
                return None;
            }
            let mut cave_layers: Vec<i32> = std::fs::read_dir(e.path().join("caves"))
                .map(|caves| {
                    caves
                        .filter_map(|c| c.ok())
                        .filter(|c| c.file_type().map(|t| t.is_dir()).unwrap_or(false))
                        .filter_map(|c| c.file_name().to_string_lossy().parse::<i32>().ok())
                        .collect()
                })
                .unwrap_or_default();
            cave_layers.sort();
            let display = config.multiworld_display_name(&id).to_string();
            Some(MwEntry {
                id,
                display,
                cave_layers,
            })
        })
        .collect();
    mws.sort_by(|a, b| a.id.cmp(&b.id));
    mws
}

// ------------------------------------------------------------------ index --

#[derive(Debug, Clone, Copy)]
pub struct RegionMeta {
    /// Modification time, unix milliseconds (0 when unavailable).
    pub mtime_ms: u64,
    pub size: u64,
    pub is_zip: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RegionIndex {
    pub dir: PathBuf,
    pub entries: HashMap<(i32, i32), RegionMeta>,
}

impl RegionIndex {
    pub fn region_path(&self, rx: i32, rz: i32) -> Option<PathBuf> {
        let meta = self.entries.get(&(rx, rz))?;
        let ext = if meta.is_zip { "zip" } else { "xaero" };
        Some(self.dir.join(format!("{rx}_{rz}.{ext}")))
    }

    /// Inclusive (min_x, min_z, max_x, max_z) over present regions.
    pub fn bounds(&self) -> Option<(i32, i32, i32, i32)> {
        let mut it = self.entries.keys();
        let &(x0, z0) = it.next()?;
        Some(it.fold((x0, z0, x0, z0), |(ax, az, bx, bz), &(x, z)| {
            (ax.min(x), az.min(z), bx.max(x), bz.max(z))
        }))
    }
}

/// One readdir pass over a map directory (a `mw$*` folder or a
/// `mw$*/caves/<n>` folder). Skips cache dirs and temp/outdated files.
pub fn index_regions(map_dir: &Path) -> std::io::Result<RegionIndex> {
    let mut index = RegionIndex {
        dir: map_dir.to_path_buf(),
        entries: HashMap::new(),
    };
    for entry in std::fs::read_dir(map_dir)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            // `cache*`, `caves`, and `<version>_backup_<n>` — the last of which
            // holds pre-conversion copies that shadow live coordinates and so
            // must stay out of the live index. See `scan_region_alternates`.
            continue;
        }
        let Some((rx, rz, is_zip)) = parse_region_filename(&name) else {
            continue;
        };
        // Prefer .zip when both containers exist for the same coords.
        let Some(meta) = region_meta(&entry, is_zip) else {
            continue;
        };
        index
            .entries
            .entry((rx, rz))
            .and_modify(|m| {
                if is_zip {
                    *m = meta;
                }
            })
            .or_insert(meta);
    }
    Ok(index)
}

// -------------------------------------------------------- historical data --

/// Why a region file is not part of the live layer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlternateSource {
    /// `<layer>/<version>_backup_<index>/`: the copy Xaero moved aside before
    /// rewriting the region at a newer format version. `version` is the raw
    /// i32 header value (major = `>> 16`, minor = `& 0xFFFF`).
    VersionBackup { version: i32, index: u32 },
    /// `<rx>_<rz>.sync-conflict-<tag>.<ext>`: a Syncthing conflict copy left
    /// beside the live file.
    SyncConflict { tag: String },
}

/// One region file that exists in a layer but is invisible to the live index.
#[derive(Debug, Clone)]
pub struct RegionAlternate {
    pub rx: i32,
    pub rz: i32,
    pub source: AlternateSource,
    pub path: PathBuf,
    pub meta: RegionMeta,
    /// False when the live layer has no file at these coordinates, i.e. this
    /// is the only surviving copy of the region.
    pub live: bool,
}

/// Every alternate copy of a region held by one map layer: the mod's own
/// `<version>_backup_<n>` snapshots (`MapSaveLoad.backupFile` *moves* the live
/// file aside, so these can be the last copy of a region) and Syncthing
/// conflict copies.
///
/// This is an opt-in historical view. Nothing here belongs in the live map —
/// backups shadow live coordinates at an older format version — so callers
/// should surface it as a separate layer or a recovery report, and prefer the
/// live copy on any tie. Ordered by (rz, rx, source).
pub fn scan_region_alternates(map_dir: &Path) -> std::io::Result<Vec<RegionAlternate>> {
    let mut live: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();
    let mut backup_dirs: Vec<(i32, u32, PathBuf)> = Vec::new();
    let mut out = Vec::new();
    // One pass over the layer, which can hold a million entries: live
    // coordinates, backup dirs and conflict copies all come out of it.
    for entry in std::fs::read_dir(map_dir)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if let Some((version, index)) = parse_backup_dir_name(&name) {
                backup_dirs.push((version, index, entry.path()));
            }
            continue;
        }
        if let Some((rx, rz, _)) = parse_region_filename(&name) {
            live.insert((rx, rz));
        } else if let Some((rx, rz, is_zip, tag)) = parse_sync_conflict_filename(&name) {
            let Some(meta) = region_meta(&entry, is_zip) else {
                continue;
            };
            out.push(RegionAlternate {
                rx,
                rz,
                source: AlternateSource::SyncConflict {
                    tag: tag.to_string(),
                },
                path: entry.path(),
                meta,
                live: false,
            });
        }
    }
    for (version, index, dir) in backup_dirs {
        let Ok(inner) = std::fs::read_dir(&dir) else {
            continue;
        };
        for f in inner.filter_map(|f| f.ok()) {
            let fname = f.file_name().to_string_lossy().to_string();
            let Some((rx, rz, is_zip)) = parse_region_filename(&fname) else {
                continue;
            };
            let Some(meta) = region_meta(&f, is_zip) else {
                continue;
            };
            out.push(RegionAlternate {
                rx,
                rz,
                source: AlternateSource::VersionBackup { version, index },
                path: f.path(),
                meta,
                live: false,
            });
        }
    }
    for a in &mut out {
        a.live = live.contains(&(a.rx, a.rz));
    }
    out.sort_by(|a, b| (a.rz, a.rx, &a.source, &a.path).cmp(&(b.rz, b.rx, &b.source, &b.path)));
    Ok(out)
}

fn region_meta(entry: &std::fs::DirEntry, is_zip: bool) -> Option<RegionMeta> {
    let md = entry.metadata().ok()?;
    let mtime_ms = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some(RegionMeta {
        mtime_ms,
        size: md.len(),
        is_zip,
    })
}

/// Resolves the on-disk directory for a map layer.
pub fn layer_dir(
    world_map_path: &Path,
    dim_folder: &str,
    mw: &str,
    cave_layer: Option<i32>,
) -> PathBuf {
    let base = world_map_path.join(dim_folder).join(mw);
    match cave_layer {
        None => base,
        Some(n) => base.join("caves").join(n.to_string()),
    }
}

/// Default platform locations worth offering at first run.
pub fn default_root_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    if let Some(home) = home {
        let home = PathBuf::from(home);
        out.push(home.join(".minecraft"));
        out.push(home.join("AppData/Roaming/.minecraft"));
        out.push(home.join("Library/Application Support/minecraft"));
        // Instance folders of the launchers people actually use. Depending on
        // the launcher, the game dir is the instance dir itself or a
        // `.minecraft`/`minecraft` inside it — push all three shapes and let
        // the xaero-dir filter below keep whichever is real.
        for base in [
            // Prism / MultiMC
            home.join(".local/share/PrismLauncher/instances"),
            home.join(".var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher/instances"),
            home.join("AppData/Roaming/PrismLauncher/instances"),
            home.join("Library/Application Support/PrismLauncher/instances"),
            home.join(".local/share/multimc/instances"),
            // CurseForge (Windows default, and the Documents fallback it and
            // macOS use)
            home.join("curseforge/minecraft/Instances"),
            home.join("Documents/curseforge/minecraft/Instances"),
            // Modrinth App (both its folder names, per platform)
            home.join("AppData/Roaming/ModrinthApp/profiles"),
            home.join("AppData/Roaming/com.modrinth.theseus/profiles"),
            home.join(".local/share/ModrinthApp/profiles"),
            home.join(".local/share/com.modrinth.theseus/profiles"),
            home.join("Library/Application Support/ModrinthApp/profiles"),
            home.join("Library/Application Support/com.modrinth.theseus/profiles"),
            // ATLauncher / GDLauncher
            home.join("AppData/Roaming/ATLauncher/instances"),
            home.join("AppData/Roaming/gdlauncher_next/instances"),
        ] {
            if let Ok(instances) = std::fs::read_dir(&base) {
                for inst in instances.filter_map(|e| e.ok()).take(64) {
                    out.push(inst.path());
                    out.push(inst.path().join(".minecraft"));
                    out.push(inst.path().join("minecraft"));
                }
            }
        }
    }
    // Some packs put the mod's data under config/xaero/ instead of xaero/.
    out.retain(|p| p.join("xaero").is_dir() || p.join("config").join("xaero").is_dir());
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

// ------------------------------------------------------------- waypoints --

/// Whether a waypoint file is one the game is still writing to, or a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaypointSourceKind {
    /// A live minimap tree: `<root>/xaero/minimap`, `<root>/config/xaero/minimap`.
    Live,
    /// A snapshot: the minimap's own `backup*` dirs (`SimpleBackup`) or a
    /// hand-made `XaeroWaypoints*` copy of a minimap tree. These hold
    /// waypoints the player may since have deleted, so they must never be
    /// folded into the live set — see the note on [`scan_waypoint_files`].
    Archived,
}

/// One waypoint file found on disk.
#[derive(Debug, Clone)]
pub struct WaypointFileRef {
    pub kind: WaypointSourceKind,
    /// Minimap world folder name, e.g. "Multiplayer_2b2t".
    pub world: String,
    /// Dimension folder as written on disk ("dim%0", the legacy "Nether"), or
    /// `None` for the pre-dimension-folder layout, where waypoint files sit
    /// directly in the world folder and carry no dimension at all.
    pub dim_folder: Option<String>,
    pub dimension: Option<Dimension>,
    /// Multiworld id from the file name; `None` for a legacy `waypoints.txt`.
    pub mw: Option<String>,
    /// Root of the minimap-shaped tree this file was found in.
    pub tree: PathBuf,
    pub path: PathBuf,
}

/// Every waypoint file reachable from `root`, live and archived, independent of
/// world-map discovery — most of a long-lived instance's waypoints live in
/// worlds that never had map data, or in snapshot trees.
///
/// Callers that feed the vault **must** keep the two kinds apart: archived
/// files are the state of the world on the day of the snapshot, so importing
/// them as live resurrects waypoints the player deleted. The vault's identity
/// key is (world, dim, mw_file, name, x, y, z) — the bare file name, not the
/// path — so a snapshot collides with the live file it was copied from and
/// must be given its own source identity before it can be ingested at all.
pub fn scan_waypoint_files(root: &Path) -> Vec<WaypointFileRef> {
    let mut out = Vec::new();
    for tree in [
        root.join("xaero").join("minimap"),
        root.join("config").join("xaero").join("minimap"),
        root.join("minimap"),
    ] {
        if tree.is_dir() {
            scan_minimap_tree(&tree, WaypointSourceKind::Live, &mut out);
        }
    }
    // Hand-made copies of a whole minimap tree, e.g. XaeroWaypoints_BACKUP240807/.
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("XaeroWaypoints") && e.path().is_dir() {
                scan_minimap_tree(&e.path(), WaypointSourceKind::Archived, &mut out);
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    out
}

/// Walks one minimap-shaped tree: `<tree>/<world>/[<dim folder>/]<file>.txt`.
/// The `backup*` dirs the mod writes repeat the shape of whatever they sit in
/// and are always reported as [`WaypointSourceKind::Archived`].
fn scan_minimap_tree(tree: &Path, kind: WaypointSourceKind, out: &mut Vec<WaypointFileRef>) {
    let Ok(worlds) = std::fs::read_dir(tree) else {
        return;
    };
    for w in worlds.filter_map(|e| e.ok()) {
        let world = w.file_name().to_string_lossy().to_string();
        if !w.file_type().map(|t| t.is_dir()).unwrap_or(false) || world == "temp_to_add" {
            continue;
        }
        if is_minimap_backup_dir_name(&world) {
            // <tree>/backup/ holds a copy of the tree it sits in.
            scan_minimap_tree(&w.path(), WaypointSourceKind::Archived, out);
        } else {
            scan_minimap_world(&w.path(), &world, kind, tree, out);
        }
    }
}

fn scan_minimap_world(
    dir: &Path,
    world: &str,
    kind: WaypointSourceKind,
    tree: &Path,
    out: &mut Vec<WaypointFileRef>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if is_minimap_backup_dir_name(&name) {
                // <world>/backup/ holds a copy of the dimension folders.
                scan_minimap_world(&e.path(), world, WaypointSourceKind::Archived, tree, out);
                continue;
            }
            if name == "temp_to_add" {
                continue;
            }
            let Ok(files) = std::fs::read_dir(e.path()) else {
                continue;
            };
            for f in files.filter_map(|f| f.ok()) {
                let fname = f.file_name().to_string_lossy().to_string();
                let Some((mw, _)) = parse_waypoint_filename(&fname) else {
                    continue;
                };
                out.push(WaypointFileRef {
                    kind,
                    world: world.to_string(),
                    dim_folder: Some(name.clone()),
                    dimension: Dimension::from_minimap_folder(&name),
                    mw: mw.map(String::from),
                    tree: tree.to_path_buf(),
                    path: f.path(),
                });
            }
        } else if let Some((mw, _)) = parse_waypoint_filename(&name) {
            // Pre-dimension-folder layout, still loaded by the mod.
            out.push(WaypointFileRef {
                kind,
                world: world.to_string(),
                dim_folder: None,
                dimension: None,
                mw: mw.map(String::from),
                tree: tree.to_path_buf(),
                path: e.path(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus_root() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("XAERO_CORPUS") {
            let p = PathBuf::from(p);
            return p.is_dir().then_some(p);
        }
        let fallback = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../sample data");
        fallback.is_dir().then(|| fallback.canonicalize().unwrap())
    }

    #[test]
    fn discovers_sample_worlds() {
        let Some(root) = corpus_root() else { return };
        let worlds = discover_root(&root.join("xaero1.21.8"));
        assert!(worlds.iter().any(|w| w.id == "Multiplayer_2b2t"));
        let w2 = worlds.iter().find(|w| w.id == "Multiplayer_2b2t").unwrap();
        assert!(w2.dims.iter().any(|d| d.folder == "null"));
        assert!(w2.dims.iter().any(|d| d.folder == "DIM-1"));
        assert!(w2.dims.iter().any(|d| d.folder == "DIM1"));
        assert!(!w2.databases.is_empty());
        assert!(!w2.waypoint_files.is_empty(), "waypoints discovered");

        let worlds4 = discover_root(&root.join("xaero1.21.4"));
        let custom = worlds4
            .iter()
            .find(|w| w.id == "Multiplayer_Minecraft Server")
            .expect("custom-dim world");
        assert!(custom
            .dims
            .iter()
            .any(|d| d.folder.starts_with("minecraft$worlds%")));
        // caves discovered on the 2b2t.org world (1.21.8)
        let org = discover_root(&root.join("xaero1.21.8"))
            .into_iter()
            .find(|w| w.id == "Multiplayer_2b2t.org")
            .unwrap();
        let nether = org.dims.iter().find(|d| d.folder == "DIM-1").unwrap();
        assert!(!nether.multiworlds[0].cave_layers.is_empty());
    }

    #[test]
    fn discovers_minimap_only_worlds() {
        let Some(root) = corpus_root() else { return };
        let worlds = discover_root(&root.join("xaero1.21.4"));
        // Worlds the player only ever had the minimap for: waypoints, no map.
        let pvp = worlds
            .iter()
            .find(|w| w.id == "Multiplayer_pvp")
            .expect("minimap-only world");
        assert!(pvp.world_map_path.is_none());
        assert!(pvp.minimap_path.is_some());
        assert_eq!(pvp.waypoint_files.len(), 2);
        // A minimap folder holding nothing but config.txt is not a world.
        assert!(!worlds
            .iter()
            .any(|w| w.id == "Multiplayer_masonic" && w.world_map_path.is_none()));
        // The mod's own backup dir is never a world.
        assert!(!worlds.iter().any(|w| w.id == "backup"));
        // Custom dimensions stay distinguishable in the UI.
        let custom = worlds
            .iter()
            .find(|w| w.id == "Multiplayer_Minecraft Server")
            .unwrap();
        let d = custom
            .dims
            .iter()
            .find(|d| d.folder.starts_with("minecraft$worlds%"))
            .unwrap();
        assert_eq!(
            d.dimension_id().as_deref(),
            Some("minecraft:worlds/2b2t/2b2t_1")
        );
        assert_eq!(d.label(), "2b2t_1");
    }

    #[test]
    fn scans_every_waypoint_file_layout() {
        let Some(root) = corpus_root() else { return };
        let files = scan_waypoint_files(&root.join("xaero1.21.4"));
        assert!(files.iter().all(|f| f.kind == WaypointSourceKind::Live));
        // Every mw$*_<n>.txt under a dim% folder of the corpus.
        assert_eq!(files.len(), 9);
        let f = files
            .iter()
            .find(|f| f.dim_folder.as_deref() == Some("dim%minecraft$worlds%2b2t%2b2t_1"))
            .unwrap();
        assert_eq!(f.world, "Multiplayer_Minecraft Server");
        assert_eq!(f.mw.as_deref(), Some("mw$-542221765"));
        assert_eq!(
            f.dimension,
            Some(Dimension::Custom("minecraft:worlds/2b2t/2b2t_1".into()))
        );
    }

    #[test]
    fn pairs_a_split_minimap_and_world_map() {
        // Two mods, two configs: minimap under xaero/, world map under
        // config/xaero/. The map must still be found, with its waypoints.
        let root = std::env::temp_dir().join(format!("xt-split-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mm = root.join("xaero/minimap/Multiplayer_split/dim%0");
        let layer = root.join("config/xaero/world-map/Multiplayer_split/null/mw$default");
        std::fs::create_dir_all(&mm).unwrap();
        std::fs::create_dir_all(&layer).unwrap();
        std::fs::write(mm.join("mw$default_1.txt"), "").unwrap();
        std::fs::write(layer.join("0_0.zip"), "").unwrap();
        std::fs::write(
            layer.parent().unwrap().join("dimension_config.txt"),
            "dimensionTypeId:minecraft:overworld\n",
        )
        .unwrap();

        let worlds = discover_root(&root);
        assert_eq!(worlds.len(), 1);
        assert!(worlds[0].world_map_path.is_some(), "map data was dropped");
        assert_eq!(worlds[0].dims.len(), 1);
        assert_eq!(worlds[0].waypoint_files.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn classifies_layer_alternates() {
        let dir = std::env::temp_dir().join(format!("xt-alt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let backup = dir.join("458760_backup_3");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::create_dir_all(dir.join("cache_1")).unwrap();
        for (p, body) in [
            (dir.join("0_0.zip"), "live"),
            (dir.join("0_0.zip.temp.backup7"), "junk"),
            (
                dir.join("0_0.sync-conflict-20240705-215039-QQNGROR.zip"),
                "conflict",
            ),
            (backup.join("0_0.zip"), "shadowed"),
            (backup.join("-9_4.zip"), "only copy"),
        ] {
            std::fs::write(p, body).unwrap();
        }

        // The live index sees exactly one region and no longer trips on the
        // backup dir's name.
        let idx = index_regions(&dir).unwrap();
        assert_eq!(idx.entries.len(), 1);

        let alts = scan_region_alternates(&dir).unwrap();
        assert_eq!(alts.len(), 3);
        let only: Vec<_> = alts.iter().filter(|a| !a.live).collect();
        assert_eq!(only.len(), 1);
        assert_eq!((only[0].rx, only[0].rz), (-9, 4));
        assert_eq!(
            only[0].source,
            AlternateSource::VersionBackup {
                version: 458760,
                index: 3
            }
        );
        assert!(alts.iter().any(|a| a.live
            && a.source
                == AlternateSource::SyncConflict {
                    tag: "20240705-215039-QQNGROR".into()
                }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A shared server's world starts as highlight databases and nothing
    /// else — the chunk finds arrive before the first region upload.
    #[test]
    fn finds_a_world_that_is_only_highlight_databases() {
        let root = std::env::temp_dir().join(format!("xt-scan-dbonly-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let world = root.join("world-map").join("Multiplayer_2b2t");
        std::fs::create_dir_all(&world).unwrap();
        std::fs::write(world.join("XaeroPlusNewChunks.db"), b"").unwrap();
        // A stray database elsewhere must not turn its folder into a world.
        let other = root.join("world-map").join("notes");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("recipes.db"), b"").unwrap();

        let worlds = discover_root(&root);
        assert_eq!(worlds.len(), 1);
        assert_eq!(worlds[0].id, "Multiplayer_2b2t");
        assert!(worlds[0].dims.is_empty());
        assert_eq!(worlds[0].databases, vec!["XaeroPlusNewChunks.db"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn indexes_sample_regions() {
        let Some(root) = corpus_root() else { return };
        let dir = root.join("xaero1.21.8/world-map/Multiplayer_2b2t/DIM-1/mw$default");
        let idx = index_regions(&dir).unwrap();
        assert_eq!(idx.entries.len(), 794);
        assert!(idx.region_path(0, 0).unwrap().ends_with("0_0.zip"));
        assert!(idx.bounds().is_some());
    }
}
