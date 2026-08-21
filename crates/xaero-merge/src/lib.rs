//! xaero-merge — merging two Xaero map-data roots.
//!
//! Semantics (per plan):
//! - Unit of merge = (world, dimension, multiworld, cave-layer).
//! - Non-conflicting region files are copied byte-for-byte, mtime preserved.
//! - Same-coordinate conflicts are merged at TILE granularity: every tile of
//!   the preferred source (newer mtime by default) wins; tiles it lacks come
//!   from the other file. Output is re-encoded as 7.8 with fresh palettes and
//!   gets mtime = max(A, B).
//! - Derived caches (`cache*/`, `.xwmc`, `.outdated`, `.temp`) are never
//!   copied.
//! - XaeroPlus highlight DBs are merged oldest-foundTime-wins (xaero-db).
//! - Waypoint files are unioned record-wise; dimension_config MWName lines are
//!   unioned; other aux files: newer file wins.
//! - Sources are NEVER modified. Nothing is written unless `apply`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rayon::prelude::*;
use serde::Serialize;
use xaero_core::naming::{is_cache_dir_name, Dimension};
use xaero_core::waypoints::{format_waypoint_line, parse_waypoints_file};
use xaero_scan::{discover_root, index_regions, layer_dir, RegionIndex, World};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Prefer {
    Mtime,
    A,
    B,
}

#[derive(Debug, Clone)]
pub struct MergeOptions {
    pub apply: bool,
    pub prefer: Prefer,
    /// Only merge worlds whose id matches one of these (empty = all).
    pub servers: Vec<String>,
    /// Explicit world-id pairings "A-id=B-id".
    pub aliases: Vec<(String, String)>,
    /// Accept the built-in base-domain alias heuristic without confirmation.
    pub auto_alias: bool,
}

