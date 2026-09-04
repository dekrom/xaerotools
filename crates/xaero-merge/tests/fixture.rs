//! The built-in merge fixture from the plan: Multiplayer_2b2t exists in both
//! sample roots (1.21.4 = major 6 data, 1.21.8 = major 7 data) with real
//! overlap — null 307 vs 90 regions (20 conflicts), DIM-1 296 vs 794 (71),
//! DIM1 0 vs 4. Expected merged totals: null 377, DIM-1 1019, DIM1 4.

use xaero_merge::{merge_to_output, MergeOptions};

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
#[ignore = "requires corpus (XAERO_CORPUS)"]
fn merges_the_2b2t_fixture() {
    let root = test_support::corpus_root().expect("XAERO_CORPUS");
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

/// An interrupted merge must be continuable: `--resume` fills in what is
/// missing, leaves finished output alone, and still reaches the same result as
/// an uninterrupted run — including rewriting a file the interruption
/// truncated.
#[test]
fn resume_completes_an_interrupted_merge() {
    let Some(root) = corpus_root() else {
        eprintln!("corpus not found; skipping");
        return;
    };
    let a = root.join("xaero1.21.4");
    let b = root.join("xaero1.21.8");
    let base = std::env::temp_dir().join(format!("xt-merge-resume-{}", std::process::id()));
    let full = base.join("full");
    let partial = base.join("partial");
    let _ = std::fs::remove_dir_all(&base);

    let opts = MergeOptions {
        apply: true,
        servers: vec!["Multiplayer_2b2t".into()],
        ..Default::default()
    };

    // Reference: one clean, uninterrupted merge.
    merge_to_output(&a, &b, &full, &opts).unwrap();

    // Now simulate the interrupted run: same merge, then damage the output the
    // way a hard stop would — delete some regions outright and truncate one.
    merge_to_output(&a, &b, &partial, &opts).unwrap();
    let layer = partial.join("world-map/Multiplayer_2b2t/DIM-1/mw$default");
    let mut regions: Vec<PathBuf> = std::fs::read_dir(&layer)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "zip"))
        .collect();
    regions.sort();
    assert!(regions.len() > 20, "fixture should have regions to damage");
    let deleted: Vec<PathBuf> = regions.iter().take(10).cloned().collect();
    for p in &deleted {
        std::fs::remove_file(p).unwrap();
    }
    let truncated = regions[11].clone();
    std::fs::write(&truncated, b"half a file").unwrap();

    // Without --resume the guard refuses to touch a non-empty output at all.
    // With it, the run fills the holes back in.
    let resumed = MergeOptions {
        resume: true,
        ..opts.clone()
    };
    merge_to_output(&a, &b, &partial, &resumed).unwrap();

    for p in &deleted {
        assert!(p.exists(), "resume should have restored {}", p.display());
    }
    let reference = full.join("world-map/Multiplayer_2b2t/DIM-1/mw$default");
    let rebuilt = std::fs::read(&truncated).unwrap();
    let expected = std::fs::read(reference.join(truncated.file_name().unwrap())).unwrap();
    assert_eq!(
        rebuilt, expected,
        "a truncated copy must be written again, not kept"
    );

    // Every file of the clean run is present in the resumed one, byte for byte.
    for entry in std::fs::read_dir(&reference).unwrap() {
        let want = entry.unwrap().path();
        let got = layer.join(want.file_name().unwrap());
        assert!(got.exists(), "missing after resume: {}", got.display());
        assert_eq!(
            std::fs::read(&got).unwrap(),
            std::fs::read(&want).unwrap(),
            "differs after resume: {}",
            got.display()
        );
    }

    let _ = std::fs::remove_dir_all(&base);
}

