//! xaero-db — read and merge XaeroPlus SQLite databases.
//!
//! Chunk-highlight DBs (`XaeroPlusNewChunks.db`, `...OldChunks.db`, …) hold
//! one table per dimension with `(x, z, foundTime)` rows, x/z in CHUNK
//! coordinates. `foundTime` is NOT always a time: it is the raw `long` of the
//! mod's in-memory map, and `XaeroPlusLavaColumns.db` stores a lava-column
//! HEIGHT in it (see [`HighlightSemantics`]). Known schema versions:
//!   v0: tables named "0"/"-1"/"1", no metadata table
//!   v1: resource-key tables + `unique_xz_*` indexes + metadata(id,version)
//!   v2: resource-key tables WITHOUT ROWID, PRIMARY KEY (x,z)
//! `XaeroPlusDrawing.db` uses a different shape and lives in [`drawing`].

pub mod drawing;
pub mod merge;
pub mod pearls;
pub mod vault;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};

pub struct HighlightDb {
    pub conn: Connection,
    pub version: u32,
    /// Shape of the `metadata` table — distinguishes a highlight DB from a
    /// drawing DB without string-matching the filename.
    pub metadata: MetadataShape,
    /// Dimension tables present (raw table names, e.g. "minecraft:overworld").
    pub tables: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHighlight {
    pub x: i32,
    pub z: i32,
    pub found_time: i64,
}

/// Layout of the `metadata` table, which differs between the two XaeroPlus DB
/// families and decides how the schema version must be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataShape {
    /// No `metadata` table — a highlight DB still on schema v0.
    Absent,
    /// `metadata (id INTEGER PRIMARY KEY, version INTEGER)`: a single row
    /// holding the current highlight-DB schema version.
    Highlight,
    /// `metadata (version INTEGER PRIMARY KEY, time DATETIME …)`: one row per
    /// APPLIED MIGRATION, so the schema version is `MAX(version)` — reading
    /// the first row reports a v1 drawing DB as v0.
    Drawing,
}

/// What the `foundTime` column of a highlight DB actually means.
///
/// Every module but `LavaColumns` calls the mod's 3-argument `addHighlight`,
/// which stores `System.currentTimeMillis()`. `LavaColumns` calls the
/// 4-argument form with `maxHeight` — the tallest flowing-lava column in the
/// chunk, in blocks (observed range 0..=123 on the real archive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighlightSemantics {
    /// Epoch-ms of first sighting; oldest wins on merge.
    Timestamp,
    /// Lava-column height in blocks; tallest wins on merge.
    ColumnHeight,
}

impl HighlightSemantics {
    /// True when a bigger value is the more informative one (heights), false
    /// when a smaller one is (first-seen timestamps).
    pub fn prefers_max(self) -> bool {
        matches!(self, HighlightSemantics::ColumnHeight)
    }
}

/// The per-DB overlay description: colour, label and value semantics.
///
/// The colours are distinct per detection method — `NewChunks` (liquid-flow)
/// and `PaletteNewChunks` (blockstate palette) used to share one red, and the
/// two `*Inverse` overlays one teal, which made two genuinely different
/// detections indistinguishable on the map.
pub struct HighlightDbInfo {
    /// Substring matched against the DB file name (first match wins).
    pub pattern: &'static str,
    /// Short human label for the overlay control.
    pub label: &'static str,
    /// One-line description of what the module detects.
    pub detection: &'static str,
    /// Overlay colour, RGB.
    pub color: [u8; 3],
    pub semantics: HighlightSemantics,
}