impl Default for MergeOptions {
    fn default() -> Self {
        MergeOptions {
            apply: false,
            prefer: Prefer::Mtime,
            servers: Vec::new(),
            aliases: Vec::new(),
            auto_alias: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UnitReport {
    pub world: String,
    pub dim: String,
    pub mw: String,
    pub cave: Option<i32>,
    pub only_a: usize,
    pub only_b: usize,
    pub conflicts: usize,
    pub merge_errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MergeReport {
    pub applied: bool,
    pub world_pairs: Vec<(String, String)>,
    pub only_worlds: Vec<String>,
    pub units: Vec<UnitReport>,
    pub aux_copied: usize,
    pub waypoint_files_merged: usize,
    pub dbs: Vec<xaero_db::merge::DbMergeReport>,
    pub suggested_aliases: Vec<(String, String)>,
}

impl MergeReport {
    pub fn total_regions_out(&self) -> usize {
        self.units
            .iter()
            .map(|u| u.only_a + u.only_b + u.conflicts)
            .sum()
    }
}

/// Merge roots A and B into a fresh output directory.
/// The output world id and folder conventions follow B (the "base" side).
pub fn merge_to_output(
    a_root: &Path,
    b_root: &Path,
    out: &Path,
    opts: &MergeOptions,
) -> Result<MergeReport, String> {
    let mut report = MergeReport {
        applied: opts.apply,
        ..Default::default()
    };

    let worlds_a = discover_root(a_root);
    let worlds_b = discover_root(b_root);
    if worlds_a.is_empty() {
        return Err(format!("no Xaero worlds found under {}", a_root.display()));
    }
    if worlds_b.is_empty() {
        return Err(format!("no Xaero worlds found under {}", b_root.display()));
    }

    let keep = |id: &str| opts.servers.is_empty() || opts.servers.iter().any(|s| s == id);

    // ---- pair worlds -------------------------------------------------------
    let mut pairs: Vec<(&World, &World)> = Vec::new();
    let mut used_b: BTreeSet<usize> = BTreeSet::new();
    for wa in &worlds_a {
        if !keep(&wa.id) {
            continue;
        }
        let mut matched: Option<usize> = None;
        for (i, wb) in worlds_b.iter().enumerate() {
            let aliased = opts
                .aliases
                .iter()
                .any(|(x, y)| (*x == wa.id && *y == wb.id) || (*x == wb.id && *y == wa.id));
            if wb.id == wa.id || aliased {
                matched = Some(i);
                break;
            }
        }
        if matched.is_none() {
            // Base-domain heuristic: Multiplayer_2b2t <-> Multiplayer_2b2t.org
            for (i, wb) in worlds_b.iter().enumerate() {
                if base_domain_match(&wa.id, &wb.id) {
                    if opts.auto_alias {
                        matched = Some(i);
                    } else {
                        report
                            .suggested_aliases
                            .push((wa.id.clone(), wb.id.clone()));
                    }
                    break;
                }
            }
        }
        match matched {
            Some(i) => {
                used_b.insert(i);
                pairs.push((wa, &worlds_b[i]));
                report
                    .world_pairs
                    .push((wa.id.clone(), worlds_b[i].id.clone()));
            }
            None => report.only_worlds.push(format!("{} (A only)", wa.id)),
        }
    }
    for (i, wb) in worlds_b.iter().enumerate() {
        if keep(&wb.id) && !used_b.contains(&i) {
            report.only_worlds.push(format!("{} (B only)", wb.id));
        }
    }

    // ---- unpaired worlds: wholesale copy -----------------------------------
    for wa in &worlds_a {
        if keep(&wa.id) && !pairs.iter().any(|(a, _)| a.id == wa.id) {
            copy_world_tree(wa, out, opts.apply, &mut report)?;
        }
    }
    for (i, wb) in worlds_b.iter().enumerate() {
        if keep(&wb.id) && !used_b.contains(&i) {
            copy_world_tree(wb, out, opts.apply, &mut report)?;
        }
    }

    // ---- paired worlds: real merge ------------------------------------------
    for (wa, wb) in pairs {
        merge_world_pair(wa, wb, out, opts, &mut report)?;
    }
    Ok(report)
}

fn base_domain_match(a: &str, b: &str) -> bool {
    let strip = |s: &str| {
        s.strip_prefix("Multiplayer_")
            .unwrap_or(s)
            .trim_end_matches(".org")
            .trim_end_matches(".net")
            .trim_end_matches(".com")
            .to_ascii_lowercase()
    };
    a != b && strip(a) == strip(b)
}

// ---------------------------------------------------------------- fs utils --

fn mtime_of(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn copy_preserving_mtime(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::copy(from, to)
        .map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))?;
    if let Ok(md) = std::fs::metadata(from) {
        if let Ok(t) = md.modified() {
            let _ = filetime::set_file_mtime(to, filetime::FileTime::from_system_time(t));
        }
    }
    Ok(())
}

fn set_mtime_ms(path: &Path, ms: u64) {
    let ft =
        filetime::FileTime::from_unix_time((ms / 1000) as i64, ((ms % 1000) * 1_000_000) as u32);
    let _ = filetime::set_file_mtime(path, ft);
}

/// Copies one world tree (minus caches, temp files and the mod's own backups)
/// into OUT. Used for worlds that exist on one side only.
///
/// A world may legitimately have no world-map data at all — a minimap-only
/// instance still carries waypoints, and dropping it here would silently lose
/// them.
fn copy_world_tree(
    w: &World,
    out: &Path,
    apply: bool,
    report: &mut MergeReport,
) -> Result<(), String> {
    if let Some(wm) = &w.world_map_path {
        let dst_world = out.join("world-map").join(&w.id);
        let mut stack = vec![wm.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let path = entry.path();
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    // Caches are derived, and the mod's own snapshots
                    // (`<version>_backup_<n>/`, `XaeroPlus-db-backups/`) are
                    // superseded copies — on a real archive the DB backups
                    // alone run to tens of gigabytes.
                    if is_cache_dir_name(&name)
                        || name == xaero_core::naming::DB_BACKUP_DIR
                        || xaero_core::naming::parse_backup_dir_name(&name).is_some()
                    {
                        continue;
                    }
                    stack.push(path);
                } else if !xaero_core::naming::is_transient_artifact(&name) {
                    let rel = path.strip_prefix(wm).unwrap();
                    report.aux_copied += 1;
                    if apply {
                        copy_preserving_mtime(&path, &dst_world.join(rel))?;
                    }
                }
            }
        }
    }
    // Minimap side (waypoints + config).
    if let Some(mm) = &w.minimap_path {
        let dst_mm = out.join("minimap").join(&w.id);
        let mut stack = vec![mm.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let path = entry.path();
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    if xaero_core::naming::is_minimap_backup_dir_name(&name) {
                        continue;
                    }
                    stack.push(path);
                } else if !xaero_core::naming::is_transient_artifact(&name) {
                    let rel = path.strip_prefix(mm).unwrap();
                    report.aux_copied += 1;
                    if apply {
                        copy_preserving_mtime(&path, &dst_mm.join(rel))?;
                    }
                }
            }
        }
    }
    Ok(())
}

