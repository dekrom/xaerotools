//! Folder- and file-name conventions of Xaero's World Map / Minimap saves.
//! Centralizes every `$`/`%` escaping rule so nothing else guesses.
//!
//! World-map dimension folders: `null` (overworld; `DIM0` under XaeroPlus's
//! nullOverworldDimensionFolder=false), `DIM-1` (nether), `DIM1` (end), or an
//! escaped resource id: `:` -> `$`, `/` -> `%` (e.g. dimension
//! `minecraft:worlds/2b2t/2b2t_1` -> folder `minecraft$worlds%2b2t%2b2t_1`).
//! Minimap dimension folders use `dim%<numericId>` for vanilla and
//! `dim%<escaped id>` otherwise.

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Dimension {
    Overworld,
    Nether,
    End,
    /// Full resource id, e.g. "minecraft:worlds/2b2t/2b2t_1".
    Custom(String),
}

impl Dimension {
    /// Canonical resource key ("minecraft:overworld", ...), which is also the
    /// XaeroPlus SQLite table name for this dimension.
    pub fn resource_key(&self) -> String {
        match self {
            Dimension::Overworld => "minecraft:overworld".into(),
            Dimension::Nether => "minecraft:the_nether".into(),
            Dimension::End => "minecraft:the_end".into(),
            Dimension::Custom(id) => id.clone(),
        }
    }

    /// Parses a world-map dimension folder name.
    pub fn from_worldmap_folder(folder: &str) -> Option<Dimension> {
        match folder {
            "null" | "DIM0" => Some(Dimension::Overworld),
            "DIM-1" => Some(Dimension::Nether),
            "DIM1" => Some(Dimension::End),
            _ => {
                // `mw$default` and friends also carry a `$`; a root pointed
                // at a dimension folder must not read them as dimensions.
                if folder.contains('$') && !is_multiworld_folder(folder) {
                    Some(Dimension::Custom(unescape_folder_id(folder)))
                } else {
                    None
                }
            }
        }
    }

    /// World-map folder name; `null_overworld` selects `null` vs `DIM0`.
    pub fn to_worldmap_folder(&self, null_overworld: bool) -> String {
        match self {
            Dimension::Overworld => if null_overworld { "null" } else { "DIM0" }.into(),
            Dimension::Nether => "DIM-1".into(),
            Dimension::End => "DIM1".into(),
            Dimension::Custom(id) => escape_folder_id(id),
        }
    }

    /// Parses a minimap dimension folder name (`dim%0`, `dim%-1`, `dim%1`,
    /// `dim%<escaped id>`). Also accepts the pre-`dim%` folder names the mod
    /// still maps on load (`WaypointOldIO.fixOldDimensionName`).
    pub fn from_minimap_folder(folder: &str) -> Option<Dimension> {
        match folder {
            "Overworld" => return Some(Dimension::Overworld),
            "Nether" => return Some(Dimension::Nether),
            "The End" => return Some(Dimension::End),
            _ => {}
        }
        let rest = folder.strip_prefix("dim%")?;
        match rest {
            "0" => Some(Dimension::Overworld),
            "-1" => Some(Dimension::Nether),
            "1" => Some(Dimension::End),
            _ => Some(Dimension::Custom(unescape_folder_id(rest))),
        }
    }

    pub fn to_minimap_folder(&self) -> String {
        match self {
            Dimension::Overworld => "dim%0".into(),
            Dimension::Nether => "dim%-1".into(),
            Dimension::End => "dim%1".into(),
            Dimension::Custom(id) => format!("dim%{}", escape_folder_id(id)),
        }
    }

    /// Short human label. Custom ids collapse to their last path segment
    /// (`minecraft:worlds/2b2t/2b2t_1` -> `2b2t_1`) so several custom
    /// dimensions of the same vanilla type stay distinguishable.
    pub fn display_name(&self) -> String {
        match self {
            Dimension::Overworld => "Overworld".into(),
            Dimension::Nether => "Nether".into(),
            Dimension::End => "The End".into(),
            Dimension::Custom(id) => {
                let tail = id.rsplit(['/', ':']).next().unwrap_or(id);
                if tail.is_empty() {
                    id.clone()
                } else {
                    tail.into()
                }
            }
        }
    }
}

