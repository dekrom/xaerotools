//! `XaeroPlusDrawing.db` — the mod's hand-drawn annotation layer.
//!
//! Shape (XaeroPlus `feature/drawing/db/DrawingDatabase.java`): four table
//! families per dimension, named `"<dimension resource key>-<family>"`.
//!
//! ```text
//! "<dim>-highlights" (x, z, color)                     CHUNK coords
//! "<dim>-lines"      (x1, z1, x2, z2, color)           BLOCK coords
//! "<dim>-texts"      (value, x, z, color, scale)       BLOCK coords
//! "<dim>-ellipses"   (centerX, centerZ, radiusX, radiusZ, color)  BLOCK
//! metadata (version INTEGER PRIMARY KEY, time DATETIME …)
//! ```
//!
//! `color` is a signed 32-bit ARGB int. `metadata` holds one row per applied
//! migration, so the schema version is `MAX(version)`: v0 has highlights,
//! lines and texts, v1 adds ellipses — see [`crate::MetadataShape`]. Tables
//! are created lazily per dimension actually visited, so a drawing DB
//! routinely lacks whole families, including for custom dimensions.

use std::path::Path;

use rusqlite::Connection;

use crate::merge::TableMergeReport;
use crate::{inspect, open_conn_readonly, quote_ident, MetadataShape};

/// True for a file name that is a XaeroPlus drawing database.
pub fn is_drawing_db(db_name: &str) -> bool {
    db_name.contains("Drawing")
}

/// One of the four drawing table families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawingFamily {
    Highlights,
    Lines,
    Texts,
    Ellipses,
}

impl DrawingFamily {
    pub const ALL: [DrawingFamily; 4] = [
        DrawingFamily::Highlights,
        DrawingFamily::Lines,
        DrawingFamily::Texts,
        DrawingFamily::Ellipses,
    ];

    /// The table-name suffix, including the separating '-'.
    pub fn suffix(self) -> &'static str {
        match self {
            DrawingFamily::Highlights => "-highlights",
            DrawingFamily::Lines => "-lines",
            DrawingFamily::Texts => "-texts",
            DrawingFamily::Ellipses => "-ellipses",
        }
    }

    fn from_table(table: &str) -> Option<(&str, DrawingFamily)> {
        DrawingFamily::ALL
            .iter()
            .find_map(|f| table.strip_suffix(f.suffix()).map(|dim| (dim, *f)))
    }
}

/// A chunk-sized filled square. x/z are CHUNK coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DrawingHighlight {
    pub x: i32,
    pub z: i32,
    /// ARGB, as stored (signed in SQLite, reinterpreted unsigned here).
    pub color: u32,
}

/// A segment between two BLOCK positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DrawingLine {
    pub x1: i32,
    pub z1: i32,
    pub x2: i32,
    pub z2: i32,
    pub color: u32,
}

/// A label anchored at a BLOCK position. `scale` is the mod's render scale.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DrawingText {
    pub value: String,
    pub x: i32,
    pub z: i32,
    pub color: u32,
    pub scale: f32,
}

/// An axis-aligned ellipse in BLOCK coordinates (v1 drawing DBs only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DrawingEllipse {
    pub center_x: i32,
    pub center_z: i32,
    pub radius_x: i32,
    pub radius_z: i32,
    pub color: u32,
}

/// Everything drawn in one dimension — small enough to serve whole (the real
/// 2b2t archive holds 5,746 highlights and 14 lines in total).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DimensionDrawings {
    pub dimension: String,
    pub highlights: Vec<DrawingHighlight>,
    pub lines: Vec<DrawingLine>,
    pub texts: Vec<DrawingText>,
    pub ellipses: Vec<DrawingEllipse>,
}

impl DimensionDrawings {
    pub fn is_empty(&self) -> bool {
        self.highlights.is_empty()
            && self.lines.is_empty()
            && self.texts.is_empty()
            && self.ellipses.is_empty()
    }
}