/// Overlay palette, checked in order: the first `pattern` that the DB file name
/// contains wins (so `NewChunksLiquidInverse` must precede `NewChunks`).
pub const HL_PALETTE: &[HighlightDbInfo] = &[
    HighlightDbInfo {
        pattern: "NewChunksLiquidInverse",
        label: "New chunks (liquid, inverse)",
        detection: "chunks proven NOT new: liquid already flowing at load",
        color: [0x2a, 0xb5, 0xa0],
        semantics: HighlightSemantics::Timestamp,
    },
    HighlightDbInfo {
        pattern: "PaletteNewChunksInverse",
        label: "New chunks (palette, inverse)",
        detection: "chunks proven NOT new: blockstate palette already compacted",
        color: [0x1e, 0x88, 0xe5],
        semantics: HighlightSemantics::Timestamp,
    },
    HighlightDbInfo {
        pattern: "PaletteNewChunks",
        label: "New chunks (palette)",
        detection: "uncompacted blockstate/biome palette",
        color: [0xff, 0x7a, 0x00],
        semantics: HighlightSemantics::Timestamp,
    },
    HighlightDbInfo {
        pattern: "NewChunks",
        label: "New chunks (liquid)",
        detection: "liquid flow starting after chunk load",
        color: [0xff, 0x3b, 0x30],
        semantics: HighlightSemantics::Timestamp,
    },
    HighlightDbInfo {
        pattern: "OldChunks",
        label: "Old chunks",
        detection: "pre-1.13 terrain signature",
        color: [0xe6, 0xc2, 0x29],
        semantics: HighlightSemantics::Timestamp,
    },
    HighlightDbInfo {
        pattern: "ModernChunks",
        label: "Modern chunks",
        detection: "post-1.13 terrain signature",
        color: [0x58, 0xd0, 0x5b],
        semantics: HighlightSemantics::Timestamp,
    },
    HighlightDbInfo {
        pattern: "Portals",
        label: "Portals",
        detection: "nether portal blocks",
        color: [0xc6, 0x78, 0xdd],
        semantics: HighlightSemantics::Timestamp,
    },
    HighlightDbInfo {
        pattern: "LavaColumns",
        label: "Lava columns",
        detection: "tallest flowing-lava column in the chunk (height, not time)",
        color: [0xff, 0x8c, 0x1a],
        semantics: HighlightSemantics::ColumnHeight,
    },
    HighlightDbInfo {
        pattern: "OldBiomes",
        label: "Old biomes",
        detection: "pre-1.18 biome layout",
        color: [0x3d, 0x9d, 0xf2],
        semantics: HighlightSemantics::Timestamp,
    },
    HighlightDbInfo {
        pattern: "Breadcrumbs",
        label: "Breadcrumbs",
        detection: "chunks the player walked through",
        color: [0xee, 0xee, 0xee],
        semantics: HighlightSemantics::Timestamp,
    },
];

/// The palette entry for a DB file name, if it is a known highlight DB.
pub fn highlight_db_info(db_name: &str) -> Option<&'static HighlightDbInfo> {
    HL_PALETTE.iter().find(|i| db_name.contains(i.pattern))
}

/// What `foundTime` means in `db_name`. Unknown DBs are assumed to be
/// timestamps, which is the shape 9 of the 10 known modules use.
pub fn highlight_semantics(db_name: &str) -> HighlightSemantics {
    highlight_db_info(db_name)
        .map(|i| i.semantics)
        .unwrap_or(HighlightSemantics::Timestamp)
}

/// The mod's LavaColumns render rule (`LavaColumns.colorFunction`).
#[derive(Debug, Clone, Copy)]
pub struct LavaColumnStyle {
    /// Columns shorter than this are not drawn at all (mod default 5). On the
    /// real archive that hides 92% of the rows — most nether chunks carry a
    /// height-0 row simply because they were loaded.
    pub min_height: i64,
    /// Alpha = clamp(shift + height * step, 0, 255).
    pub alpha_shift: i32,
    pub alpha_step: i32,
}

impl Default for LavaColumnStyle {
    fn default() -> Self {
        LavaColumnStyle {
            min_height: 5,
            alpha_shift: 0,
            alpha_step: 8,
        }
    }
}

impl LavaColumnStyle {
    /// Alpha for a column height, or `None` when the mod would hide it.
    pub fn alpha(&self, height: i64) -> Option<u8> {
        if height < self.min_height {
            return None;
        }
        let a = self.alpha_shift as i64 + height * self.alpha_step as i64;
        Some(a.clamp(0, 255) as u8)
    }
}

