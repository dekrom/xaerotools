//! The waypoint vault — XaeroTools' own database of every waypoint ever seen.
//!
//! Motivation: players (especially multi-account hunters) create far more
//! waypoints than the in-game list can hold and routinely delete them. The
//! vault ingests waypoints from every scanned root/account and NEVER deletes:
//! a waypoint missing from the live files is flagged `present = 0`
//! ("archived") and kept forever, with first/last-seen timestamps.
//!
//! Identity: (world, dim, mw file, name, x, y, z) — the same waypoint synced
//! from several accounts collapses into one row; re-adding a deleted waypoint
//! in game revives its row.

use std::path::Path;

use rusqlite::{params, Connection};
use xaero_core::waypoints::Waypoint;

pub struct Vault {
    conn: Connection,
}

/// One live waypoint file's parsed content, ready to sync.
pub struct VaultBatch {
    /// World id, e.g. "Multiplayer_2b2t" (shared across accounts).
    pub world: String,
    /// Dimension resource key, e.g. "minecraft:the_nether".
    pub dim_key: String,
    /// Waypoint file name, e.g. "mw$default_1.txt".
    pub mw_file: String,
    /// Where this copy came from (root path) — informational.
    pub source: String,
    pub waypoints: Vec<Waypoint>,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct VaultSyncReport {
    pub seen: usize,
    pub added: usize,
    pub revived: usize,
    pub newly_archived: usize,
    pub total: usize,
    pub archived_total: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VaultWaypoint {
    pub world: String,
    #[serde(rename = "dimKey")]
    pub dim_key: String,
    #[serde(rename = "mwFile")]
    pub mw_file: String,
    pub name: String,
    pub initials: String,
    pub x: i32,
    pub y: Option<i32>,
    pub z: i32,
    pub color: u8,
    pub purpose: i32,
    pub set: String,
    pub present: bool,
    #[serde(rename = "firstSeen")]
    pub first_seen: i64,
    #[serde(rename = "lastSeen")]
    pub last_seen: i64,
    pub source: String,
}

const Y_NONE: i64 = -100_000;

impl Vault {
    /// Opens (creating if needed) the vault database.
    pub fn open(path: &Path) -> Result<Vault, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))
            .map_err(|e| e.to_string())?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS vault_meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT OR IGNORE INTO vault_meta VALUES ('schema', '1');
             CREATE TABLE IF NOT EXISTS waypoints (
               id INTEGER PRIMARY KEY,
               world TEXT NOT NULL,
               dim TEXT NOT NULL,
               mw_file TEXT NOT NULL,
               name TEXT NOT NULL,
               initials TEXT NOT NULL DEFAULT '',
               x INTEGER NOT NULL,
               y INTEGER,
               y_key INTEGER NOT NULL,
               z INTEGER NOT NULL,
               color INTEGER NOT NULL DEFAULT 0,
               purpose INTEGER NOT NULL DEFAULT 0,
               wp_set TEXT NOT NULL DEFAULT 'gui.xaero_default',
               disabled INTEGER NOT NULL DEFAULT 0,
               rotate_on_tp INTEGER NOT NULL DEFAULT 0,
               tp_yaw INTEGER NOT NULL DEFAULT 0,
               visibility_type INTEGER NOT NULL DEFAULT 0,
               destination INTEGER NOT NULL DEFAULT 0,
               present INTEGER NOT NULL DEFAULT 1,
               first_seen INTEGER NOT NULL,
               last_seen INTEGER NOT NULL,
               source TEXT NOT NULL DEFAULT '',
               UNIQUE (world, dim, mw_file, name, x, y_key, z)
             );
             CREATE INDEX IF NOT EXISTS idx_wp_world ON waypoints (world, dim);",
        )
        .map_err(|e| e.to_string())?;
        Ok(Vault { conn })
    }

    /// Ingests live waypoint files. Rows already known get their fields and
    /// `last_seen` refreshed; unknown rows are inserted. Afterwards, rows
    /// belonging to a (world, dim, mw_file) group that WAS scanned this run
    /// but were not seen in any batch of the group are flagged archived.
    /// Groups not scanned this run are left untouched.
    pub fn sync(&mut self, batches: &[VaultBatch], now_ms: i64) -> Result<VaultSyncReport, String> {
        let e = |e: rusqlite::Error| e.to_string();
        let tx = self.conn.transaction().map_err(e)?;
        let mut report = VaultSyncReport::default();
        let counts = |tx: &rusqlite::Transaction| -> Result<(i64, i64), String> {
            tx.query_row(
                "SELECT COUNT(*), COALESCE(SUM(present = 0), 0) FROM waypoints",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .map_err(|e| e.to_string())
        };
        let (total_before, archived_before) = counts(&tx)?;
        {
            let mut upsert = tx
                .prepare(
                    "INSERT INTO waypoints
                       (world, dim, mw_file, name, initials, x, y, y_key, z, color, purpose,
                        wp_set, disabled, rotate_on_tp, tp_yaw, visibility_type, destination,
                        present, first_seen, last_seen, source)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,1,?18,?18,?19)
                     ON CONFLICT(world, dim, mw_file, name, x, y_key, z) DO UPDATE SET
                       initials=excluded.initials, color=excluded.color,
                       purpose=excluded.purpose, wp_set=excluded.wp_set,
                       disabled=excluded.disabled, rotate_on_tp=excluded.rotate_on_tp,
                       tp_yaw=excluded.tp_yaw, visibility_type=excluded.visibility_type,
                       destination=excluded.destination,
                       present=1, last_seen=excluded.last_seen, source=excluded.source",
                )
                .map_err(e)?;
            for batch in batches {
                for wp in &batch.waypoints {
                    let y_key = wp.y.map(|v| v as i64).unwrap_or(Y_NONE);
                    upsert
                        .execute(params![
                            batch.world,
                            batch.dim_key,
                            batch.mw_file,
                            wp.name,
                            wp.initials,
                            wp.x,
                            wp.y,
                            y_key,
                            wp.z,
                            wp.color,
                            wp.purpose,
                            wp.set,
                            wp.disabled as i64,
                            wp.rotate_on_tp as i64,
                            wp.tp_yaw,
                            wp.visibility_type,
                            wp.destination as i64,
                            now_ms,
                            batch.source,
                        ])
                        .map_err(e)?;
                    report.seen += 1;
                }
            }
        }
        // Delta accounting is unambiguous even when several accounts submit
        // the same waypoint within one run.
        let (total_after, archived_after) = counts(&tx)?;
        report.added = (total_after - total_before) as usize;
        report.revived = (archived_before - archived_after).max(0) as usize;
        // Archive rows in scanned groups that no batch touched this run.
        {
            let mut groups: Vec<(&str, &str, &str)> = batches
                .iter()
                .map(|b| (b.world.as_str(), b.dim_key.as_str(), b.mw_file.as_str()))
                .collect();
            groups.sort();
            groups.dedup();
            let mut archive = tx
                .prepare(
                    "UPDATE waypoints SET present = 0
                     WHERE world = ?1 AND dim = ?2 AND mw_file = ?3
                       AND present = 1 AND last_seen < ?4",
                )
                .map_err(e)?;
            for (world, dim, mw_file) in groups {
                report.newly_archived += archive
                    .execute(params![world, dim, mw_file, now_ms])
                    .map_err(e)?;
            }
        }
        tx.commit().map_err(e)?;
        let (total, archived_total) = self.stats()?;
        report.total = total;
        report.archived_total = archived_total;
        Ok(report)
    }

    pub fn stats(&self) -> Result<(usize, usize), String> {
        self.conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(present = 0), 0) FROM waypoints",
                [],
                |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize)),
            )
            .map_err(|e| e.to_string())
    }

    pub fn waypoints_for_world(
        &self,
        world: &str,
        archived_only: bool,
    ) -> Result<Vec<VaultWaypoint>, String> {
        let sql = format!(
            "SELECT world, dim, mw_file, name, initials, x, y, z, color, purpose, wp_set,
                    present, first_seen, last_seen, source
             FROM waypoints WHERE world = ?1 {} ORDER BY dim, name",
            if archived_only { "AND present = 0" } else { "" }
        );
        let mut stmt = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([world], |r| {
                Ok(VaultWaypoint {
                    world: r.get(0)?,
                    dim_key: r.get(1)?,
                    mw_file: r.get(2)?,
                    name: r.get(3)?,
                    initials: r.get(4)?,
                    x: r.get(5)?,
                    y: r.get(6)?,
                    z: r.get(7)?,
                    color: r.get::<_, i64>(8)? as u8,
                    purpose: r.get::<_, i64>(9)? as i32,
                    set: r.get(10)?,
                    present: r.get::<_, i64>(11)? != 0,
                    first_seen: r.get(12)?,
                    last_seen: r.get(13)?,
                    source: r.get(14)?,
                })
            })
            .map_err(|e| e.to_string())?;
        Ok(rows.flatten().collect())
    }

    pub fn worlds(&self) -> Result<Vec<(String, usize, usize)>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT world, COUNT(*), COALESCE(SUM(present = 0), 0)
                 FROM waypoints GROUP BY world ORDER BY world",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as usize,
                    r.get::<_, i64>(2)? as usize,
                ))
            })
            .map_err(|e| e.to_string())?;
        Ok(rows.flatten().collect())
    }

    /// Renders one (world, dim, mw_file) group back into Xaero's waypoint file
    /// format — game-ready, so archived waypoints can be RESTORED by dropping
    /// the file into `minimap/<world>/dim%<n>/`.
    pub fn export_file(
        &self,
        world: &str,
        dim_key: &str,
        mw_file: &str,
        include_archived: bool,
    ) -> Result<String, String> {
        let all = self.waypoints_for_world(world, false)?;
        let mut out = String::from(
            "#\n#waypoint:name:initials:x:y:z:color:disabled:type:set:rotate_on_tp:tp_yaw:visibility_type:destination\n#\n",
        );
        for wp in all {
            if wp.dim_key != dim_key || wp.mw_file != mw_file {
                continue;
            }
            if !wp.present && !include_archived {
                continue;
            }
            let w = Waypoint {
                name: wp.name,
                initials: wp.initials,
                x: wp.x,
                y: wp.y,
                z: wp.z,
                color: wp.color,
                disabled: false,
                purpose: wp.purpose,
                set: wp.set,
                rotate_on_tp: false,
                tp_yaw: 0,
                visibility_type: 0,
                destination: false,
            };
            out.push_str(&xaero_core::waypoints::format_waypoint_line(&w));
            out.push('\n');
        }
        Ok(out)
    }
}