/// Resource id -> folder segment (`:` -> `$`, `/` -> `%`).
pub fn escape_folder_id(id: &str) -> String {
    id.replace(':', "$").replace('/', "%")
}

/// Folder segment -> resource id. Only the first `$` is the namespace
/// separator; every `%` was a `/`.
pub fn unescape_folder_id(folder: &str) -> String {
    let with_slashes = folder.replace('%', "/");
    match with_slashes.split_once('$') {
        Some((ns, path)) => format!("{ns}:{path}"),
        None => with_slashes,
    }
}

/// Parses `<rx>_<rz>.zip` / `<rx>_<rz>.xaero`. Returns (rx, rz, is_zip).
pub fn parse_region_filename(name: &str) -> Option<(i32, i32, bool)> {
    let (stem, is_zip) = if let Some(s) = name.strip_suffix(".zip") {
        (s, true)
    } else {
        let s = name.strip_suffix(".xaero")?;
        (s, false)
    };
    let (a, b) = stem.split_once('_')?;
    // Reject stray files like `12_34_backup`: both halves must be pure ints,
    // and the mod's regex (`-?\d+`) takes no `+` sign where `i32::from_str`
    // would — `+0_5.zip` is not a region the game reads.
    if a.starts_with('+') || b.starts_with('+') {
        return None;
    }
    Some((a.parse().ok()?, b.parse().ok()?, is_zip))
}

pub fn region_filename(rx: i32, rz: i32) -> String {
    format!("{rx}_{rz}.zip")
}

/// True for the derived-cache directory names that must be skipped when
/// scanning and never copied by merges: `cache`, `caches`, `cache_<n>`.
pub fn is_cache_dir_name(name: &str) -> bool {
    name == "cache" || name == "caches" || {
        name.strip_prefix("cache_")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    }
}

/// Multiworld folder. Mirrors `MapDimension.loadMultiworldsList`, which accepts
/// three forms: `mw$*` (`mw$default`, `mw$-542221765`), `cm$*` (the multiworld
/// Xaero creates when converting old-format data, e.g. `cm$converted`) and the
/// legacy `mw<x>,<y>,<z>` form still written into `defaultMultiworldId`.
pub fn is_multiworld_folder(name: &str) -> bool {
    name.starts_with("mw$") || name.starts_with("cm$") || is_legacy_multiworld_folder(name)
}

/// `^mw(-?\d+),(-?\d+),(-?\d+)$`, hand-rolled: xaero-core has no regex dep and
/// this runs once per directory entry of a scan.
fn is_legacy_multiworld_folder(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("mw") else {
        return false;
    };
    let mut parts = rest.split(',');
    let int = |p: Option<&str>| {
        p.is_some_and(|p| {
            let digits = p.strip_prefix('-').unwrap_or(p);
            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
        })
    };
    int(parts.next()) && int(parts.next()) && int(parts.next()) && parts.next().is_none()
}

/// Xaero moves a region file it is about to rewrite at a newer format version
/// into a sibling `<fullVersion>_backup_<n>` dir (`MapSaveLoad.getBackupFolder`,
/// a `Files.move`), so these dirs can hold the only surviving copy of a region.
/// Returns the raw i32 `fullVersion` from the header (major = `v >> 16`, minor =
/// `v & 0xFFFF`; the archive holds 2.24 through 7.8) and the slot index.
pub fn parse_backup_dir_name(name: &str) -> Option<(i32, u32)> {
    let (version, index) = name.split_once("_backup_")?;
    Some((version.parse().ok()?, index.parse().ok()?))
}

/// `XaeroPlus-db-backups` (`DatabaseMigrator`): dated snapshots of the
/// XaeroPlus SQLite DBs, derived data that must never be scanned or copied.
pub const DB_BACKUP_DIR: &str = "XaeroPlus-db-backups";