/// Default cap on rows one tile query may consume before it gives up.
///
/// These are runaway guards, not a UX budget: memory is `O(cells^2)` whatever
/// they are, so they only need to sit clear of legitimately large work. The
/// biggest real tile measured is `XaeroPlusModernChunks.db` (6.0 GB), overworld
/// z=-16 tile (0,0): **38.5M rows, ~10 s warm** — a 40M/10 s guard cut that
/// tile off mid-scan and silently dropped its high-x slab. A caller that wants
/// a responsive deadline should set its own with [`TileQuery::with_time_limit`]
/// and must then treat [`HighlightGrid::truncated`] as "do not cache this".
pub const TILE_ROW_CAP: u64 = 200_000_000;
/// Default wall-clock budget for one tile query. See [`TILE_ROW_CAP`].
pub const TILE_TIME_LIMIT: Duration = Duration::from_secs(45);

/// Parameters of a single highlight tile: the half-open chunk window
/// `[cx0, cx0+span) x [cz0, cz0+span)` rendered into a `tile_size` px square.
#[derive(Debug, Clone)]
pub struct TileQuery {
    pub cx0: i64,
    pub cz0: i64,
    /// Chunks per tile axis.
    pub span: i64,
    /// Output tile edge in pixels.
    pub tile_size: usize,
    pub semantics: HighlightSemantics,
    /// Pushed into SQL as `foundTime >= min_value`; used for the LavaColumns
    /// minimum height. Forces the value column to be read.
    pub min_value: Option<i64>,
    pub row_cap: u64,
    pub time_limit: Duration,
}

impl TileQuery {
    /// A tile query with the default caps and the semantics of `db_name`;
    /// LavaColumns additionally gets the mod's default minimum height.
    pub fn new(db_name: &str, cx0: i64, cz0: i64, span: i64, tile_size: usize) -> TileQuery {
        let semantics = highlight_semantics(db_name);
        TileQuery {
            cx0,
            cz0,
            span,
            tile_size,
            semantics,
            min_value: match semantics {
                HighlightSemantics::ColumnHeight => Some(LavaColumnStyle::default().min_height),
                HighlightSemantics::Timestamp => None,
            },
            row_cap: TILE_ROW_CAP,
            time_limit: TILE_TIME_LIMIT,
        }
    }

    pub fn with_min_value(mut self, min_value: Option<i64>) -> TileQuery {
        self.min_value = min_value;
        self
    }

    pub fn with_row_cap(mut self, row_cap: u64) -> TileQuery {
        self.row_cap = row_cap;
        self
    }

    pub fn with_time_limit(mut self, time_limit: Duration) -> TileQuery {
        self.time_limit = time_limit;
        self
    }
}

/// One tile's highlights aggregated per output cell. Rows are folded into the
/// grid as they are stepped, so memory is `O(cells^2)` no matter how many rows
/// the window covers (a z=-16 tile over the real archive spans tens of
/// millions of them).
#[derive(Debug, Clone)]
pub struct HighlightGrid {
    /// Grid edge in cells: `min(tile_size, span)`.
    pub cells: usize,
    /// Output pixels per cell edge, >= 1 (`tile_size / cells`).
    pub cell_px: usize,
    /// Rows landing in each cell, row-major, `cells * cells` long.
    pub counts: Vec<u32>,
    /// Representative `foundTime` per cell — oldest under
    /// [`HighlightSemantics::Timestamp`], tallest under `ColumnHeight`.
    /// `None` when the value column was not read (the common timestamp case,
    /// where skipping it makes the scan index-only and ~3x cheaper).
    pub values: Option<Vec<i64>>,
    /// Rows actually folded in.
    pub rows: u64,
    /// The row cap or the time limit stopped the scan early, so the grid is a
    /// partial picture of the window. The rows lost are the tail of an index
    /// scan, i.e. a contiguous high-x slab of the tile rather than a thinning
    /// of it — and a time-based cut is not reproducible, so a truncated grid
    /// must never be cached as if it were the finished tile.
    pub truncated: bool,
}

impl HighlightGrid {
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Rows in cell (`cx`, `cz`).
    pub fn count_at(&self, cx: usize, cz: usize) -> u32 {
        if cx >= self.cells || cz >= self.cells {
            return 0;
        }
        self.counts[cz * self.cells + cx]
    }

    /// Representative value of cell (`cx`, `cz`), if the cell is non-empty and
    /// values were read.
    pub fn value_at(&self, cx: usize, cz: usize) -> Option<i64> {
        if self.count_at(cx, cz) == 0 {
            return None;
        }
        self.values.as_ref().map(|v| v[cz * self.cells + cx])
    }
}

