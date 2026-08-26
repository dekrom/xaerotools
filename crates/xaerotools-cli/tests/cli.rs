//! End-to-end checks of the archivist subcommands against the sample corpus.
//!
//! `render` is asserted pixel-for-pixel against `render-region` so the stitch
//! path can never silently drift from the single-region renderer, and `doctor`
//! is pointed at a scratch tree of deliberately broken files. The corpus lives
//! outside the repo; corpus tests are skipped (with a notice) when it is
//! absent. Override with XAERO_CORPUS=/path/to/sample-data.
//!
//! Nothing here ever writes inside the corpus.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The 1.21.8 half of the corpus: one world with a 4-region End dimension.
fn corpus_root_1218() -> PathBuf {
    test_support::corpus_root()
        .expect("XAERO_CORPUS")
        .join("xaero1.21.8")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xaerotools-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn xt(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_xaerotools"))
        .args(args)
        .output()
        .expect("run xaerotools")
}

fn read_png(path: &Path) -> (u32, u32, Vec<u8>) {
    let file = std::io::BufReader::new(std::fs::File::open(path).expect("open png"));
    let dec = png::Decoder::new(file);
    let mut reader = dec.read_info().expect("png info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("png size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

#[test]
#[ignore = "requires corpus (XAERO_CORPUS)"]
fn render_stitch_matches_render_region() {
    let root = corpus_root_1218();
    let dir = scratch("render");
    let stitched = dir.join("end.png");
    let single = dir.join("region.png");
    let out = xt(&[
        "render",
        "--root",
        root.to_str().unwrap(),
        "--world",
        "Multiplayer_2b2t",
        "--dim",
        "DIM1",
        "--all",
        "-o",
        stitched.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "render failed: {out:?}");

    // The End map is exactly regions (-1,-1)..(0,0), so --all is 2x2 natively.
    let (w, h, stitch) = read_png(&stitched);
    assert_eq!((w, h), (1024, 1024));

    let region = root
        .join("world-map/Multiplayer_2b2t/DIM1/mw$default")
        .join("-1_-1.zip");
    let out = xt(&[
        "render-region",
        region.to_str().unwrap(),
        "-o",
        single.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "render-region failed: {out:?}");
    let (rw, rh, one) = read_png(&single);
    assert_eq!((rw, rh), (512, 512));

    // (-1,-1) is the top-left quadrant of the stitch.
    for y in 0..512usize {
        let a = &stitch[y * 1024 * 4..y * 1024 * 4 + 512 * 4];
        let b = &one[y * 512 * 4..(y + 1) * 512 * 4];
        assert_eq!(a, b, "row {y} differs from the single-region render");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires corpus (XAERO_CORPUS)"]
fn render_refuses_an_oversized_box() {
    let root = corpus_root_1218();
    let dir = scratch("cap");
    let out = xt(&[
        "render",
        "--root",
        root.to_str().unwrap(),
        "--world",
        "Multiplayer_2b2t",
        "--dim",
        "DIM1",
        "--all",
        "--max-px",
        "100",
        "-o",
        dir.join("big.png").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(2));
    let msg = String::from_utf8_lossy(&out.stderr);
    assert!(msg.contains("over the 100 px cap"), "{msg}");
    assert!(!dir.join("big.png").exists(), "capped render wrote a file");
    let _ = std::fs::remove_dir_all(&dir);
}

/// One unreadable file in a 431 GB archive must cost its own square and
/// nothing else.
#[test]
fn render_leaves_a_hole_for_a_region_it_cannot_decode() {
    let dir = scratch("hole");
    let map = dir.join("world-map/Multiplayer_test/null/mw$default");
    std::fs::create_dir_all(&map).expect("scratch world");
    let mut good = vec![0xFFu8];
    good.extend_from_slice(&((7i32 << 16) | 8).to_be_bytes());
    good.push(0x00); // chunk marker (0,0)
    good.extend(std::iter::repeat_n(0xFFu8, 16 * 4)); // 16 tiles, all "absent"
    std::fs::write(map.join("0_0.xaero"), &good).unwrap();
    std::fs::write(map.join("1_0.zip"), b"PK\x03\x04not really a zip").unwrap();

    let png = dir.join("row.png");
    let out = xt(&[
        "render",
        "--root",
        dir.to_str().unwrap(),
        "--all",
        "-o",
        png.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "render aborted: {out:?}");
    let msg = String::from_utf8_lossy(&out.stderr);
    assert!(msg.contains("1 region(s) could not be decoded"), "{msg}");
    let (w, h, px) = read_png(&png);
    assert_eq!((w, h), (1024, 512));
    // The right half — region 1_0 — is the hole, and it is fully transparent.
    for y in 0..h as usize {
        let row = &px[y * 1024 * 4..(y + 1) * 1024 * 4];
        assert!(
            row[512 * 4..].iter().all(|&b| b == 0),
            "row {y} of the hole is not transparent"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires corpus (XAERO_CORPUS)"]
fn stats_json_describes_the_corpus() {
    let root = corpus_root_1218();
    let out = xt(&[
        "stats",
        "--root",
        root.to_str().unwrap(),
        "--world",
        "Multiplayer_2b2t",
        "--json",
    ]);
    assert!(out.status.success(), "stats failed: {out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stats json");
    let world = &v["worlds"][0];
    assert_eq!(world["world"], "Multiplayer_2b2t");
    let end = world["layers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["dim"] == "DIM1" && l["layer"] == "surface")
        .expect("DIM1 surface layer");
    assert_eq!(end["regions"], 4);
    assert_eq!(end["bounds"], serde_json::json!([-1, -1, 0, 0]));
    assert_eq!(end["versions"]["7.8"], 4);
    assert!(end["chunksExplored"].as_u64().unwrap() > 0);
    assert!(v["totals"]["bytes"].as_u64().unwrap() > 0);
    // The highlight DBs are counted read-only.
    assert!(
        world["databases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["db"] == "XaeroPlusNewChunks.db")
    );
    // Waypoints come from the minimap tree, which has 8 in the Nether file.
    assert_eq!(world["waypoints"]["waypoints"], 8);
    assert_eq!(world["waypoints"]["files"], 2);
    assert_eq!(v["totals"]["waypoints"]["waypoints"], 8);
    // The whole-archive version histogram sums the per-layer ones.
    assert_eq!(v["mode"], "sample");
    let read = v["totals"]["versionHeadersRead"].as_u64().unwrap();
    let hist: u64 = v["totals"]["versions"]
        .as_object()
        .unwrap()
        .values()
        .map(|n| n.as_u64().unwrap())
        .sum();
    assert_eq!(read, hist);
    assert!(hist >= 4, "{hist}");
}

#[test]
#[ignore = "requires corpus (XAERO_CORPUS)"]
fn doctor_is_quiet_on_a_healthy_corpus() {
    let root = corpus_root_1218();
    let out = xt(&[
        "doctor",
        "--root",
        root.to_str().unwrap(),
        "--full",
        "--json",
    ]);
    assert!(out.status.success(), "doctor failed to run: {out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("doctor json");
    assert_eq!(v["findings"], 0, "doctor flagged the corpus: {v}");
    assert_eq!(v["regionsChecked"], v["regionsTotal"]);
}

#[test]
fn doctor_names_every_way_a_region_can_be_bad() {
    let dir = scratch("doctor");
    let map = dir.join("world-map/Multiplayer_test/null/mw$default");
    std::fs::create_dir_all(&map).expect("scratch world");
    // 0_0: a valid, tiny 7.8 region (header + one absent-tile chunk).
    let mut good = vec![0xFFu8];
    good.extend_from_slice(&((7i32 << 16) | 8).to_be_bytes());
    good.push(0x00); // chunk marker (0,0)
    good.extend(std::iter::repeat_n(0xFFu8, 16 * 4)); // 16 tiles, all "absent"
    std::fs::write(map.join("0_0.xaero"), &good).unwrap();
    // 1_0: same stream cut mid-chunk -> decodes, truncated = true.
    std::fs::write(map.join("1_0.xaero"), &good[..good.len() - 10]).unwrap();
    // 2_0: a save version newer than the codec accepts.
    let mut future = vec![0xFFu8];
    future.extend_from_slice(&((8i32 << 16) | 8).to_be_bytes());
    std::fs::write(map.join("2_0.xaero"), future).unwrap();
    // 3_0: nothing at all — reported from the index, never decoded.
    std::fs::write(map.join("3_0.xaero"), b"").unwrap();
    // 4_0: non-empty rubbish that is neither a zip nor a region stream.
    std::fs::write(map.join("4_0.zip"), b"PK\x03\x04not really a zip").unwrap();

    let out = xt(&[
        "doctor",
        "--root",
        dir.to_str().unwrap(),
        "--full",
        "--json",
    ]);
    // Findings are not errors — a survey that ran is a success.
    assert_eq!(out.status.code(), Some(0), "doctor should exit 0: {out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("doctor json");
    // The zero-byte file is counted from the index, so only four are decoded.
    let layer = &v["worlds"][0]["layers"][0];
    assert_eq!(v["regionsChecked"], 4);
    assert_eq!(v["findings"], 4);
    let issues = layer["issues"].as_array().unwrap();
    let kinds: Vec<&str> = issues.iter().map(|i| i["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"empty file"), "{kinds:?}");
    assert!(kinds.contains(&"truncated"), "{kinds:?}");
    assert!(kinds.contains(&"unsupported"), "{kinds:?}");
    assert!(kinds.contains(&"unreadable"), "{kinds:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A region Xaero moved into a `<version>_backup_<n>` dir and never wrote back
/// is the only copy of that map square, and the live index cannot see it.
#[test]
fn doctor_finds_regions_that_survive_only_as_copies() {
    let dir = scratch("alternates");
    let map = dir.join("world-map/Multiplayer_test/null/mw$default");
    std::fs::create_dir_all(map.join("393224_backup_0")).expect("scratch world");
    let mut region = vec![0xFFu8];
    region.extend_from_slice(&((7i32 << 16) | 8).to_be_bytes());
    region.push(0x00); // chunk marker (0,0)
    region.extend(std::iter::repeat_n(0xFFu8, 16 * 4)); // 16 tiles, all "absent"
    // 0_0 is live; 7_7 exists only inside the backup dir.
    std::fs::write(map.join("0_0.xaero"), &region).unwrap();
    std::fs::write(map.join("393224_backup_0/0_0.xaero"), &region).unwrap();
    std::fs::write(map.join("393224_backup_0/7_7.xaero"), &region).unwrap();
    // A Syncthing conflict copy beside the live file.
    std::fs::write(
        map.join("0_0.sync-conflict-20240705-215039-QQNGROR.xaero"),
        &region,
    )
    .unwrap();

    let out = xt(&[
        "doctor",
        "--root",
        dir.to_str().unwrap(),
        "--full",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(0), "doctor should exit 0: {out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("doctor json");
    // Only the live 0_0 is a region; the copies are found by name, not decoded.
    assert_eq!(v["regionsChecked"], 1);
    let issues = v["worlds"][0]["layers"][0]["issues"].as_array().unwrap();
    let backup = issues
        .iter()
        .find(|i| i["kind"] == "backup only")
        .expect("backup-only finding");
    assert_eq!(backup["count"], 1, "0_0 has a live copy, 7_7 does not");
    assert!(
        backup["examples"][0]
            .as_str()
            .unwrap()
            .ends_with("7_7.xaero"),
        "{backup}"
    );
    let conflict = issues
        .iter()
        .find(|i| i["kind"] == "sync conflict")
        .expect("sync-conflict finding");
    assert_eq!(conflict["count"], 1);
    let _ = std::fs::remove_dir_all(&dir);
}
