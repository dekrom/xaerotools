//! Merging XaeroPlus chunk-highlight databases.
//!
//! Strategy (per plan): normalize the destination to schema v2, then per
//! dimension table `INSERT ... ON CONFLICT(x,z) DO UPDATE SET
//! foundTime = MIN(...)` — oldest-first-seen wins, preserving discovery
//! history. That rule is only correct while `foundTime` IS a time: in
//! `XaeroPlusLavaColumns.db` it is a column height, where MIN would keep the
//! shallowest column and erase the deep ones the module exists to find, so
//! those merge with MAX (see [`crate::HighlightSemantics`]).
//! Sources are never modified (attached read-only). Drawing DBs have their
//! own shape and merge in [`crate::drawing`].

use std::path::Path;

use rusqlite::Connection;

use crate::{
    attach_uri, highlight_semantics, is_migration_leftover, quote_ident, HighlightSemantics,
};

#[derive(Debug, Clone, serde::Serialize)]
pub struct TableMergeReport {
    pub table: String,
    pub source_rows: u64,
    pub dest_rows_before: u64,
    pub overlap: u64,
    pub dest_rows_after: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DbMergeReport {
    pub dest: String,
    pub sources: Vec<String>,
    pub tables: Vec<TableMergeReport>,
    pub applied: bool,
}

/// Maps a source table name to its canonical (v1/v2) name.
fn canonical_table(name: &str) -> &str {
    match name {
        "0" => "minecraft:overworld",
        "-1" => "minecraft:the_nether",
        "1" => "minecraft:the_end",
        other => other,
    }
}

/// The v0 table name a canonical dimension table had, if it had one.
fn numeric_alias(canon: &str) -> Option<&'static str> {
    match canon {
        "minecraft:overworld" => Some("0"),
        "minecraft:the_nether" => Some("-1"),
        "minecraft:the_end" => Some("1"),
        _ => None,
    }
}

/// Brings a highlight DB (opened read-write) to schema v2 in place.
/// Re-implements the semantics of XaeroPlus's V0ToV1Migration and
/// V1ToV2Migration (rename numeric tables, rebuild as WITHOUT ROWID).
pub fn normalize_to_v2(conn: &Connection) -> Result<(), String> {
    let e = |e: rusqlite::Error| e.to_string();
    conn.execute_batch("BEGIN").map_err(e)?;
    let result = (|| -> Result<(), String> {
        // Enumerate tables with their DDL.
        let mut tables: Vec<(String, String)> = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT name, COALESCE(sql,'') FROM sqlite_master \
                     WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                )
                .map_err(e)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map_err(e)?;
            for row in rows.flatten() {
                if !is_migration_leftover(&row.0) {
                    tables.push(row);
                }
            }
        }
        let has_metadata = tables.iter().any(|(n, _)| n == "metadata");

        // v0 -> v1: numeric dimension tables get resource-key names.
        for (name, _) in tables.clone() {
            let canon = canonical_table(&name);
            if canon == name {
                continue;
            }
            if tables.iter().any(|(n, _)| n == canon) {
                // Target exists (user ran mixed versions): union then drop.
                conn.execute_batch(&format!(
                    "INSERT OR IGNORE INTO {c} (x, z, foundTime) \
                     SELECT x, z, foundTime FROM {o}; DROP TABLE {o};",
                    c = quote_ident(canon),
                    o = quote_ident(&name),
                ))
                .map_err(e)?;
            } else {
                conn.execute_batch(&format!(
                    "ALTER TABLE {o} RENAME TO {c};",
                    o = quote_ident(&name),
                    c = quote_ident(canon),
                ))
                .map_err(e)?;
            }
        }

        // v1 -> v2: rebuild any highlight table that isn't WITHOUT ROWID.
        let mut current: Vec<(String, String)> = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT name, COALESCE(sql,'') FROM sqlite_master \
                     WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name != 'metadata'",
                )
                .map_err(e)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map_err(e)?;
            for row in rows.flatten() {
                if !is_migration_leftover(&row.0) {
                    current.push(row);
                }
            }
        }
        for (name, sql) in current {
            let is_highlight = conn
                .prepare(&format!(
                    "SELECT x, z, foundTime FROM {} LIMIT 0",
                    quote_ident(&name)
                ))
                .is_ok();
            if !is_highlight || sql.to_uppercase().contains("WITHOUT ROWID") {
                continue;
            }
            let tmp = quote_ident(&format!("{name}_v2_migration"));
            let t = quote_ident(&name);
            // A twin left by an interrupted game migration would make the
            // CREATE fail and roll the whole normalization back.
            conn.execute_batch(&format!(
                "DROP TABLE IF EXISTS {tmp}; \
                 CREATE TABLE {tmp} (x INTEGER, z INTEGER, foundTime INTEGER, \
                 PRIMARY KEY (x, z)) WITHOUT ROWID; \
                 INSERT OR IGNORE INTO {tmp} (x, z, foundTime) \
                 SELECT x, z, foundTime FROM {t}; \
                 DROP TABLE {t}; \
                 ALTER TABLE {tmp} RENAME TO {t};",
            ))
            .map_err(e)?;
        }

        if !has_metadata {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS metadata (id INTEGER PRIMARY KEY, version INTEGER)",
            )
            .map_err(e)?;
        }
        conn.execute(
            "INSERT OR REPLACE INTO metadata (id, version) VALUES (0, 2)",
            [],
        )
        .map_err(e)?;
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT").map_err(e),
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