/// Opens a highlight DB strictly read-only (safe while the game is running:
/// WAL readers don't block the writer; we never checkpoint).
pub fn open_readonly(path: &Path) -> Result<HighlightDb, String> {
    let conn = open_conn_readonly(path)?;
    let (version, metadata, tables) = inspect(&conn)?;
    Ok(HighlightDb {
        conn,
        version,
        metadata,
        tables,
    })
}

/// Read-only connection with the pragmas every reader in this crate wants.
pub(crate) fn open_conn_readonly(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("open {}: {e}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))
        .map_err(|e| e.to_string())?;
    let _ = conn.pragma_update(None, "query_only", 1);
    Ok(conn)
}

pub(crate) fn inspect(conn: &Connection) -> Result<(u32, MetadataShape, Vec<String>), String> {
    let mut tables = Vec::new();
    let mut has_metadata = false;
    {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            )
            .map_err(|e| e.to_string())?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        for name in names.flatten() {
            if name == "metadata" {
                has_metadata = true;
            } else {
                tables.push(name);
            }
        }
    }
    tables.sort();
    if !has_metadata {
        return Ok((0, MetadataShape::Absent, tables));
    }
    // Highlight DBs key metadata on `id` and keep one row; drawing DBs key it
    // on `version` and append a row per migration. MAX(version) is right for
    // both, but only the shape says which family this file belongs to.
    let mut has_id = false;
    {
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info('metadata')")
            .map_err(|e| e.to_string())?;
        let cols = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        for c in cols.flatten() {
            if c == "id" {
                has_id = true;
            }
        }
    }
    let shape = if has_id {
        MetadataShape::Highlight
    } else {
        MetadataShape::Drawing
    };
    let version = conn
        .query_row("SELECT MAX(version) FROM metadata", [], |r| {
            r.get::<_, Option<u32>>(0)
        })
        .ok()
        .flatten()
        .unwrap_or(if shape == MetadataShape::Highlight {
            1
        } else {
            0
        });
    Ok((version, shape, tables))
}

impl HighlightDb {
    /// True when `table` looks like a chunk-highlight dimension table.
    pub fn is_highlight_table(&self, table: &str) -> bool {
        self.conn
            .prepare(&format!(
                "SELECT x, z, foundTime FROM {} LIMIT 0",
                quote_ident(table)
            ))
            .is_ok()
    }