// ----------------------------------------------------------- world pairing --

/// One mergeable map layer with its source dirs on each side.
struct LayerUnit {
    dim_a: Option<String>, // original folder names per side
    dim_b: Option<String>,
    mw: String,
    cave: Option<i32>,
}

fn merge_world_pair(
    wa: &World,
    wb: &World,
    out: &Path,
    opts: &MergeOptions,
    report: &mut MergeReport,
) -> Result<(), String> {
    let (Some(wma), Some(wmb)) = (&wa.world_map_path, &wb.world_map_path) else {
        // No map layers to merge on at least one side. The minimap half still
        // carries waypoints, so copy rather than drop the world.
        copy_world_tree(wa, out, opts.apply, report)?;
        copy_world_tree(wb, out, opts.apply, report)?;
        return Ok(());
    };
    let out_world = out.join("world-map").join(&wb.id);

    // Canonical dimension key: resource key when parseable, else raw folder.
    let dim_key = |folder: &str| {
        Dimension::from_worldmap_folder(folder)
            .map(|d| d.resource_key())
            .unwrap_or_else(|| folder.to_string())
    };

    // Collect layer units across both sides.
    let mut units: BTreeMap<(String, String, Option<i32>), LayerUnit> = BTreeMap::new();
    for (world, side) in [(wa, 'a'), (wb, 'b')] {
        for dim in &world.dims {
            let key_dim = dim_key(&dim.folder);
            for mw in &dim.multiworlds {
                let mut layers: Vec<Option<i32>> = vec![None];
                layers.extend(mw.cave_layers.iter().map(|n| Some(*n)));
                for cave in layers {
                    let unit = units
                        .entry((key_dim.clone(), mw.id.clone(), cave))
                        .or_insert_with(|| LayerUnit {
                            dim_a: None,
                            dim_b: None,
                            mw: mw.id.clone(),
                            cave,
                        });
                    if side == 'a' {
                        unit.dim_a = Some(dim.folder.clone());
                    } else {
                        unit.dim_b = Some(dim.folder.clone());
                    }
                }
            }
        }
    }

    for ((key_dim, _, _), unit) in &units {
        let dir_a = unit
            .dim_a
            .as_ref()
            .map(|d| layer_dir(wma, d, &unit.mw, unit.cave));
        let dir_b = unit
            .dim_b
            .as_ref()
            .map(|d| layer_dir(wmb, d, &unit.mw, unit.cave));
        let idx_a = dir_a
            .as_deref()
            .and_then(|d| index_regions(d).ok())
            .unwrap_or_default();
        let idx_b = dir_b
            .as_deref()
            .and_then(|d| index_regions(d).ok())
            .unwrap_or_default();
        // Output folder name: prefer B's original name, else A's.
        let out_dim_folder = unit.dim_b.clone().or(unit.dim_a.clone()).unwrap();
        let out_dir = layer_dir(&out_world, &out_dim_folder, &unit.mw, unit.cave);

        let mut ur = UnitReport {
            world: wb.id.clone(),
            dim: out_dim_folder.clone(),
            mw: unit.mw.clone(),
            cave: unit.cave,
            only_a: 0,
            only_b: 0,
            conflicts: 0,
            merge_errors: Vec::new(),
        };

        let mut conflicts: Vec<(i32, i32)> = Vec::new();
        for &coord in idx_a.entries.keys() {
            if idx_b.entries.contains_key(&coord) {
                conflicts.push(coord);
            } else {
                ur.only_a += 1;
                if opts.apply {
                    let from = idx_a.region_path(coord.0, coord.1).unwrap();
                    copy_preserving_mtime(&from, &out_dir.join(from.file_name().unwrap()))?;
                }
            }
        }
        for &coord in idx_b.entries.keys() {
            if !idx_a.entries.contains_key(&coord) {
                ur.only_b += 1;
                if opts.apply {
                    let from = idx_b.region_path(coord.0, coord.1).unwrap();
                    copy_preserving_mtime(&from, &out_dir.join(from.file_name().unwrap()))?;
                }
            }
        }
        ur.conflicts = conflicts.len();

        if opts.apply && !conflicts.is_empty() {
            std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
            let errors: Vec<String> = conflicts
                .par_iter()
                .filter_map(|&(rx, rz)| {
                    merge_one_conflict(&idx_a, &idx_b, rx, rz, &out_dir, opts.prefer).err()
                })
                .collect();
            ur.merge_errors = errors;
        }
        let _ = key_dim;
        report.units.push(ur);
    }

    // ---- aux files ---------------------------------------------------------
    if opts.apply {
        std::fs::create_dir_all(&out_world).map_err(|e| e.to_string())?;
    }
    // dimension_config.txt: union MWName lines, newer file wins for scalars.
    let mut dims_all: BTreeSet<String> = BTreeSet::new();
    for w in [wa, wb] {
        for d in &w.dims {
            dims_all.insert(d.folder.clone());
        }
    }
    for dim_folder in &dims_all {
        let ca = wma.join(dim_folder).join("dimension_config.txt");
        let cb = wmb.join(dim_folder).join("dimension_config.txt");
        // Match A's config under B's folder-name conventions (null vs DIM0).
        let ca = if ca.is_file() {
            ca
        } else {
            let alt = match dim_folder.as_str() {
                "null" => Some("DIM0"),
                "DIM0" => Some("null"),
                _ => None,
            };
            match alt {
                Some(alt) => wma.join(alt).join("dimension_config.txt"),
                None => ca,
            }
        };
        let merged = merge_dimension_config(&ca, &cb);
        if let Some(text) = merged {
            report.aux_copied += 1;
            if opts.apply {
                let to = out_world.join(dim_folder).join("dimension_config.txt");
                std::fs::create_dir_all(to.parent().unwrap()).map_err(|e| e.to_string())?;
                std::fs::write(&to, text).map_err(|e| e.to_string())?;
            }
        }
    }
    // server_config.txt: newer wins.
    let sa = wma.join("server_config.txt");
    let sb = wmb.join("server_config.txt");
    let newest = if mtime_of(&sa) >= mtime_of(&sb) {
        &sa
    } else {
        &sb
    };
    if newest.is_file() {
        report.aux_copied += 1;
        if opts.apply {
            copy_preserving_mtime(newest, &out_world.join("server_config.txt"))?;
        }
    }

    // XaeroPlus DBs: newer file becomes the base, other side merged in.
    let mut db_names: BTreeSet<String> = BTreeSet::new();
    db_names.extend(wa.databases.iter().cloned());
    db_names.extend(wb.databases.iter().cloned());
    for db in &db_names {
        if xaero_db::drawing::is_drawing_db(db) {
            // Drawing DBs use their own schema (highlights/lines/texts/
            // ellipses per dimension). Copying the newer file threw away
            // whatever the other side had drawn — the real archive has
            // thousands of user-drawn highlights in here.
            let pa = wma.join(db);
            let pb = wmb.join(db);
            let (base, other) = if mtime_of(&pa) >= mtime_of(&pb) {
                (&pa, &pb)
            } else {
                (&pb, &pa)
            };
            if !base.is_file() {
                continue;
            }
            let dest = out_world.join(db);
            report.aux_copied += 1;
            if opts.apply {
                copy_preserving_mtime(base, &dest)?;
            }
            if other.is_file() {
                let sources = [other.as_path()];
                let dr = if opts.apply {
                    xaero_db::drawing::merge_into(&dest, &sources, true)?
                } else {
                    // Dry run counts against the base without touching it.
                    xaero_db::drawing::merge_into(base, &sources, false)?
                };
                // Same shape as a highlight merge, so it lands in the same
                // report section and prints identically.
                report.dbs.push(xaero_db::merge::DbMergeReport {
                    dest: dr.dest,
                    sources: dr.sources,
                    tables: dr.tables,
                    applied: dr.applied,
                });
            }
            continue;
        }
        let pa = wma.join(db);
        let pb = wmb.join(db);
        let (base, other) = if mtime_of(&pa) >= mtime_of(&pb) {
            (&pa, &pb)
        } else {
            (&pb, &pa)
        };
        let dest = out_world.join(db);
        if !base.is_file() {
            continue;
        }
        if opts.apply {
            copy_preserving_mtime(base, &dest)?;
        }
        if other.is_file() {
            let sources = [other.as_path()];
            let dbr = if opts.apply {
                xaero_db::merge::merge_into(&dest, &sources, true)?
            } else {
                // Dry-run against the base in place (read-only counting).
                xaero_db::merge::merge_into(base, &sources, false)?
            };
            report.dbs.push(dbr);
        } else if opts.apply {
            // base copied, nothing to merge
        }
    }

    // Minimap waypoints: union per dim%/file.
    let mut wp_keys: BTreeSet<(String, String)> = BTreeSet::new();
    for w in [wa, wb] {
        for (dim, path) in &w.waypoint_files {
            wp_keys.insert((
                dim.clone(),
                path.file_name().unwrap().to_string_lossy().to_string(),
            ));
        }
    }
    for (dim, file) in &wp_keys {
        let find = |w: &World| {
            w.waypoint_files
                .iter()
                .find(|(d, p)| d == dim && p.file_name().unwrap().to_string_lossy() == *file)
                .map(|(_, p)| p.clone())
        };
        let pa = find(wa);
        let pb = find(wb);
        let merged = merge_waypoint_files(pa.as_deref(), pb.as_deref())?;
        if let Some(text) = merged {
            report.waypoint_files_merged += 1;
            if opts.apply {
                let to = out.join("minimap").join(&wb.id).join(dim).join(file);
                std::fs::create_dir_all(to.parent().unwrap()).map_err(|e| e.to_string())?;
                std::fs::write(&to, text).map_err(|e| e.to_string())?;
            }
        }
    }
    // Minimap config.txt: newer wins.
    if let (Some(mma), Some(mmb)) = (&wa.minimap_path, &wb.minimap_path) {
        let ca = mma.join("config.txt");
        let cb = mmb.join("config.txt");
        let newest = if mtime_of(&ca) >= mtime_of(&cb) {
            &ca
        } else {
            &cb
        };
        if newest.is_file() {
            report.aux_copied += 1;
            if opts.apply {
                copy_preserving_mtime(
                    newest,
                    &out.join("minimap").join(&wb.id).join("config.txt"),
                )?;
            }
        }
    } else if let Some(mm) = wa.minimap_path.as_ref().or(wb.minimap_path.as_ref()) {
        let c = mm.join("config.txt");
        if c.is_file() {
            report.aux_copied += 1;
            if opts.apply {
                copy_preserving_mtime(&c, &out.join("minimap").join(&wb.id).join("config.txt"))?;
            }
        }
    }
    Ok(())
}

