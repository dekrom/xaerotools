//! The built-in merge fixture from the plan: Multiplayer_2b2t exists in both
//! sample roots (1.21.4 = major 6 data, 1.21.8 = major 7 data) with real
//! overlap — null 307 vs 90 regions (20 conflicts), DIM-1 296 vs 794 (71),
//! DIM1 0 vs 4. Expected merged totals: null 377, DIM-1 1019, DIM1 4.

use std::path::{Path, PathBuf};

use xaero_merge::{merge_to_output, MergeOptions};

fn corpus_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("XAERO_CORPUS") {
        let p = PathBuf::from(p);
        return p.is_dir().then_some(p);
    }
    let fallback = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../sample data");
    fallback.is_dir().then(|| fallback.canonicalize().unwrap())
}

fn unit<'a>(
    report: &'a xaero_merge::MergeReport,
    dim: &str,
    cave: Option<i32>,
) -> &'a xaero_merge::UnitReport {
    report
        .units
        .iter()
        .find(|u| u.dim == dim && u.cave == cave && u.mw == "mw$default")
        .unwrap_or_else(|| panic!("unit {dim} {cave:?} missing"))
}

#[test]
fn merges_the_2b2t_fixture() {
    let Some(root) = corpus_root() else {
        eprintln!("corpus not found; skipping");
        return;
    };
    let a = root.join("xaero1.21.4");
    let b = root.join("xaero1.21.8");
    let out = std::env::temp_dir().join(format!("xt-merge-fixture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);

    let opts = MergeOptions {
        apply: false,
        servers: vec!["Multiplayer_2b2t".into()],
        ..Default::default()
    };

    // Source fingerprints to prove sources stay untouched.
    let probe_a = a.join("world-map/Multiplayer_2b2t/null/mw$default/-155_-95.zip");
    let probe_b = b.join("world-map/Multiplayer_2b2t/DIM-1/mw$default/0_0.zip");
    let before_a = std::fs::read(&probe_a).ok();
    let before_b = std::fs::read(&probe_b).ok();

    // ---- dry run -----------------------------------------------------------
    let dry = merge_to_output(&a, &b, &out, &opts).unwrap();
    assert!(!dry.applied);
    assert!(!out.exists(), "dry run must write nothing");
    let ow = unit(&dry, "null", None);
    assert_eq!((ow.only_a, ow.only_b, ow.conflicts), (287, 70, 20));
    let nether = unit(&dry, "DIM-1", None);
    assert_eq!(
        (nether.only_a, nether.only_b, nether.conflicts),
        (225, 723, 71)
    );
    let end = unit(&dry, "DIM1", None);
    assert_eq!((end.only_a, end.only_b, end.conflicts), (0, 4, 0));
    assert!(!dry.dbs.is_empty(), "db dry-run reports expected");

    // ---- apply -------------------------------------------------------------
    let opts = MergeOptions {
        apply: true,
        ..opts
    };
    let report = merge_to_output(&a, &b, &out, &opts).unwrap();
    assert!(report.applied);
    for u in &report.units {
        assert!(
            u.merge_errors.is_empty(),
            "{}/{}: {:?}",
            u.dim,
            u.mw,
            u.merge_errors
        );
    }

    let count = |dim: &str| {
        std::fs::read_dir(
            out.join("world-map/Multiplayer_2b2t")
                .join(dim)
                .join("mw$default"),
        )
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().ends_with(".zip"))
                .count()
        })
        .unwrap_or(0)
    };
    assert_eq!(count("null"), 377);
    assert_eq!(count("DIM-1"), 1019);
    assert_eq!(count("DIM1"), 4);

    // Every merged conflict decodes as 7.8; untouched copies keep their bytes.
    let merged_conflict = out.join("world-map/Multiplayer_2b2t/DIM-1/mw$default/0_0.zip");
    let stream =
        xaero_core::read_region_container(&std::fs::read(&merged_conflict).unwrap()).unwrap();
    let dec = xaero_core::decode_region(&stream).unwrap();
    assert_eq!((dec.version.major, dec.version.minor), (7, 8));
    assert!(!dec.truncated);

    // No cache dirs or temp files leaked into the output.
    let mut stack = vec![out.clone()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.file_type().unwrap().is_dir() {
                assert!(
                    !xaero_core::naming::is_cache_dir_name(&name),
                    "cache dir leaked: {}",
                    e.path().display()
                );
                stack.push(e.path());
            } else {
                assert!(
                    !name.ends_with(".temp")
                        && !name.ends_with(".outdated")
                        && !name.contains(".tmp-xt")
                );
            }
        }
    }

    // Merged DBs normalized to v2 with plausible row counts.
    let db = xaero_db::open_readonly(&out.join("world-map/Multiplayer_2b2t/XaeroPlusOldChunks.db"))
        .unwrap();
    assert_eq!(db.version, 2);
    let nether_rows = db
        .count(&db.table_for_dimension("minecraft:the_nether").unwrap())
        .unwrap();
    assert!(
        nether_rows >= 251_228,
        "merged >= B alone, got {nether_rows}"
    );

    // Waypoints merged.
    assert!(report.waypoint_files_merged > 0);
    let wp = out.join("minimap/Multiplayer_2b2t/dim%-1/mw$default_1.txt");
    assert!(wp.is_file());

    // Sources untouched.
    assert_eq!(std::fs::read(&probe_a).ok(), before_a);
    assert_eq!(std::fs::read(&probe_b).ok(), before_b);

    let _ = std::fs::remove_dir_all(&out);
}