    pub fn count(&self, table: &str) -> Result<u64, String> {
        self.conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {}", quote_ident(table)),
                [],
                |r| r.get::<_, u64>(0),
            )
            .map_err(|e| e.to_string())
    }

    /// Row count of every dimension table, so callers can tell an overlay that
    /// is legitimately empty here from one that failed to load. SQLite serves
    /// this from the covering `unique_xz_*` index where v1 left one behind
    /// (~0.2 s for 17.7M rows, ~1.5 s for 106M), but it is still a full b-tree
    /// walk per table: the real archive's `XaeroPlusModernChunks.db` is 191.7M
    /// rows across three dimensions and takes ~12 s warm. Cache it on the file
    /// mtime and never call it on a request path.
    pub fn dimension_counts(&self) -> Result<Vec<(String, u64)>, String> {
        let mut out = Vec::with_capacity(self.tables.len());
        for t in &self.tables {
            out.push((t.clone(), self.count(t)?));
        }
        Ok(out)
    }

    /// Rows in the half-open chunk-coordinate window [x0,x1) x [z0,z1).
    ///
    /// Materializes every row — only safe for windows known to be small. Tile
    /// renderers must use [`HighlightDb::tile_grid`], which is bounded.
    pub fn query_window(
        &self,
        table: &str,
        x0: i64,
        x1: i64,
        z0: i64,
        z1: i64,
    ) -> Result<Vec<ChunkHighlight>, String> {
        let sql = format!(
            "SELECT x, z, foundTime FROM {} WHERE x >= ?1 AND x < ?2 AND z >= ?3 AND z < ?4",
            quote_ident(table)
        );
        let mut stmt = self.conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([x0, x1, z0, z1], |r| {
                Ok(ChunkHighlight {
                    x: r.get(0)?,
                    z: r.get(1)?,
                    found_time: r.get(2)?,
                })
            })
            .map_err(|e| e.to_string())?;
        Ok(rows.flatten().collect())
    }

    /// Aggregates one tile's window into a [`HighlightGrid`].
    ///
    /// The rows are streamed and folded per output cell rather than collected,
    /// and the scan gives up at `q.row_cap` rows or `q.time_limit`, so a tile
    /// covering the whole world costs bounded memory and bounded time. The
    /// value column is read only when it is actually needed, which keeps the
    /// common case on the covering `unique_xz_*` index.
    pub fn tile_grid(&self, table: &str, q: &TileQuery) -> Result<HighlightGrid, String> {
        let span = q.span.max(1);
        let tile_size = q.tile_size.max(1);
        let cells = (tile_size as i64).min(span) as usize;
        let cell_px = (tile_size / cells).max(1);
        let need_value = q.semantics.prefers_max() || q.min_value.is_some();

        let mut grid = HighlightGrid {
            cells,
            cell_px,
            counts: vec![0u32; cells * cells],
            values: need_value.then(|| vec![0i64; cells * cells]),
            rows: 0,
            truncated: false,
        };

        let cols = if need_value {
            "x, z, foundTime"
        } else {
            "x, z"
        };
        let mut sql = format!(
            "SELECT {cols} FROM {} WHERE x >= ?1 AND x < ?2 AND z >= ?3 AND z < ?4",
            quote_ident(table)
        );
        if q.min_value.is_some() {
            sql.push_str(" AND foundTime >= ?5");
        }

        let guard = DeadlineGuard::install(&self.conn, q.time_limit);
        let mut stmt = self.conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
        let x1 = q.cx0.saturating_add(span);
        let z1 = q.cz0.saturating_add(span);
        let mut rows = match q.min_value {
            Some(v) => stmt.query(rusqlite::params![q.cx0, x1, q.cz0, z1, v]),
            None => stmt.query(rusqlite::params![q.cx0, x1, q.cz0, z1]),
        }
        .map_err(|e| e.to_string())?;

        let prefers_max = q.semantics.prefers_max();
        loop {
            let row = match rows.next() {
                Ok(Some(row)) => row,
                Ok(None) => break,
                Err(e) => {
                    // The deadline handler aborts the statement with
                    // SQLITE_INTERRUPT; anything else is a real failure.
                    if guard.fired() && is_interrupt(&e) {
                        grid.truncated = true;
                        break;
                    }
                    return Err(e.to_string());
                }
            };
            // Checked with a row in hand, so a result that exactly fills the
            // cap is reported complete rather than falsely truncated.
            if grid.rows >= q.row_cap {
                grid.truncated = true;
                break;
            }
            let x: i64 = row.get(0).map_err(|e| e.to_string())?;
            let z: i64 = row.get(1).map_err(|e| e.to_string())?;
            let cx = (((x - q.cx0) * cells as i64) / span) as usize;
            let cz = (((z - q.cz0) * cells as i64) / span) as usize;
            if cx >= cells || cz >= cells {
                continue;
            }
            let i = cz * cells + cx;
            let first = grid.counts[i] == 0;
            grid.counts[i] = grid.counts[i].saturating_add(1);
            grid.rows += 1;
            if let Some(values) = grid.values.as_mut() {
                let v: i64 = row.get(2).map_err(|e| e.to_string())?;
                values[i] = if first {
                    v
                } else if prefers_max {
                    values[i].max(v)
                } else {
                    values[i].min(v)
                };
            }
        }
        Ok(grid)
    }

    /// The table holding `dimension_key` rows, resolving the v0 numeric names.
    pub fn table_for_dimension(&self, dimension_key: &str) -> Option<String> {
        if self.tables.iter().any(|t| t == dimension_key) {
            return Some(dimension_key.to_string());
        }
        let legacy = match dimension_key {
            "minecraft:overworld" => "0",
            "minecraft:the_nether" => "-1",
            "minecraft:the_end" => "1",
            _ => return None,
        };
        self.tables.iter().find(|t| t.as_str() == legacy).cloned()
    }
}

/// Installs a SQLite progress handler that interrupts the connection once the
/// deadline passes, and clears it again on drop (handlers are per-connection
/// and would otherwise leak into unrelated queries).
struct DeadlineGuard<'a> {
    conn: &'a Connection,
    fired: Arc<AtomicBool>,
}