pub struct DrawingDb {
    pub conn: Connection,
    /// `MAX(version)` of the migration ledger: 0 = no ellipses, 1 = ellipses.
    pub version: u32,
    pub metadata: MetadataShape,
    /// Raw table names, e.g. "minecraft:overworld-highlights".
    pub tables: Vec<String>,
}

/// Opens a drawing DB strictly read-only (live DBs are WAL; never checkpoint).
pub fn open_readonly(path: &Path) -> Result<DrawingDb, String> {
    let conn = open_conn_readonly(path)?;
    let (version, metadata, tables) = inspect(&conn)?;
    Ok(DrawingDb {
        conn,
        version,
        metadata,
        tables,
    })
}

impl DrawingDb {
    /// Dimension resource keys that have at least one drawing table, sorted.
    pub fn dimensions(&self) -> Vec<String> {
        let mut dims: Vec<String> = self
            .tables
            .iter()
            .filter_map(|t| DrawingFamily::from_table(t).map(|(d, _)| d.to_string()))
            .collect();
        dims.sort();
        dims.dedup();
        dims
    }

    /// The table for a dimension/family pair, if the mod ever created it.
    pub fn table_for(&self, dimension_key: &str, family: DrawingFamily) -> Option<String> {
        let want = format!("{dimension_key}{}", family.suffix());
        self.tables.iter().find(|t| **t == want).cloned()
    }

    /// Row count per drawing table, so an empty overlay is distinguishable
    /// from a missing one.
    pub fn counts(&self) -> Result<Vec<(String, u64)>, String> {
        let mut out = Vec::with_capacity(self.tables.len());
        for t in &self.tables {
            let n = self
                .conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {}", quote_ident(t)),
                    [],
                    |r| r.get::<_, u64>(0),
                )
                .map_err(|e| e.to_string())?;
            out.push((t.clone(), n));
        }
        Ok(out)
    }

    /// Chunk highlights inside the half-open CHUNK window [x0,x1) x [z0,z1).
    pub fn highlights_in_window(
        &self,
        dimension_key: &str,
        x0: i64,
        x1: i64,
        z0: i64,
        z1: i64,
    ) -> Result<Vec<DrawingHighlight>, String> {
        let Some(table) = self.table_for(dimension_key, DrawingFamily::Highlights) else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "SELECT x, z, color FROM {} WHERE x >= ?1 AND x < ?2 AND z >= ?3 AND z < ?4",
            quote_ident(&table)
        );
        let mut stmt = self.conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([x0, x1, z0, z1], |r| {
                Ok(DrawingHighlight {
                    x: r.get(0)?,
                    z: r.get(1)?,
                    color: r.get::<_, i64>(2)? as u32,
                })
            })
            .map_err(|e| e.to_string())?;
        Ok(rows.flatten().collect())
    }

    /// Everything drawn in one dimension. The mod itself reads lines, texts
    /// and ellipses with a bare `SELECT *`, so there is nothing to window.
    pub fn read_dimension(&self, dimension_key: &str) -> Result<DimensionDrawings, String> {
        let mut out = DimensionDrawings {
            dimension: dimension_key.to_string(),
            ..Default::default()
        };
        if let Some(t) = self.table_for(dimension_key, DrawingFamily::Highlights) {
            let mut stmt = self
                .conn
                .prepare(&format!("SELECT x, z, color FROM {}", quote_ident(&t)))
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(DrawingHighlight {
                        x: r.get(0)?,
                        z: r.get(1)?,
                        color: r.get::<_, i64>(2)? as u32,
                    })
                })
                .map_err(|e| e.to_string())?;
            out.highlights = rows.flatten().collect();
        }
        if let Some(t) = self.table_for(dimension_key, DrawingFamily::Lines) {
            let mut stmt = self
                .conn
                .prepare(&format!(
                    "SELECT x1, z1, x2, z2, color FROM {}",
                    quote_ident(&t)
                ))
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(DrawingLine {
                        x1: r.get(0)?,
                        z1: r.get(1)?,
                        x2: r.get(2)?,
                        z2: r.get(3)?,
                        color: r.get::<_, i64>(4)? as u32,
                    })
                })
                .map_err(|e| e.to_string())?;
            out.lines = rows.flatten().collect();
        }
        if let Some(t) = self.table_for(dimension_key, DrawingFamily::Texts) {
            let mut stmt = self
                .conn
                .prepare(&format!(
                    "SELECT value, x, z, color, scale FROM {}",
                    quote_ident(&t)
                ))
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(DrawingText {
                        value: r.get(0)?,
                        x: r.get(1)?,
                        z: r.get(2)?,
                        color: r.get::<_, i64>(3)? as u32,
                        scale: r.get::<_, f64>(4)? as f32,
                    })
                })
                .map_err(|e| e.to_string())?;
            out.texts = rows.flatten().collect();
        }
        if let Some(t) = self.table_for(dimension_key, DrawingFamily::Ellipses) {
            let mut stmt = self
                .conn
                .prepare(&format!(
                    "SELECT centerX, centerZ, radiusX, radiusZ, color FROM {}",
                    quote_ident(&t)
                ))
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(DrawingEllipse {
                        center_x: r.get(0)?,
                        center_z: r.get(1)?,
                        radius_x: r.get(2)?,
                        radius_z: r.get(3)?,
                        color: r.get::<_, i64>(4)? as u32,
                    })
                })
                .map_err(|e| e.to_string())?;
            out.ellipses = rows.flatten().collect();
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DrawingMergeReport {
    pub dest: String,
    pub sources: Vec<String>,
    pub tables: Vec<TableMergeReport>,
    pub applied: bool,
}

