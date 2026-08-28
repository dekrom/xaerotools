//! Legacy (major-0) region decoding.
//!
//! Roughly half of a long-lived 2b2t world map is stored in the pre-palette
//! format that Xaero shipped before block states became NBT: a numeric 1.12
//! block id + meta per pixel and a numeric biome id. The two committed
//! fixtures are real regions taken from such an archive, one of each minor
//! (0.4 and 0.7).
//!
//! Point `XAERO_LEGACY_CORPUS` at a directory of region zips to run the same
//! assertions across a larger private archive. That sweep is `#[ignore]`d, so
//! it never runs by accident, and it stops with a clear error rather than
//! reporting a pass when the archive is absent.

use std::path::{Path, PathBuf};

use xaero_core::codec::{decode_region, encode_region, read_region_container};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy")
}

fn decode_file(path: &Path) -> xaero_core::model::DecodedRegion {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let stream = read_region_container(&bytes)
        .unwrap_or_else(|e| panic!("container {}: {e}", path.display()));
    decode_region(&stream).unwrap_or_else(|e| panic!("decode {}: {e}", path.display()))
}

#[test]
fn decodes_legacy_fixtures_cleanly() {
    let mut seen = Vec::new();
    for entry in std::fs::read_dir(fixture_dir()).expect("fixture dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("zip") {
            continue;
        }
        let dr = decode_file(&path);
        assert_eq!(dr.version.major, 0, "{} should be major 0", path.display());
        assert!(
            !dr.truncated,
            "{} decoded as truncated: {} bytes left",
            path.display(),
            dr.trailing
        );
        assert_eq!(
            dr.trailing,
            0,
            "{} left trailing bytes — the stream desynchronised",
            path.display()
        );
        assert!(
            !dr.region.chunks.is_empty(),
            "{} decoded no chunks",
            path.display()
        );
        seen.push(dr.version.minor);
    }
    seen.sort_unstable();
    assert_eq!(seen, vec![4, 7], "expected one 0.4 and one 0.7 fixture");
}

#[test]
fn legacy_ids_resolve_to_real_block_and_biome_names() {
    for entry in std::fs::read_dir(fixture_dir()).expect("fixture dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("zip") {
            continue;
        }
        let dr = decode_file(&path);
        // Legacy numeric ids are interned as ordinary palette entries, so the
        // renderer and the encoder see one uniform representation.
        assert!(
            !dr.palettes.state_names.is_empty(),
            "{}: no states interned",
            path.display()
        );
        for name in &dr.palettes.state_names {
            assert!(
                name.starts_with("minecraft:"),
                "{}: unresolved state {name:?}",
                path.display()
            );
        }
        for name in &dr.palettes.biome_names {
            assert!(
                name.starts_with("minecraft:"),
                "{}: unresolved biome {name:?}",
                path.display()
            );
        }
        // The interning must dedupe: one entry per distinct name, not per pixel.
        let mut unique = dr.palettes.state_names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            dr.palettes.state_names.len(),
            "{}: duplicate palette entries — interning is not deduping",
            path.display()
        );
    }
}

#[test]
fn legacy_regions_re_encode_to_modern_and_survive_a_round_trip() {
    for entry in std::fs::read_dir(fixture_dir()).expect("fixture dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("zip") {
            continue;
        }
        let before = decode_file(&path);
        let encoded = encode_region(&before);
        let after =
            decode_region(&encoded).unwrap_or_else(|e| panic!("re-decode {}: {e}", path.display()));

        assert_eq!(
            after.version.major,
            xaero_core::WRITE_MAJOR,
            "{}: should re-encode as the modern major",
            path.display()
        );
        assert!(
            !after.truncated && after.trailing == 0,
            "{}: re-encoded stream does not decode cleanly",
            path.display()
        );
        assert_eq!(
            before.region.chunks.len(),
            after.region.chunks.len(),
            "{}: chunk count changed",
            path.display()
        );

        // Heights, block names and biome names must survive the format change.
        for ((_, cb), (_, ca)) in before.region.chunks.iter().zip(&after.region.chunks) {
            for (tb, ta) in cb.tiles.iter().zip(&ca.tiles) {
                let (Some(tb), Some(ta)) = (tb, ta) else {
                    assert_eq!(
                        tb.is_some(),
                        ta.is_some(),
                        "{}: tile presence",
                        path.display()
                    );
                    continue;
                };
                for (pb, pa) in tb.pixels.iter().zip(&ta.pixels) {
                    assert_eq!(pb.height, pa.height, "{}: height drift", path.display());
                    let nb = pb.state.map(|i| &before.palettes.state_names[i as usize]);
                    let na = pa.state.map(|i| &after.palettes.state_names[i as usize]);
                    assert_eq!(nb, na, "{}: block name drift", path.display());
                    let bb = match pb.biome {
                        Some(xaero_core::model::BiomeRef::Palette(i)) => {
                            before.palettes.biome_names.get(i as usize)
                        }
                        _ => None,
                    };
                    let ba = match pa.biome {
                        Some(xaero_core::model::BiomeRef::Palette(i)) => {
                            after.palettes.biome_names.get(i as usize)
                        }
                        _ => None,
                    };
                    assert_eq!(bb, ba, "{}: biome name drift", path.display());
                }
            }
        }
    }
}

/// Opt-in sweep over a private archive: every region must decode to exact EOF.
#[test]
#[ignore = "requires XAERO_LEGACY_CORPUS"]
fn optional_corpus_decodes_to_exact_eof() {
    let dir = test_support::legacy_corpus_root().unwrap_or_else(|| {
        panic!(
            "XAERO_LEGACY_CORPUS is unset. This sweep reads a private archive \
             that is not part of the public sample corpus, so `--ignored` on \
             the public data alone cannot satisfy it. Either set the variable, \
             or exclude this one test with \
             `--skip optional_corpus_decodes_to_exact_eof`."
        )
    });
    let mut checked = 0usize;
    let mut exact = 0usize;
    let mut by_version: std::collections::BTreeMap<String, usize> = Default::default();
    for entry in std::fs::read_dir(&dir).expect("corpus dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("zip") {
            continue;
        }
        let dr = decode_file(&path);
        assert!(
            !dr.truncated && dr.trailing == 0,
            "{}: v{} truncated={} trailing={}",
            path.display(),
            dr.version,
            dr.truncated,
            dr.trailing
        );
        // The byte-identity oracle applies to whatever the writer emits, so
        // hold real major-7 files to it here too, not just the curated corpus.
        if dr.version.major == xaero_core::WRITE_MAJOR
            && dr.version.minor == xaero_core::WRITE_MINOR
        {
            let bytes = std::fs::read(&path).expect("re-read");
            let stream = read_region_container(&bytes).expect("container");
            assert_eq!(
                encode_region(&dr),
                stream,
                "{}: re-encode is not byte-identical",
                path.display()
            );
            exact += 1;
        }
        *by_version.entry(dr.version.to_string()).or_default() += 1;
        checked += 1;
    }
    assert!(
        checked > 0,
        "corpus dir {} had no region zips",
        dir.display()
    );
    eprintln!("decoded {checked} regions cleanly ({exact} byte-identical): {by_version:?}");
}