impl<'a> DeadlineGuard<'a> {
    fn install(conn: &'a Connection, limit: Duration) -> DeadlineGuard<'a> {
        let fired = Arc::new(AtomicBool::new(false));
        // `Instant + Duration` panics on overflow; an absurd budget is "no
        // deadline", not a crash.
        let deadline = Instant::now().checked_add(limit);
        let flag = fired.clone();
        // ~20k VM instructions between checks: cheap relative to a page read.
        conn.progress_handler(
            20_000,
            Some(move || match deadline {
                Some(d) if Instant::now() >= d => {
                    flag.store(true, Ordering::Relaxed);
                    true
                }
                _ => false,
            }),
        );
        DeadlineGuard { conn, fired }
    }

    fn fired(&self) -> bool {
        self.fired.load(Ordering::Relaxed)
    }
}

impl Drop for DeadlineGuard<'_> {
    fn drop(&mut self) {
        self.conn.progress_handler(0, None::<fn() -> bool>);
    }
}

/// True only for the `SQLITE_INTERRUPT` a fired [`DeadlineGuard`] raises, so a
/// real failure arriving after the deadline is still reported as one.
fn is_interrupt(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.code == rusqlite::ErrorCode::OperationInterrupted
    )
}

/// Quotes an SQL identifier ("" doubling) — table names contain ':' and
/// arbitrary resource-key characters.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    #[ignore = "requires corpus (XAERO_CORPUS)"]
    fn reads_sample_newchunks() {
        let root = test_support::corpus_root().expect("XAERO_CORPUS");
        let db = open_readonly(
            &root.join("xaero1.21.8/world-map/Multiplayer_2b2t/XaeroPlusNewChunks.db"),
        )
        .unwrap();
        assert_eq!(db.version, 1);
        assert_eq!(db.metadata, MetadataShape::Highlight);
        assert!(db.tables.iter().any(|t| t == "minecraft:the_nether"));
        let table = db.table_for_dimension("minecraft:the_nether").unwrap();
        assert_eq!(db.count(&table).unwrap(), 1086);
        // Window around the known sample rows (6096, 218753).
        let rows = db.query_window(&table, 6000, 6200, 218000, 219000).unwrap();
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|h| h.found_time > 1_600_000_000_000));
    }

    #[test]
    #[ignore = "requires corpus (XAERO_CORPUS)"]
    fn reads_all_sample_dbs() {
        let root = test_support::corpus_root().expect("XAERO_CORPUS");
        let mut checked = 0;
        for version_dir in ["xaero1.21.4", "xaero1.21.8"] {
            let wm = root.join(version_dir).join("world-map");
            let Ok(worlds) = std::fs::read_dir(&wm) else {
                continue;
            };
            for world in worlds.flatten() {
                let Ok(files) = std::fs::read_dir(world.path()) else {
                    continue;
                };
                for f in files.flatten() {
                    let name = f.file_name().to_string_lossy().to_string();
                    if !name.ends_with(".db") || name.contains("Drawing") {
                        continue;
                    }
                    let db = open_readonly(&f.path()).unwrap();
                    for t in db.tables.clone() {
                        assert!(db.is_highlight_table(&t), "{name} {t}");
                        let _ = db.count(&t).unwrap();
                    }
                    checked += 1;
                }
            }
        }
        eprintln!("checked {checked} highlight DBs");
        assert!(checked >= 15);
    }

    /// A v0 highlight DB, matching XaeroPlus's V0ToV1Migration: numeric table
    /// names and the unique_xzO/N/E indexes, no metadata table. No such file
    /// exists in the corpus or the real archive — the shape is taken from the
    /// mod source, so the fallback path needs a synthetic fixture to be
    /// covered at all.
    pub(crate) fn write_v0_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE \"0\" (x INTEGER, z INTEGER, foundTime INTEGER);
             CREATE UNIQUE INDEX unique_xzO ON \"0\" (x, z);
             CREATE TABLE \"-1\" (x INTEGER, z INTEGER, foundTime INTEGER);
             CREATE UNIQUE INDEX unique_xzN ON \"-1\" (x, z);
             CREATE TABLE \"1\" (x INTEGER, z INTEGER, foundTime INTEGER);
             CREATE UNIQUE INDEX unique_xzE ON \"1\" (x, z);
             INSERT INTO \"0\" VALUES (-3, -3, 1000), (5, 7, 2000);
             INSERT INTO \"-1\" VALUES (9, 9, 42);",
        )
        .unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xt-db-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn v0_numeric_tables_resolve() {
        let dir = scratch("v0");
        let p = dir.join("XaeroPlusNewChunks.db");
        write_v0_db(&p);
        let db = open_readonly(&p).unwrap();
        assert_eq!(db.version, 0);
        assert_eq!(db.metadata, MetadataShape::Absent);
        assert_eq!(
            db.table_for_dimension("minecraft:overworld").as_deref(),
            Some("0")
        );
        assert_eq!(
            db.table_for_dimension("minecraft:the_nether").as_deref(),
            Some("-1")
        );
        assert_eq!(db.table_for_dimension("minecraft:sky3"), None);
        let t = db.table_for_dimension("minecraft:overworld").unwrap();
        assert_eq!(db.count(&t).unwrap(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tile_grid_buckets_negative_coordinates() {
        let dir = scratch("grid");
        let p = dir.join("XaeroPlusNewChunks.db");
        {
            let conn = Connection::open(&p).unwrap();
            conn.execute_batch(
                "CREATE TABLE metadata (id INTEGER PRIMARY KEY, version INTEGER);
                 INSERT INTO metadata VALUES (0, 2);
                 CREATE TABLE \"minecraft:overworld\" (x INTEGER, z INTEGER, foundTime INTEGER,
                   PRIMARY KEY (x, z)) WITHOUT ROWID;
                 INSERT INTO \"minecraft:overworld\" VALUES
                   (-64, -64, 500), (-63, -64, 400), (-1, -1, 900), (0, 0, 100), (63, 63, 700);",
            )
            .unwrap();
        }
        let db = open_readonly(&p).unwrap();
        // 128-chunk window starting at -64, 4 cells => 32 chunks per cell.
        // Cell 0 covers [-64,-32), cell 1 [-32,0), cell 2 [0,32), cell 3 [32,64).
        let q = TileQuery::new("XaeroPlusNewChunks.db", -64, -64, 128, 4);
        let g = db.tile_grid("minecraft:overworld", &q).unwrap();
        assert_eq!(g.cells, 4);
        assert_eq!(g.cell_px, 1);
        assert_eq!(g.rows, 5);
        assert!(!g.truncated);
        assert_eq!(g.count_at(0, 0), 2, "both -64/-63 rows land in cell 0");
        assert_eq!(
            g.count_at(1, 1),
            1,
            "(-1,-1) is its own cell, not merged with (0,0)"
        );
        assert_eq!(g.count_at(2, 2), 1);
        assert_eq!(g.count_at(3, 3), 1);
        assert!(g.values.is_none(), "timestamps skip the value column");

        // Zoomed in: fewer chunks than pixels, so cells == span and each cell
        // paints cell_px pixels.
        let q = TileQuery::new("XaeroPlusNewChunks.db", -64, -64, 32, 512);
        let g = db.tile_grid("minecraft:overworld", &q).unwrap();
        assert_eq!(g.cells, 32);
        assert_eq!(g.cell_px, 16);
        assert_eq!(g.rows, 2);

        // Row cap truncates instead of running away.
        let q = TileQuery::new("XaeroPlusNewChunks.db", -64, -64, 128, 4).with_row_cap(2);
        let g = db.tile_grid("minecraft:overworld", &q).unwrap();
        assert!(g.truncated);
        assert_eq!(g.rows, 2);

        // A window that exactly fills the cap is complete, not truncated —
        // otherwise a cap-sized tile is cached as a partial one forever.
        let q = TileQuery::new("XaeroPlusNewChunks.db", -64, -64, 128, 4).with_row_cap(5);
        let g = db.tile_grid("minecraft:overworld", &q).unwrap();
        assert_eq!(g.rows, 5);
        assert!(!g.truncated, "5 rows under a cap of 5 is a complete scan");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lava_columns_are_heights_not_times() {
        assert_eq!(
            highlight_semantics("XaeroPlusLavaColumns.db"),
            HighlightSemantics::ColumnHeight
        );
        assert_eq!(
            highlight_semantics("XaeroPlusNewChunks.db"),
            HighlightSemantics::Timestamp
        );
        let s = LavaColumnStyle::default();
        assert_eq!(s.alpha(0), None);
        assert_eq!(s.alpha(4), None);
        assert_eq!(s.alpha(5), Some(40));
        assert_eq!(s.alpha(32), Some(255));
        assert_eq!(s.alpha(123), Some(255));

        let dir = scratch("lava");
        let p = dir.join("XaeroPlusLavaColumns.db");
        {
            let conn = Connection::open(&p).unwrap();
            conn.execute_batch(
                "CREATE TABLE metadata (id INTEGER PRIMARY KEY, version INTEGER);
                 INSERT INTO metadata VALUES (0, 2);
                 CREATE TABLE \"minecraft:the_nether\" (x INTEGER, z INTEGER, foundTime INTEGER,
                   PRIMARY KEY (x, z)) WITHOUT ROWID;
                 INSERT INTO \"minecraft:the_nether\" VALUES
                   (0, 0, 0), (1, 0, 3), (2, 0, 7), (3, 0, 40);",
            )
            .unwrap();
        }
        let db = open_readonly(&p).unwrap();
        // Default min height 5 hides the 0 and 3 rows in SQL.
        let q = TileQuery::new("XaeroPlusLavaColumns.db", 0, 0, 4, 4);
        let g = db.tile_grid("minecraft:the_nether", &q).unwrap();
        assert_eq!(g.rows, 2);
        assert_eq!(g.value_at(2, 0), Some(7));
        assert_eq!(g.value_at(3, 0), Some(40));
        assert_eq!(g.value_at(0, 0), None);
        // Coarser grid: the tallest column in the cell wins, not the shortest.
        let q = TileQuery::new("XaeroPlusLavaColumns.db", 0, 0, 4, 1);
        let g = db.tile_grid("minecraft:the_nether", &q).unwrap();
        assert_eq!(g.value_at(0, 0), Some(40));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tile_grid_deadline_interrupts_the_scan() {
        let dir = scratch("deadline");
        let p = dir.join("XaeroPlusNewChunks.db");
        {
            let conn = Connection::open(&p).unwrap();
            conn.execute_batch(
                "CREATE TABLE \"minecraft:overworld\" (x INTEGER, z INTEGER, foundTime INTEGER,
                   PRIMARY KEY (x, z)) WITHOUT ROWID;
                 WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i+1 FROM n WHERE i < 199999)
                 INSERT INTO \"minecraft:overworld\" SELECT i % 512, i / 512, i FROM n;",
            )
            .unwrap();
        }
        let db = open_readonly(&p).unwrap();
        let q =
            TileQuery::new("XaeroPlusNewChunks.db", 0, 0, 512, 512).with_time_limit(Duration::ZERO);
        let g = db.tile_grid("minecraft:overworld", &q).unwrap();
        assert!(g.truncated, "an expired budget must abort the scan");
        assert!(g.rows < 200_000);
        // The handler is per-connection: the next query must not inherit it.
        let q = TileQuery::new("XaeroPlusNewChunks.db", 0, 0, 512, 512);
        let g = db.tile_grid("minecraft:overworld", &q).unwrap();
        assert!(!g.truncated);
        assert_eq!(g.rows, 200_000);
        assert_eq!(
            db.dimension_counts().unwrap(),
            vec![("minecraft:overworld".to_string(), 200_000u64)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_palette_entry_has_a_distinct_color() {
        for (i, a) in HL_PALETTE.iter().enumerate() {
            for b in HL_PALETTE.iter().skip(i + 1) {
                assert_ne!(
                    a.color, b.color,
                    "{} and {} share a color",
                    a.pattern, b.pattern
                );
            }
        }
        // Order matters: the longer patterns must be matched first.
        assert_eq!(
            highlight_db_info("XaeroPlusNewChunksLiquidInverse.db")
                .unwrap()
                .pattern,
            "NewChunksLiquidInverse"
        );
        assert_eq!(
            highlight_db_info("XaeroPlusPaletteNewChunks.db")
                .unwrap()
                .pattern,
            "PaletteNewChunks"
        );
    }
}