/// Merges drawing DBs into `dest` (modified in place — callers doing `-o out`
/// copy first). Sources are attached read-only and never modified.
///
/// The rule is a union on each table's declared primary key: `INSERT OR
/// IGNORE`, so the destination wins on an identical key. Drawings are
/// hand-authored, so note the consequence: a drawing erased on the newer side
/// is resurrected from the older side. There is no timestamp column to
/// arbitrate with, and the alternative (keeping only one side) is the data
/// loss this replaces.
///
/// Tables missing on the destination are created from the source's own DDL —
/// XaeroPlus creates them lazily per dimension, so whole families and whole
/// custom dimensions can be absent, not just the v1 `-ellipses` tables.
pub fn merge_into(
    dest: &Path,
    sources: &[&Path],
    apply: bool,
) -> Result<DrawingMergeReport, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let conn = if apply {
        Connection::open(dest)
    } else {
        Connection::open_with_flags(
            dest,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    }
    .map_err(|er| format!("open {}: {er}", dest.display()))?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))
        .map_err(e)?;
    if apply {
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
    }

    let mut report = DrawingMergeReport {
        dest: dest.display().to_string(),
        sources: sources.iter().map(|s| s.display().to_string()).collect(),
        applied: apply,
        ..Default::default()
    };

    for (i, source) in sources.iter().enumerate() {
        let alias = format!("src{i}");
        let uri = crate::attach_uri(source);
        conn.execute(&format!("ATTACH DATABASE ?1 AS {alias}"), [&uri])
            .map_err(|er| format!("attach {}: {er}", source.display()))?;
        let result = merge_one(&conn, &alias, apply, &mut report.tables);
        conn.execute_batch(&format!("DETACH DATABASE {alias}"))
            .map_err(e)?;
        result?;
    }

    if apply {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;");
    }
    Ok(report)
}