/// The minimap's own waypoint backups: `backup`, `backup-`, `backup--`, ...
/// (`SimpleBackup.moveToBackup`). The mod skips them when loading
/// (`^backup-*$` in `MinimapWorldManagerIO`) and so must we — they hold
/// waypoints the player may since have deleted.
pub fn is_minimap_backup_dir_name(name: &str) -> bool {
    name.strip_prefix("backup")
        .is_some_and(|rest| rest.bytes().all(|b| b == b'-'))
}

/// Files that are derived, mid-write, superseded or written by something other
/// than the mod, and are therefore never live region data and never worth
/// copying: `*.temp`, `*.outdated`, `*.xwmc` render caches, Xaero's
/// `*.backup<n>` (and XaeroPlus's `*.backup<n>-xp-<n>`), Syncthing conflict
/// copies, SQLite sidecars, `.lock`, and our own merge/ingest temp files
/// (`*.tmp-xt`, `*.zip.tmp*`) that a killed run can leave behind.
pub fn is_transient_artifact(name: &str) -> bool {
    if name.ends_with(".temp") || name.ends_with(".outdated") || name == ".lock" {
        return true;
    }
    if name.ends_with(".tmp-xt") || name.contains(".zip.tmp") {
        return true;
    }
    if name.ends_with(".xwmc") {
        return true;
    }
    if name.ends_with(".db-wal") || name.ends_with(".db-shm") || name.ends_with(".db-journal") {
        return true;
    }
    if name.contains(".sync-conflict-") {
        return true;
    }
    let base = match name.rsplit_once("-xp-") {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => name,
    };
    base.rsplit_once(".backup")
        .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Syncthing conflict copy of a region file:
/// `<rx>_<rz>.sync-conflict-<stamp>-<device>.<ext>`. Returns
/// (rx, rz, is_zip, `<stamp>-<device>`). These are alternate versions of a
/// region, not live data — the mod's own filename regex rejects them too.
pub fn parse_sync_conflict_filename(name: &str) -> Option<(i32, i32, bool, &str)> {
    let (stem, tag) = name.split_once(".sync-conflict-")?;
    let (tag, is_zip) = if let Some(t) = tag.strip_suffix(".zip") {
        (t, true)
    } else {
        (tag.strip_suffix(".xaero")?, false)
    };
    let (rx, rz) = stem.split_once('_')?;
    Some((rx.parse().ok()?, rz.parse().ok()?, is_zip, tag))
}

/// Minimap waypoint file name, per `MinimapWorldManagerIO.loadWorldFile`:
/// `<multiworldId>_<displayName>.txt`, where `_` inside the display name is
/// stored as `%us%`, plus the legacy `waypoints.txt` which carries no
/// multiworld id. Returns (multiworld id, display name).
pub fn parse_waypoint_filename(name: &str) -> Option<(Option<&str>, String)> {
    if !name.ends_with(".txt") {
        return None;
    }
    // The mod cuts at the LAST '.', so `default.cfg.txt` yields `default.cfg`,
    // which then fails the `_` split below exactly as it does in game.
    let (stem, _) = name.rsplit_once('.')?;
    if stem == "waypoints" {
        return Some((None, stem.to_string()));
    }
    let mut parts = stem.split('_');
    let mw = parts.next()?;
    let display = parts.next()?;
    Some((Some(mw), display.replace("%us%", "_")))
}

/// `caves/<layer>` uses `Integer.MIN_VALUE` for the "full" cave map — one layer
/// spanning the whole column, ignoring the used top Y (`caveModeType:2`, and
/// what XaeroPlus's netherCaveFix forces). Any other layer is `topY >> 4`.
pub const CAVE_LAYER_FULL: i32 = i32::MIN;

/// Human label for a `caves/<layer>` folder.
pub fn cave_layer_label(layer: i32) -> String {
    if layer == CAVE_LAYER_FULL {
        return "Cave (full column)".into();
    }
    let bottom = layer as i64 * 16;
    format!("Cave Y {}..{}", bottom, bottom + 15)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worldmap_folders() {
        assert_eq!(
            Dimension::from_worldmap_folder("null"),
            Some(Dimension::Overworld)
        );
        assert_eq!(
            Dimension::from_worldmap_folder("DIM0"),
            Some(Dimension::Overworld)
        );
        assert_eq!(
            Dimension::from_worldmap_folder("DIM-1"),
            Some(Dimension::Nether)
        );
        assert_eq!(
            Dimension::from_worldmap_folder("DIM1"),
            Some(Dimension::End)
        );
        assert_eq!(
            Dimension::from_worldmap_folder("minecraft$worlds%2b2t%2b2t_1"),
            Some(Dimension::Custom("minecraft:worlds/2b2t/2b2t_1".into()))
        );
        assert_eq!(Dimension::from_worldmap_folder("random"), None);
        // Multiworld folder names are never dimensions.
        assert_eq!(Dimension::from_worldmap_folder("mw$default"), None);
        assert_eq!(Dimension::from_worldmap_folder("mw$-542221765"), None);
        assert_eq!(Dimension::from_worldmap_folder("cm$converted"), None);
        assert_eq!(Dimension::from_worldmap_folder("mw1,2,3"), None);
        assert_eq!(
            Dimension::Custom("minecraft:worlds/2b2t/2b2t_1".into()).to_worldmap_folder(true),
            "minecraft$worlds%2b2t%2b2t_1"
        );
        assert_eq!(Dimension::Overworld.to_worldmap_folder(true), "null");
        assert_eq!(Dimension::Overworld.to_worldmap_folder(false), "DIM0");
    }

    #[test]
    fn minimap_folders() {
        assert_eq!(
            Dimension::from_minimap_folder("dim%0"),
            Some(Dimension::Overworld)
        );
        assert_eq!(
            Dimension::from_minimap_folder("dim%-1"),
            Some(Dimension::Nether)
        );
        assert_eq!(
            Dimension::from_minimap_folder("dim%minecraft$worlds%2b2t%2b2t_1"),
            Some(Dimension::Custom("minecraft:worlds/2b2t/2b2t_1".into()))
        );
        assert_eq!(Dimension::Nether.to_minimap_folder(), "dim%-1");
    }

    #[test]
    fn region_filenames() {
        assert_eq!(
            parse_region_filename("4040_-9370.zip"),
            Some((4040, -9370, true))
        );
        assert_eq!(parse_region_filename("0_-24.xaero"), Some((0, -24, false)));
        assert_eq!(parse_region_filename("0_-24.zip.temp"), None);
        assert_eq!(parse_region_filename("12_34_x.zip"), None);
        assert_eq!(parse_region_filename("cache_1"), None);
        assert_eq!(parse_region_filename("+5_3.zip"), None);
        assert_eq!(parse_region_filename("5_+3.zip"), None);
        assert_eq!(region_filename(-3, 7), "-3_7.zip");
    }

    #[test]
    fn multiworld_folders() {
        for ok in [
            "mw$default",
            "mw$-542221765",
            "cm$converted",
            "mw0,1,0",
            "mw1112,1,-5422",
            "mw-1,0,-3",
            "mw7816,2,7809",
        ] {
            assert!(is_multiworld_folder(ok), "{ok}");
        }
        for bad in [
            "mw0,1",
            "mw0,1,0,2",
            "mwa,b,c",
            "mw",
            "mw,,",
            "mw+1,1,1",
            "mw 0,1,0",
            "mw0,1,",
            "caves",
            "cache_1",
        ] {
            assert!(!is_multiworld_folder(bad), "{bad}");
        }
    }

    #[test]
    fn backup_and_transient_names() {
        assert_eq!(
            parse_backup_dir_name("458760_backup_32"),
            Some((458760, 32))
        );
        assert_eq!(parse_backup_dir_name("131096_backup_0"), Some((131096, 0)));
        assert_eq!(parse_backup_dir_name("foo_backup_bar"), None);
        assert_eq!(parse_backup_dir_name("caves"), None);

        assert!(is_minimap_backup_dir_name("backup"));
        assert!(is_minimap_backup_dir_name("backup--"));
        assert!(!is_minimap_backup_dir_name("backup-1"));
        assert!(!is_minimap_backup_dir_name("backups"));

        for t in [
            "0_0.zip.temp",
            "0_0.zip.temp.backup10266733",
            "0_0.zip.backup3",
            "-100_-3638.zip.temp.backup12-xp-988134450",
            "1126_-3782.sync-conflict-20240705-215039-QQNGROR.zip",
            "XaeroPlusOldBiomes.db-wal",
            "XaeroPlusNewChunksData 108145703224700.temp",
            "region.xaero.outdated",
            "0_0.xwmc",
            ".lock",
        ] {
            assert!(is_transient_artifact(t), "{t}");
        }
        for keep in [
            "0_0.zip",
            "0_0.xaero",
            "XaeroPlusOldBiomes.db",
            "mw$default_1.txt",
            "dimension_config.txt",
        ] {
            assert!(!is_transient_artifact(keep), "{keep}");
        }
    }

    #[test]
    fn transient_artifacts() {
        for name in [
            "0_0.zip.temp",
            "0_0.outdated",
            "0_0.xwmc",
            ".lock",
            "XaeroPlusNewChunks.db-wal",
            "0_0.zip.backup1",
            "0_0.zip.backup2-xp-3",
            "0_0.sync-conflict-20240705-215039-Q.zip",
            "0_0.zip.tmp-xt",
            "0_0.zip.tmp",
            "0_0.zip.tmp-abc",
        ] {
            assert!(is_transient_artifact(name), "{name}");
        }
        for name in [
            "0_0.zip",
            "0_0.xaero",
            "dimension_config.txt",
            "XaeroPlusNewChunks.db",
        ] {
            assert!(!is_transient_artifact(name), "{name}");
        }
    }

    #[test]
    fn sync_conflict_filenames() {
        assert_eq!(
            parse_sync_conflict_filename("1126_-3782.sync-conflict-20240705-215039-QQNGROR.zip"),
            Some((1126, -3782, true, "20240705-215039-QQNGROR"))
        );
        assert_eq!(parse_sync_conflict_filename("1126_-3782.zip"), None);
        assert_eq!(
            parse_sync_conflict_filename("x_y.sync-conflict-20240705-215039-Q.zip"),
            None
        );
    }

    #[test]
    fn waypoint_filenames() {
        assert_eq!(
            parse_waypoint_filename("mw$default_1.txt"),
            Some((Some("mw$default"), "1".to_string()))
        );
        assert_eq!(
            parse_waypoint_filename("mw0,1,0_2.txt"),
            Some((Some("mw0,1,0"), "2".to_string()))
        );
        assert_eq!(
            parse_waypoint_filename("mw$-542221765_Map%us%2.txt"),
            Some((Some("mw$-542221765"), "Map_2".to_string()))
        );
        assert_eq!(
            parse_waypoint_filename("waypoints.txt"),
            Some((None, "waypoints".to_string()))
        );
        assert_eq!(parse_waypoint_filename("default.cfg.txt"), None);
        assert_eq!(parse_waypoint_filename("config.txt"), None);
        assert_eq!(parse_waypoint_filename("mw$default_1.txt.temp"), None);
    }

    #[test]
    fn cave_layer_labels() {
        assert_eq!(cave_layer_label(CAVE_LAYER_FULL), "Cave (full column)");
        assert_eq!(cave_layer_label(-4), "Cave Y -64..-49");
        assert_eq!(cave_layer_label(19), "Cave Y 304..319");
    }

    #[test]
    fn dimension_labels() {
        assert_eq!(Dimension::Overworld.display_name(), "Overworld");
        assert_eq!(
            Dimension::Custom("minecraft:worlds/2b2t/2b2t_1".into()).display_name(),
            "2b2t_1"
        );
        assert_eq!(
            Dimension::Custom("minecraft:brazil".into()).display_name(),
            "brazil"
        );
        assert_eq!(
            Dimension::from_minimap_folder("Nether"),
            Some(Dimension::Nether)
        );
    }

    #[test]
    fn cache_dirs() {
        assert!(is_cache_dir_name("cache"));
        assert!(is_cache_dir_name("caches"));
        assert!(is_cache_dir_name("cache_1"));
        assert!(is_cache_dir_name("cache_25"));
        assert!(!is_cache_dir_name("cache_x"));
        assert!(!is_cache_dir_name("caves"));
        assert!(!is_cache_dir_name("cache_"));
    }
}