/// One B world must pair with at most one A world. The corpus has the case
/// built in: the 1.21.8 root holds both `Multiplayer_2b2t` and
/// `Multiplayer_2b2t.org`, and both match the 1.21.4 root's `Multiplayer_2b2t`
/// once the base-domain heuristic is accepted. The exact match wins the pair;
/// the other A world has to survive as a whole copy under its own name, not
/// be written over the pair's output.
#[test]
fn a_b_world_pairs_at_most_once() {
    let Some(root) = corpus_root() else {
        eprintln!("corpus not found; skipping");
        return;
    };
    let a = root.join("xaero1.21.8");
    let b = root.join("xaero1.21.4");
    let out = std::env::temp_dir().join(format!("xt-merge-pairing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);

    let opts = MergeOptions {
        apply: true,
        auto_alias: true,
        ..Default::default()
    };
    let report = merge_to_output(&a, &b, &out, &opts).unwrap();
    assert_eq!(
        report.world_pairs,
        vec![(
            "Multiplayer_2b2t".to_string(),
            "Multiplayer_2b2t".to_string()
        )]
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.starts_with("Multiplayer_2b2t.org (A)")),
        "the second claim on the B world must be reported: {:?}",
        report.warnings
    );

    let zips = |dir: &Path| -> usize {
        std::fs::read_dir(dir)
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".zip"))
                    .count()
            })
            .unwrap_or(0)
    };
    // The pair merged as in the fixture test (roles swapped, same totals).
    let paired = out.join("world-map/Multiplayer_2b2t");
    assert_eq!(zips(&paired.join("null/mw$default")), 377);
    assert_eq!(zips(&paired.join("DIM-1/mw$default")), 1019);
    // The .org world arrived whole under its own name, every layer intact.
    let src = a.join("world-map/Multiplayer_2b2t.org");
    let dst = out.join("world-map/Multiplayer_2b2t.org");
    let mut layers = 0;
    for dim in ["null", "DIM-1", "DIM1"] {
        let layer = Path::new(dim).join("mw$default");
        let want = zips(&src.join(&layer));
        if want > 0 {
            layers += 1;
            assert_eq!(zips(&dst.join(&layer)), want, "{dim} regions lost");
        }
    }
    assert!(layers > 0, "fixture should carry .org regions");

    let _ = std::fs::remove_dir_all(&out);
}

/// The guard against writing into a source is the library's, not the CLI's:
/// OUT inside A, A inside OUT, or A == B are all refused before anything is
/// touched, and a non-empty OUT is refused unless resuming.
#[test]
fn refuses_outputs_that_overlap_a_source() {
    let Some(root) = corpus_root() else {
        eprintln!("corpus not found; skipping");
        return;
    };
    let a = root.join("xaero1.21.4");
    let b = root.join("xaero1.21.8");
    let opts = MergeOptions {
        apply: true,
        ..Default::default()
    };
    let probe = a.join("world-map/Multiplayer_2b2t/server_config.txt");
    let before = std::fs::read(&probe).ok();

    let inside = a.join("merged-here");
    let err = merge_to_output(&a, &b, &inside, &opts).unwrap_err();
    assert!(err.contains("overlaps A root"), "{err}");
    assert!(!inside.exists());

    let err = merge_to_output(&a, &b, &a, &opts).unwrap_err();
    assert!(err.contains("overlaps A root"), "{err}");
    let err = merge_to_output(&a, &a, &std::env::temp_dir().join("xt-never"), &opts).unwrap_err();
    assert!(err.contains("same directory"), "{err}");

    let around = root.clone();
    let err = merge_to_output(&a, &b, &around, &opts).unwrap_err();
    assert!(err.contains("overlaps"), "{err}");

    let busy = std::env::temp_dir().join(format!("xt-merge-busy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&busy);
    std::fs::create_dir_all(&busy).unwrap();
    std::fs::write(busy.join("something"), b"x").unwrap();
    let err = merge_to_output(&a, &b, &busy, &opts).unwrap_err();
    assert!(err.contains("not empty"), "{err}");
    let _ = std::fs::remove_dir_all(&busy);

    assert_eq!(
        std::fs::read(&probe).ok(),
        before,
        "a refused run must not touch A"
    );
}