fn merge_one(
    conn: &Connection,
    alias: &str,
    apply: bool,
    tables: &mut Vec<TableMergeReport>,
) -> Result<(), String> {
    let e = |e: rusqlite::Error| e.to_string();

    // Every source table except the migration ledger, with its own DDL.
    let mut src_tables: Vec<(String, String)> = Vec::new();
    {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT name, COALESCE(sql,'') FROM {alias}.sqlite_master WHERE type='table' \
                 AND name NOT LIKE 'sqlite_%' AND name != 'metadata' ORDER BY name"
            ))
            .map_err(e)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(e)?;
        for row in rows.flatten() {
            src_tables.push(row);
        }
    }

    for (table, ddl) in src_tables {
        // Drawing table names are always canonical resource keys — unlike
        // highlight DBs there is no v0 numeric-name form to map.
        let qsrc = format!("{alias}.{}", quote_ident(&table));
        let qdst = quote_ident(&table);
        let source_rows: u64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {qsrc}"), [], |r| r.get(0))
            .map_err(e)?;
        let mut dest_exists = table_exists(conn, "main", &table)?;
        if apply && !dest_exists && !ddl.is_empty() {
            conn.execute_batch(&ddl).map_err(e)?;
            dest_exists = true;
        }

        let src_cols = columns(conn, alias, &table)?;
        let (dest_rows_before, overlap) = if dest_exists {
            let before = conn
                .query_row(&format!("SELECT COUNT(*) FROM {qdst}"), [], |r| r.get(0))
                .map_err(e)?;
            let keys: Vec<&str> = src_cols
                .iter()
                .filter(|c| c.pk > 0)
                .map(|c| c.name.as_str())
                .collect();
            let overlap: u64 = if keys.is_empty() {
                0
            } else {
                let on = keys
                    .iter()
                    .map(|k| format!("s.{q} = m.{q}", q = quote_ident(k)))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                conn.query_row(
                    &format!("SELECT COUNT(*) FROM {qsrc} s JOIN {qdst} m ON {on}"),
                    [],
                    |r| r.get(0),
                )
                .map_err(e)?
            };
            (before, overlap)
        } else {
            (0, 0)
        };

        let mut dest_rows_after = dest_rows_before + source_rows - overlap;
        if apply {
            // Intersect the column lists so a version-skewed table can't error.
            let dst_cols = columns(conn, "main", &table)?;
            let cols: Vec<String> = src_cols
                .iter()
                .filter(|c| dst_cols.iter().any(|d| d.name == c.name))
                .map(|c| quote_ident(&c.name))
                .collect();
            if !cols.is_empty() {
                let list = cols.join(", ");
                conn.execute_batch(&format!(
                    "INSERT OR IGNORE INTO {qdst} ({list}) SELECT {list} FROM {qsrc}"
                ))
                .map_err(e)?;
            }
            dest_rows_after = conn
                .query_row(&format!("SELECT COUNT(*) FROM {qdst}"), [], |r| r.get(0))
                .map_err(e)?;
        }

        tables.push(TableMergeReport {
            table,
            source_rows,
            dest_rows_before,
            overlap,
            dest_rows_after,
        });
    }

    // The ledger is a union too: it records which migrations have run, and the
    // destination now carries every table either side had.
    if apply && table_exists(conn, alias, "metadata")? {
        if !table_exists(conn, "main", "metadata")? {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS metadata (version INTEGER PRIMARY KEY, \
                 time DATETIME NOT NULL default CURRENT_TIMESTAMP)",
            )
            .map_err(e)?;
        }
        conn.execute_batch(&format!(
            "INSERT OR IGNORE INTO main.metadata (version, time) \
             SELECT version, time FROM {alias}.metadata"
        ))
        .map_err(e)?;
    }
    Ok(())
}

struct ColumnInfo {
    name: String,
    pk: i64,
}

