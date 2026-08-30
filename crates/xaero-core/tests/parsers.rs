//! Parser checks against the real sample corpus. `#[ignore]`d, so they report
//! as skipped rather than as passed when XAERO_CORPUS is unset.

use xaero_core::dimconfig::{parse_dimension_config, parse_minimap_config};
use xaero_core::waypoints::parse_waypoints_file;

#[test]
#[ignore = "requires corpus (XAERO_CORPUS)"]
fn parses_all_sample_waypoint_files() {
    let root = test_support::corpus_root().expect("XAERO_CORPUS");
    let mut files = 0;
    let mut total = 0;
    for entry in walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        let name = p.file_name().unwrap().to_string_lossy();
        let in_minimap = p.components().any(|c| c.as_os_str() == "minimap");
        if !in_minimap || !name.starts_with("mw$") || !name.ends_with(".txt") {
            continue;
        }
        let text = std::fs::read_to_string(p).unwrap();
        let parsed = parse_waypoints_file(&text);
        assert!(
            parsed.other_lines.is_empty(),
            "unparsed lines in {}: {:?}",
            p.display(),
            parsed.other_lines
        );
        files += 1;
        total += parsed.waypoints.len();
        for w in &parsed.waypoints {
            assert!(!w.set.is_empty());
        }
    }
    eprintln!("waypoints: {total} across {files} files");
    assert!(files >= 10, "expected sample waypoint files, saw {files}");
    assert!(total >= 19, "expected sample waypoints, saw {total}");
}

#[test]
#[ignore = "requires corpus (XAERO_CORPUS)"]
fn parses_all_sample_configs() {
    let root = test_support::corpus_root().expect("XAERO_CORPUS");
    let mut dimcfgs = 0;
    let mut mmcfgs = 0;
    for entry in walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        match p.file_name().unwrap().to_str() {
            Some("dimension_config.txt") => {
                let c = parse_dimension_config(&std::fs::read_to_string(p).unwrap());
                assert!(
                    c.other_lines.is_empty(),
                    "unparsed lines in {}: {:?}",
                    p.display(),
                    c.other_lines
                );
                assert!(
                    c.dimension_type_id.is_some(),
                    "{} missing dimensionTypeId",
                    p.display()
                );
                dimcfgs += 1;
            }
            Some("config.txt") if p.components().any(|c| c.as_os_str() == "minimap") => {
                let c = parse_minimap_config(&std::fs::read_to_string(p).unwrap());
                assert!(!c.config.entries.is_empty());
                mmcfgs += 1;
            }
            _ => {}
        }
    }
    eprintln!("configs: {dimcfgs} dimension_config, {mmcfgs} minimap config");
    assert!(dimcfgs >= 5);
    assert!(mmcfgs >= 5);
}