fn merge_one_conflict(
    idx_a: &RegionIndex,
    idx_b: &RegionIndex,
    rx: i32,
    rz: i32,
    out_dir: &Path,
    prefer: Prefer,
) -> Result<(), String> {
    let pa = idx_a.region_path(rx, rz).unwrap();
    let pb = idx_b.region_path(rx, rz).unwrap();
    let ma = idx_a.entries[&(rx, rz)].mtime_ms;
    let mb = idx_b.entries[&(rx, rz)].mtime_ms;
    let a_primary = match prefer {
        Prefer::A => true,
        Prefer::B => false,
        Prefer::Mtime => ma >= mb,
    };
    let ctx = |p: &Path, e: String| format!("{}: {e}", p.display());
    let load = |p: &Path| -> Result<xaero_core::DecodedRegion, String> {
        let bytes = std::fs::read(p).map_err(|e| ctx(p, e.to_string()))?;
        let stream =
            xaero_core::read_region_container(&bytes).map_err(|e| ctx(p, e.to_string()))?;
        xaero_core::decode_region(&stream).map_err(|e| ctx(p, e.to_string()))
    };
    let da = load(&pa);
    let db = load(&pb);
    let out_path = out_dir.join(format!("{rx}_{rz}.zip"));
    match (da, db) {
        (Ok(da), Ok(db)) => {
            let (primary, secondary) = if a_primary { (&da, &db) } else { (&db, &da) };
            let merged = xaero_core::merge::merge_regions(primary, secondary);
            let stream = xaero_core::encode_region(&merged);
            // Sanity: the merge output must decode cleanly before we keep it.
            xaero_core::decode_region(&stream).map_err(|e| format!("self-check {rx}_{rz}: {e}"))?;
            let container = xaero_core::write_region_container(&stream)
                .map_err(|e| format!("zip {rx}_{rz}: {e}"))?;
            let tmp = out_dir.join(format!("{rx}_{rz}.zip.tmp-xt"));
            std::fs::write(&tmp, container).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, &out_path).map_err(|e| e.to_string())?;
            set_mtime_ms(&out_path, ma.max(mb));
            Ok(())
        }
        // One side unreadable: keep the readable one rather than losing data.
        (Ok(_), Err(e)) => {
            copy_preserving_mtime(&pa, &out_path)?;
            Err(format!("{rx}_{rz}: B side unreadable, copied A ({e})"))
        }
        (Err(e), Ok(_)) => {
            copy_preserving_mtime(&pb, &out_path)?;
            Err(format!("{rx}_{rz}: A side unreadable, copied B ({e})"))
        }
        (Err(ea), Err(eb)) => Err(format!("{rx}_{rz}: both unreadable ({ea}; {eb})")),
    }
}