fn columns(conn: &Connection, schema: &str, table: &str) -> Result<Vec<ColumnInfo>, String> {
    let mut stmt = conn
        .prepare("SELECT name, pk FROM pragma_table_info(?1, ?2)")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([table, schema], |r| {
            Ok(ColumnInfo {
                name: r.get(0)?,
                pk: r.get(1)?,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

fn table_exists(conn: &Connection, schema: &str, table: &str) -> Result<bool, String> {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM {schema}.sqlite_master WHERE type='table' AND name = ?1"),
        [table],
        |r| r.get::<_, u64>(0).map(|n| n > 0),
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("xt-draw-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A v1 drawing DB with the vanilla families plus one custom dimension.
    fn mk_drawing(path: &Path, dim: &str, seed: i32, with_ellipses: bool) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata (version INTEGER PRIMARY KEY, \
             time DATETIME NOT NULL default CURRENT_TIMESTAMP);
             INSERT INTO metadata (version) VALUES (0);",
        )
        .unwrap();
        let q = |s: &str| quote_ident(s);
        conn.execute_batch(&format!(
            "CREATE TABLE {h} (x INTEGER, z INTEGER, color INTEGER, PRIMARY KEY (x, z));
             CREATE TABLE {l} (x1 INTEGER, z1 INTEGER, x2 INTEGER, z2 INTEGER, color INTEGER, \
               PRIMARY KEY (x1, z1, x2, z2));
             CREATE TABLE {t} (value TEXT, x INTEGER, z INTEGER, color INTEGER, scale REAL, \
               PRIMARY KEY (x, z));",
            h = q(&format!("{dim}-highlights")),
            l = q(&format!("{dim}-lines")),
            t = q(&format!("{dim}-texts")),
        ))
        .unwrap();
        if with_ellipses {
            conn.execute_batch(&format!(
                "INSERT INTO metadata (version) VALUES (1);
                 CREATE TABLE {e} (centerX INTEGER, centerZ INTEGER, radiusX INTEGER, \
                   radiusZ INTEGER, color INTEGER, PRIMARY KEY (centerX, centerZ, radiusX, radiusZ));
                 INSERT INTO {e} VALUES ({seed}, {seed}, 10, 20, 1694433280);",
                e = q(&format!("{dim}-ellipses")),
            ))
            .unwrap();
        }
        conn.execute_batch(&format!(
            "INSERT INTO {h} VALUES ({seed}, {seed}, 1694433280);
             INSERT INTO {l} VALUES ({seed}, {seed}, {seed2}, {seed2}, 1694433280);
             INSERT INTO {t} VALUES ('mark{seed}', {seed}, {seed}, 1694433280, 1.5);",
            h = q(&format!("{dim}-highlights")),
            l = q(&format!("{dim}-lines")),
            t = q(&format!("{dim}-texts")),
            seed2 = seed + 100,
        ))
        .unwrap();
    }

    #[test]
    fn version_is_max_not_first_row() {
        let dir = scratch("ver");
        let p = dir.join("XaeroPlusDrawing.db");
        mk_drawing(&p, "minecraft:overworld", 5, true);
        let db = open_readonly(&p).unwrap();
        // The ledger holds rows 0 and 1; reading the first row reports v0.
        assert_eq!(db.version, 1);
        assert_eq!(db.metadata, MetadataShape::Drawing);
        assert!(db
            .table_for("minecraft:overworld", DrawingFamily::Ellipses)
            .is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_families_and_dimensions() {
        let dir = scratch("read");
        let p = dir.join("XaeroPlusDrawing.db");
        mk_drawing(&p, "minecraft:overworld", 7, true);
        let db = open_readonly(&p).unwrap();
        assert_eq!(db.dimensions(), vec!["minecraft:overworld".to_string()]);
        let d = db.read_dimension("minecraft:overworld").unwrap();
        assert!(!d.is_empty());
        assert_eq!(d.highlights.len(), 1);
        assert_eq!(d.highlights[0].x, 7);
        // 1694433280 == 0x64FF0000: alpha 100, pure red.
        assert_eq!(d.highlights[0].color, 0x64FF_0000);
        assert_eq!(d.lines[0].x2, 107);
        assert_eq!(d.texts[0].value, "mark7");
        assert_eq!(d.texts[0].scale, 1.5);
        assert_eq!(d.ellipses[0].radius_z, 20);
        // Windowed read is chunk-coordinate based.
        assert_eq!(
            db.highlights_in_window("minecraft:overworld", 0, 8, 0, 8)
                .unwrap()
                .len(),
            1
        );
        assert!(db
            .highlights_in_window("minecraft:overworld", 100, 200, 0, 8)
            .unwrap()
            .is_empty());
        assert!(db.read_dimension("minecraft:the_end").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_unions_both_sides_and_creates_missing_tables() {
        let dir = scratch("merge");
        let dest = dir.join("dest.db");
        let src = dir.join("src.db");
        // Destination: vanilla overworld only, v0 (no ellipses).
        mk_drawing(&dest, "minecraft:overworld", 1, false);
        // Source: a custom dimension the destination has never seen, v1.
        mk_drawing(&src, "minecraft:brazil", 2, true);
        {
            // …plus an overworld row the destination lacks.
            let conn = Connection::open(&src).unwrap();
            conn.execute_batch(
                "CREATE TABLE \"minecraft:overworld-highlights\" (x INTEGER, z INTEGER, \
                   color INTEGER, PRIMARY KEY (x, z));
                 INSERT INTO \"minecraft:overworld-highlights\" VALUES (1, 1, 999), (3, 3, -1);",
            )
            .unwrap();
        }

        let dry = merge_into(&dest, &[&src], false).unwrap();
        assert!(!dry.applied);
        let ow = dry
            .tables
            .iter()
            .find(|t| t.table == "minecraft:overworld-highlights")
            .unwrap();
        assert_eq!(ow.source_rows, 2);
        assert_eq!(ow.overlap, 1);
        // Dry run wrote nothing.
        let db = open_readonly(&dest).unwrap();
        assert_eq!(db.dimensions(), vec!["minecraft:overworld".to_string()]);
        drop(db);

        merge_into(&dest, &[&src], true).unwrap();
        let db = open_readonly(&dest).unwrap();
        assert_eq!(
            db.dimensions(),
            vec![
                "minecraft:brazil".to_string(),
                "minecraft:overworld".to_string()
            ]
        );
        let ow = db.read_dimension("minecraft:overworld").unwrap();
        assert_eq!(ow.highlights.len(), 2, "both sides survive the merge");
        // Destination wins on the shared key (1,1).
        let kept = ow.highlights.iter().find(|h| h.x == 1).unwrap();
        assert_eq!(kept.color, 0x64FF_0000);
        let br = db.read_dimension("minecraft:brazil").unwrap();
        assert_eq!(br.highlights.len(), 1);
        assert_eq!(br.ellipses.len(), 1, "v1 ellipse table created on the dest");
        assert_eq!(db.version, 1, "migration ledger is unioned too");
        drop(db);

        // Source untouched.
        let s = open_readonly(&src).unwrap();
        assert_eq!(s.dimensions().len(), 2);
        assert!(s
            .read_dimension("minecraft:overworld")
            .unwrap()
            .lines
            .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "requires corpus (XAERO_CORPUS)"]
    fn reads_corpus_drawing_dbs() {
        let root = test_support::corpus_root().expect("XAERO_CORPUS");
        let mut checked = 0;
        for version_dir in ["xaero1.21.4", "xaero1.21.8"] {
            let wm = root.join(version_dir).join("world-map");
            let Ok(worlds) = std::fs::read_dir(&wm) else {
                continue;
            };
            for world in worlds.flatten() {
                let p = world.path().join("XaeroPlusDrawing.db");
                if !p.is_file() {
                    continue;
                }
                let db = open_readonly(&p).unwrap();
                assert_eq!(db.metadata, MetadataShape::Drawing);
                for dim in db.dimensions() {
                    let _ = db.read_dimension(&dim).unwrap();
                }
                let _ = db.counts().unwrap();
                checked += 1;
            }
        }
        eprintln!("checked {checked} drawing DBs");
        assert!(checked >= 4);
    }
}