/// Platform data location for the vault: `$XDG_DATA_HOME/xaerotools/vault.db`,
/// `%APPDATA%\xaerotools\vault.db`, or `~/.local/share/xaerotools/vault.db`.
pub fn default_vault_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(std::path::PathBuf::from))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("xaerotools").join("vault.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wp(name: &str, x: i32, z: i32) -> Waypoint {
        Waypoint {
            name: name.into(),
            initials: "W".into(),
            x,
            y: Some(64),
            z,
            color: 5,
            disabled: false,
            purpose: 0,
            set: "gui.xaero_default".into(),
            rotate_on_tp: false,
            tp_yaw: 0,
            visibility_type: 0,
            destination: false,
        }
    }

    fn batch(world: &str, src: &str, wps: Vec<Waypoint>) -> VaultBatch {
        VaultBatch {
            world: world.into(),
            dim_key: "minecraft:the_nether".into(),
            mw_file: "mw$default_1.txt".into(),
            source: src.into(),
            waypoints: wps,
        }
    }

    #[test]
    fn survives_ingame_deletion_and_multi_account() {
        let dir = std::env::temp_dir().join(format!("xt-vault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut v = Vault::open(&dir.join("vault.db")).unwrap();

        // Two accounts see overlapping waypoints.
        let r1 = v
            .sync(
                &[
                    batch(
                        "Multiplayer_2b2t",
                        "acct-A",
                        vec![wp("base", 100, 200), wp("stash", -5, 9)],
                    ),
                    batch(
                        "Multiplayer_2b2t",
                        "acct-B",
                        vec![wp("base", 100, 200), wp("hunt7", 7, 7)],
                    ),
                ],
                1000,
            )
            .unwrap();
        assert_eq!(r1.added, 3, "duplicate across accounts collapses");
        assert_eq!(r1.total, 3);
        assert_eq!(r1.archived_total, 0);

        // Player deletes "stash" in game on every account: next sync archives
        // it but the vault keeps the row.
        let r2 = v
            .sync(
                &[batch(
                    "Multiplayer_2b2t",
                    "acct-A",
                    vec![wp("base", 100, 200), wp("hunt7", 7, 7)],
                )],
                2000,
            )
            .unwrap();
        assert_eq!(r2.newly_archived, 1);
        assert_eq!(r2.total, 3);
        assert_eq!(r2.archived_total, 1);
        let archived = v.waypoints_for_world("Multiplayer_2b2t", true).unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].name, "stash");
        assert_eq!(archived[0].first_seen, 1000);

        // Re-adding it in game revives the same row.
        let r3 = v
            .sync(
                &[batch(
                    "Multiplayer_2b2t",
                    "acct-A",
                    vec![wp("stash", -5, 9)],
                )],
                3000,
            )
            .unwrap();
        assert_eq!(r3.revived, 1);
        assert_eq!(r3.total, 3);
        // "base"/"hunt7" untouched this run only because their group WAS
        // scanned — they get archived (absent from the scanned file).
        assert_eq!(r3.newly_archived, 2);

        // Export restores archived waypoints in game format.
        let text = v
            .export_file(
                "Multiplayer_2b2t",
                "minecraft:the_nether",
                "mw$default_1.txt",
                true,
            )
            .unwrap();
        assert!(text.contains("waypoint:base:W:100:64:200:5:false:0:gui.xaero_default:"));
        assert!(text.contains("waypoint:stash:"));
        assert!(text.contains("waypoint:hunt7:"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