fn merge_dimension_config(a: &Path, b: &Path) -> Option<String> {
    let ta = std::fs::read_to_string(a).ok();
    let tb = std::fs::read_to_string(b).ok();
    match (ta, tb) {
        (None, None) => None,
        (Some(t), None) | (None, Some(t)) => Some(t),
        (Some(ta), Some(tb)) => {
            // Newer file provides the scalar lines; MWName lines are unioned.
            let (newer, older) = if mtime_of(a) >= mtime_of(b) {
                (&ta, &tb)
            } else {
                (&tb, &ta)
            };
            let mut lines: Vec<String> = Vec::new();
            let mut mw_seen: BTreeSet<String> = BTreeSet::new();
            for line in newer.lines() {
                lines.push(line.to_string());
                if let Some(rest) = line.strip_prefix("MWName:") {
                    if let Some((mw, _)) = rest.split_once(':') {
                        mw_seen.insert(mw.to_string());
                    }
                }
            }
            let mut extra: Vec<String> = Vec::new();
            for line in older.lines() {
                if let Some(rest) = line.strip_prefix("MWName:") {
                    if let Some((mw, _)) = rest.split_once(':') {
                        if !mw_seen.contains(mw) {
                            extra.push(line.to_string());
                        }
                    }
                }
            }
            // Insert extra MWName lines after the last existing one (or at top).
            let insert_at = lines
                .iter()
                .rposition(|l| l.starts_with("MWName:"))
                .map(|i| i + 1)
                .unwrap_or(0);
            for (off, line) in extra.into_iter().enumerate() {
                lines.insert(insert_at + off, line);
            }
            Some(lines.join("\n") + "\n")
        }
    }
}