/// The value semantics of a merge, inferred from the file names involved: an
/// `-o out.db` destination carries no hint, so a LavaColumns source is enough
/// to make the whole merge height-valued.
fn merge_semantics(dest: &Path, sources: &[&Path]) -> HighlightSemantics {
    let name = |p: &Path| {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    std::iter::once(dest)
        .chain(sources.iter().copied())
        .map(|p| highlight_semantics(&name(p)))
        .find(|s| s.prefers_max())
        .unwrap_or(HighlightSemantics::Timestamp)
}

/// Merges `sources` into the file at `dest` (modified in place — callers doing
/// `-o out.db` copy first). With `apply = false` only the report is computed.
///
/// The conflict rule follows the DBs' value semantics, inferred from the file
/// names; use [`merge_into_with`] to state them explicitly.
pub fn merge_into(dest: &Path, sources: &[&Path], apply: bool) -> Result<DbMergeReport, String> {
    merge_into_with(dest, sources, apply, merge_semantics(dest, sources))
}

/// [`merge_into`] with the value semantics given rather than inferred.
pub fn merge_into_with(
    dest: &Path,
    sources: &[&Path],
    apply: bool,
    semantics: HighlightSemantics,
) -> Result<DbMergeReport, String> {
    let e = |e: rusqlite::Error| e.to_string();
    // Dry-runs open the destination read-only so even pragmas can't touch it
    // (in dry-run the "destination" may be a source file used for counting).
    // Apply mode opens without CREATE: a mistyped destination must fail, not
    // become a fresh empty database that the sources then merge into.
    let conn = if apply {
        Connection::open_with_flags(
            dest,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
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
        normalize_to_v2(&conn)?;
    }

    let mut report = DbMergeReport {
        dest: dest.display().to_string(),
        sources: sources.iter().map(|s| s.display().to_string()).collect(),
        applied: apply,
        ..Default::default()
    };

    for (i, source) in sources.iter().enumerate() {
        let alias = format!("src{i}");
        // Read-only URI attach: sources are never written.
        let uri = attach_uri(source);
        conn.execute(&format!("ATTACH DATABASE ?1 AS {alias}"), [&uri])
            .map_err(|er| format!("attach {}: {er}", source.display()))?;

        // Enumerate source highlight tables.
        let mut src_tables: Vec<String> = Vec::new();
        {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT name FROM {alias}.sqlite_master WHERE type='table' \
                     AND name NOT LIKE 'sqlite_%' AND name != 'metadata'"
                ))
                .map_err(e)?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(e)?;
            for t in rows.flatten() {
                if is_migration_leftover(&t) {
                    continue;
                }
                let is_highlight = conn
                    .prepare(&format!(
                        "SELECT x, z, foundTime FROM {alias}.{} LIMIT 0",
                        quote_ident(&t)
                    ))
                    .is_ok();
                if is_highlight {
                    src_tables.push(t);
                }
            }
        }
        src_tables.sort();

        for src_table in src_tables {
            let canon = canonical_table(&src_table).to_string();
            let qsrc = format!("{alias}.{}", quote_ident(&src_table));
            let qdst = quote_ident(&canon);
            let source_rows: u64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {qsrc}"), [], |r| r.get(0))
                .map_err(e)?;
            if apply {
                conn.execute_batch(&format!(
                    "CREATE TABLE IF NOT EXISTS {qdst} (x INTEGER, z INTEGER, foundTime INTEGER, \
                     PRIMARY KEY (x, z)) WITHOUT ROWID"
                ))
                .map_err(e)?;
            }
            let table_exists = |name: &str| -> Result<bool, String> {
                conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                    [name],
                    |r| r.get::<_, u64>(0).map(|n| n > 0),
                )
                .map_err(e)
            };
            // After normalization the canonical table is the only one there
            // can be; a dry run against a v0 destination has to count the
            // numeric-named table `--apply` would have renamed, or its report
            // shows zero overlap where the real merge finds plenty.
            let qdst = if table_exists(&canon)? {
                qdst
            } else if let Some(alias) = numeric_alias(&canon).filter(|_| !apply) {
                quote_ident(alias)
            } else {
                qdst
            };
            let dest_exists = apply
                || table_exists(&canon)?
                || numeric_alias(&canon).is_some_and(|a| table_exists(a).unwrap_or(false));
            let (dest_rows_before, overlap): (u64, u64) = if dest_exists {
                let before = conn
                    .query_row(&format!("SELECT COUNT(*) FROM {qdst}"), [], |r| r.get(0))
                    .map_err(e)?;
                let overlap = conn
                    .query_row(
                        &format!(
                            "SELECT COUNT(*) FROM {qsrc} s JOIN {qdst} m ON s.x = m.x AND s.z = m.z"
                        ),
                        [],
                        |r| r.get(0),
                    )
                    .map_err(e)?;
                (before, overlap)
            } else {
                (0, 0)
            };
            let mut dest_rows_after = dest_rows_before + source_rows - overlap;
            if apply {
                // Oldest sighting wins for timestamps; tallest column wins for
                // LavaColumns heights.
                let keep = if semantics.prefers_max() {
                    "MAX"
                } else {
                    "MIN"
                };
                // One transaction per table: the invariant below is checked
                // before anything is committed, so a failed merge leaves the
                // destination as it was. COALESCE keeps a NULL on either side
                // from erasing the real value (scalar MIN/MAX return NULL).
                conn.execute_batch("BEGIN IMMEDIATE").map_err(e)?;
                let merged = (|| -> Result<u64, String> {
                    conn.execute_batch(&format!(
                        "INSERT INTO {qdst} (x, z, foundTime) \
                         SELECT x, z, foundTime FROM {qsrc} WHERE true \
                         ON CONFLICT(x, z) DO UPDATE SET \
                         foundTime = {keep}(COALESCE(foundTime, excluded.foundTime), \
                                            COALESCE(excluded.foundTime, foundTime))"
                    ))
                    .map_err(e)?;
                    let after: u64 = conn
                        .query_row(&format!("SELECT COUNT(*) FROM {qdst}"), [], |r| r.get(0))
                        .map_err(e)?;
                    let expected = dest_rows_before + source_rows - overlap;
                    if after != expected {
                        return Err(format!(
                            "post-merge invariant failed for {canon}: {after} rows, expected {expected}"
                        ));
                    }
                    Ok(after)
                })();
                dest_rows_after = match merged {
                    Ok(n) => {
                        conn.execute_batch("COMMIT").map_err(e)?;
                        n
                    }
                    Err(err) => {
                        let _ = conn.execute_batch("ROLLBACK");
                        return Err(err);
                    }
                };
            }
            report.tables.push(TableMergeReport {
                table: canon,
                source_rows,
                dest_rows_before,
                overlap,
                dest_rows_after,
            });
        }
        conn.execute_batch(&format!("DETACH DATABASE {alias}"))
            .map_err(e)?;
    }

    if apply {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_db(path: &Path, version: u32, rows: &[(&str, i32, i32, i64)]) {
        let conn = Connection::open(path).unwrap();
        match version {
            0 => {
                conn.execute_batch(
                    "CREATE TABLE \"0\" (x INTEGER, z INTEGER, foundTime INTEGER);
                     CREATE UNIQUE INDEX unique_xzO ON \"0\" (x, z);
                     CREATE TABLE \"-1\" (x INTEGER, z INTEGER, foundTime INTEGER);
                     CREATE UNIQUE INDEX unique_xzN ON \"-1\" (x, z);",
                )
                .unwrap();
            }
            1 => {
                conn.execute_batch(
                    "CREATE TABLE metadata (id INTEGER PRIMARY KEY, version INTEGER);
                     INSERT INTO metadata VALUES (0, 1);
                     CREATE TABLE \"minecraft:overworld\" (x INTEGER, z INTEGER, foundTime INTEGER);
                     CREATE UNIQUE INDEX \"unique_xz_minecraft:overworld\" ON \"minecraft:overworld\" (x, z);
                     CREATE TABLE \"minecraft:the_nether\" (x INTEGER, z INTEGER, foundTime INTEGER);
                     CREATE UNIQUE INDEX \"unique_xz_minecraft:the_nether\" ON \"minecraft:the_nether\" (x, z);",
                )
                .unwrap();
            }
            _ => {
                conn.execute_batch(
                    "CREATE TABLE metadata (id INTEGER PRIMARY KEY, version INTEGER);
                     INSERT INTO metadata VALUES (0, 2);
                     CREATE TABLE \"minecraft:overworld\" (x INTEGER, z INTEGER, foundTime INTEGER, PRIMARY KEY (x,z)) WITHOUT ROWID;
                     CREATE TABLE \"minecraft:the_nether\" (x INTEGER, z INTEGER, foundTime INTEGER, PRIMARY KEY (x,z)) WITHOUT ROWID;",
                )
                .unwrap();
            }
        }
        for (table, x, z, t) in rows {
            let table = if version == 0 {
                match *table {
                    "minecraft:overworld" => "0",
                    "minecraft:the_nether" => "-1",
                    other => other,
                }
            } else {
                table
            };
            conn.execute(
                &format!(
                    "INSERT INTO {} (x, z, foundTime) VALUES (?1, ?2, ?3)",
                    quote_ident(table)
                ),
                rusqlite::params![x, z, t],
            )
            .unwrap();
        }
    }

    #[test]
    fn merges_v0_v1_v2_with_oldest_wins() {
        let dir = std::env::temp_dir().join(format!("xt-dbmerge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("dest.db");
        let s0 = dir.join("s0.db");
        let s1 = dir.join("s1.db");
        // dest: v1 with two rows; overlapping key (1,1) has newer time 2000.
        mk_db(
            &dest,
            1,
            &[
                ("minecraft:overworld", 1, 1, 2000),
                ("minecraft:overworld", 2, 2, 500),
            ],
        );
        // v0 source: overlapping (1,1) with OLDER time 1000 (must win) + new row.
        mk_db(
            &s0,
            0,
            &[
                ("minecraft:overworld", 1, 1, 1000),
                ("minecraft:the_nether", 9, 9, 42),
            ],
        );
        // v2 source: disjoint row.
        mk_db(&s1, 2, &[("minecraft:overworld", 3, 3, 777)]);

        // Dry-run first: no writes.
        let dry = merge_into(&dest, &[&s0, &s1], false).unwrap();
        assert!(!dry.applied);

        let report = merge_into(&dest, &[&s0, &s1], true).unwrap();
        let ow: Vec<_> = report
            .tables
            .iter()
            .filter(|t| t.table == "minecraft:overworld")
            .collect();
        assert_eq!(ow.len(), 2);
        assert_eq!(ow[0].overlap, 1);

        let conn = Connection::open(&dest).unwrap();
        let v: u32 = conn
            .query_row("SELECT version FROM metadata", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2);
        let t11: i64 = conn
            .query_row(
                "SELECT foundTime FROM \"minecraft:overworld\" WHERE x=1 AND z=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(t11, 1000, "oldest foundTime wins");
        let n: u64 = conn
            .query_row("SELECT COUNT(*) FROM \"minecraft:overworld\"", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 3);
        let nether: u64 = conn
            .query_row("SELECT COUNT(*) FROM \"minecraft:the_nether\"", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(nether, 1, "v0 numeric table mapped to resource key");
        // Sources untouched.
        let s0c = Connection::open(&s0).unwrap();
        let s0n: u64 = s0c
            .query_row("SELECT COUNT(*) FROM \"0\"", [], |r| r.get(0))
            .unwrap();
        assert_eq!(s0n, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn interrupted_game_migration_leftover_is_dropped_not_merged() {
        let dir = std::env::temp_dir().join(format!("xt-dbleftover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("dest.db");
        let src = dir.join("src.db");
        mk_db(&dest, 1, &[("minecraft:overworld", 1, 1, 10)]);
        // What XaeroPlus leaves behind when V1ToV2Migration dies mid-way: the
        // twin table exists, the original is still a rowid table.
        let c = Connection::open(&dest).unwrap();
        c.execute_batch(
            "CREATE TABLE \"minecraft:overworld_v2_migration\" (x INTEGER, z INTEGER, foundTime INTEGER, PRIMARY KEY (x,z)) WITHOUT ROWID;
             INSERT INTO \"minecraft:overworld_v2_migration\" VALUES (9, 9, 9);",
        )
        .unwrap();
        drop(c);
        mk_db(&src, 2, &[("minecraft:overworld", 2, 2, 20)]);
        let c = Connection::open(&src).unwrap();
        c.execute_batch(
            "CREATE TABLE \"minecraft:the_end_v2_migration\" (x INTEGER, z INTEGER, foundTime INTEGER);
             INSERT INTO \"minecraft:the_end_v2_migration\" VALUES (8, 8, 8);",
        )
        .unwrap();
        drop(c);

        let report = merge_into(&dest, &[&src], true).unwrap();
        assert!(
            report
                .tables
                .iter()
                .all(|t| !t.table.ends_with("_v2_migration")),
            "leftover twins are never dimensions: {:?}",
            report.tables
        );
        let conn = Connection::open(&dest).unwrap();
        let leftovers: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE '%_v2_migration'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leftovers, 0, "normalization dropped the leftover twin");
        let n: u64 = conn
            .query_row("SELECT COUNT(*) FROM \"minecraft:overworld\"", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 2, "rows of the leftover twin were not merged in");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_on_a_v0_destination_reports_what_apply_does() {
        let dir = std::env::temp_dir().join(format!("xt-dbv0dry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("dest.db");
        let src = dir.join("src.db");
        mk_db(
            &dest,
            0,
            &[
                ("minecraft:overworld", 1, 1, 10),
                ("minecraft:overworld", 2, 2, 10),
            ],
        );
        mk_db(&src, 2, &[("minecraft:overworld", 1, 1, 5)]);
        let dry = merge_into(&dest, &[&src], false).unwrap();
        let applied = merge_into(&dest, &[&src], true).unwrap();
        let pick = |r: &DbMergeReport| {
            let t = r
                .tables
                .iter()
                .find(|t| t.table == "minecraft:overworld")
                .unwrap();
            (t.dest_rows_before, t.overlap, t.dest_rows_after)
        };
        assert_eq!(pick(&dry), (2, 1, 2));
        assert_eq!(pick(&dry), pick(&applied));
        // A mistyped --apply destination must not be created on the fly.
        let err = merge_into(&dir.join("nope.db"), &[&src], true).unwrap_err();
        assert!(err.starts_with("open "), "{err}");
        assert!(!dir.join("nope.db").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lava_columns_merge_keeps_the_tallest_column() {
        let dir = std::env::temp_dir().join(format!("xt-lavamerge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // foundTime here is a column HEIGHT, so the deeper column must win.
        let dest = dir.join("XaeroPlusLavaColumns.db");
        let src = dir.join("other-XaeroPlusLavaColumns.db");
        mk_db(&dest, 2, &[("minecraft:the_nether", 1, 1, 3)]);
        mk_db(&src, 2, &[("minecraft:the_nether", 1, 1, 40)]);
        merge_into(&dest, &[&src], true).unwrap();
        let conn = Connection::open(&dest).unwrap();
        let h: i64 = conn
            .query_row(
                "SELECT foundTime FROM \"minecraft:the_nether\" WHERE x=1 AND z=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(h, 40, "tallest lava column wins");

        // The same rows under a timestamp DB name keep oldest-wins.
        let dest = dir.join("XaeroPlusNewChunks.db");
        let src = dir.join("other-XaeroPlusNewChunks.db");
        mk_db(&dest, 2, &[("minecraft:the_nether", 1, 1, 3)]);
        mk_db(&src, 2, &[("minecraft:the_nether", 1, 1, 40)]);
        merge_into(&dest, &[&src], true).unwrap();
        let conn = Connection::open(&dest).unwrap();
        let t: i64 = conn
            .query_row(
                "SELECT foundTime FROM \"minecraft:the_nether\" WHERE x=1 AND z=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(t, 3, "oldest foundTime still wins for timestamps");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