fn merge_waypoint_files(a: Option<&Path>, b: Option<&Path>) -> Result<Option<String>, String> {
    let read =
        |p: Option<&Path>| -> Option<String> { p.and_then(|p| std::fs::read_to_string(p).ok()) };
    let (ta, tb) = (read(a), read(b));
    let (ta, tb) = match (ta, tb) {
        (None, None) => return Ok(None),
        (Some(t), None) => return Ok(Some(t)),
        (None, Some(t)) => return Ok(Some(t)),
        (Some(a), Some(b)) => (a, b),
    };
    let pa = parse_waypoints_file(&ta);
    let pb = parse_waypoints_file(&tb);
    let mut out = String::new();
    out.push_str("#\n#waypoint:name:initials:x:y:z:color:disabled:type:set:rotate_on_tp:tp_yaw:visibility_type:destination\n#\n");
    let mut sets: Vec<String> = Vec::new();
    for s in pa.sets.iter().chain(pb.sets.iter()) {
        if !sets.contains(s) {
            sets.push(s.clone());
        }
    }
    if !sets.is_empty() {
        out.push_str(&format!("sets:{}\n", sets.join(":")));
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for wp in pa.waypoints.iter().chain(pb.waypoints.iter()) {
        let line = format_waypoint_line(wp);
        if seen.insert(line.clone()) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    for line in pa.other_lines.iter().chain(pb.other_lines.iter()) {
        if seen.insert(line.clone()) {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(Some(out))
}
