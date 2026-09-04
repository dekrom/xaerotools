//! Round-trip the entire sample-data corpus (1563 region files, majors 6 & 7).
//!
//! Gate (plan M1):
//!  - every region decodes with truncated == false and trailing == 0
//!  - major 7 inputs re-encode **byte-identically**
//!  - major 6 inputs re-encode identically except the 5-byte header (6.8 and
//!    7.8 share the same body layout), and survive decode(encode(x)) with
//!    semantic equality
//!
//! The corpus lives outside the repo. These tests are `#[ignore]`d, so a plain
//! `cargo test` reports them as skipped and never as passed. Run them with
//! `--ignored` and XAERO_CORPUS=/path/to/sample-data.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;
use xaero_core::{decode_region, encode_region, read_region_container};

fn is_region_file(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("zip") {
        return false;
    }
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    let mut parts = stem.splitn(2, '_');
    let (Some(a), Some(b)) = (parts.next(), parts.next()) else {
        return false;
    };
    a.parse::<i64>().is_ok() && b.parse::<i64>().is_ok()
}

fn in_cache_dir(path: &Path) -> bool {
    path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        s == "cache" || s == "caches" || s.starts_with("cache_")
    })
}

#[test]
#[ignore = "requires corpus (XAERO_CORPUS)"]
fn corpus_round_trip() {
    let root = test_support::corpus_root().expect("XAERO_CORPUS");
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| is_region_file(p) && !in_cache_dir(p))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no region files under {}",
        root.display()
    );

    let major6 = AtomicUsize::new(0);
    let major7 = AtomicUsize::new(0);
    let started = std::time::Instant::now();

    let failures: Vec<String> = files
        .par_iter()
        .filter_map(|path| {
            let check = || -> Result<(), String> {
                let bytes = std::fs::read(path).map_err(|e| format!("read: {e}"))?;
                let stream =
                    read_region_container(&bytes).map_err(|e| format!("container: {e}"))?;
                let dec = decode_region(&stream).map_err(|e| format!("decode: {e}"))?;
                if dec.truncated {
                    return Err("decoder reported truncation".into());
                }
                if dec.trailing != 0 {
                    return Err(format!("{} trailing bytes after parse", dec.trailing));
                }
                let enc = encode_region(&dec);
                match dec.version.major {
                    7 => {
                        if enc != stream {
                            let at = enc
                                .iter()
                                .zip(stream.iter())
                                .position(|(a, b)| a != b)
                                .unwrap_or(enc.len().min(stream.len()));
                            return Err(format!(
                                "major-7 re-encode differs (len {} -> {}, first diff at {at})",
                                stream.len(),
                                enc.len()
                            ));
                        }
                        major7.fetch_add(1, Ordering::Relaxed);
                    }
                    6 => {
                        if enc.len() != stream.len() || enc[5..] != stream[5..] {
                            let at = enc[5..]
                                .iter()
                                .zip(stream[5..].iter())
                                .position(|(a, b)| a != b)
                                .unwrap_or(0);
                            return Err(format!(
                                "major-6 body re-encode differs (len {} -> {}, first body diff at {})",
                                stream.len(),
                                enc.len(),
                                at + 5
                            ));
                        }
                        let redec = decode_region(&enc).map_err(|e| format!("re-decode: {e}"))?;
                        if redec.region != dec.region || redec.palettes != dec.palettes {
                            return Err("major-6 semantic mismatch after re-encode".into());
                        }
                        major6.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => unreachable!(),
                }
                Ok(())
            };
            check()
                .err()
                .map(|e| format!("{}: {e}", path.strip_prefix(&root).unwrap().display()))
        })
        .collect();

    eprintln!(
        "corpus: {} regions ({} major-6, {} major-7) in {:.1}s",
        files.len(),
        major6.load(Ordering::Relaxed),
        major7.load(Ordering::Relaxed),
        started.elapsed().as_secs_f32()
    );
    if !failures.is_empty() {
        for f in failures.iter().take(20) {
            eprintln!("FAIL {f}");
        }
        panic!(
            "{} of {} regions failed round-trip",
            failures.len(),
            files.len()
        );
    }
    assert!(
        major6.load(Ordering::Relaxed) > 0,
        "expected major-6 samples"
    );
    assert!(
        major7.load(Ordering::Relaxed) > 0,
        "expected major-7 samples"
    );
}

#[test]
#[ignore = "requires corpus (XAERO_CORPUS)"]
fn truncation_never_panics() {
    let root = test_support::corpus_root().expect("XAERO_CORPUS");
    // One small region per major version.
    let picks = [
        "xaero1.21.4/world-map/Multiplayer_2b2t/DIM-1/mw$default/0_-24.zip",
        "xaero1.21.8/world-map/Multiplayer_2b2t.org/null/mw$default/4040_-9370.zip",
    ];
    for rel in picks {
        let path = root.join(rel);
        // Reading these two is the whole test. Skipping a missing one leaves
        // the loop body unexecuted and the test green, so name it and stop.
        let bytes = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "corpus pick is missing: {} ({e}). If the corpus layout moved, \
                 update the pick rather than letting this test pass without \
                 decoding anything.",
                path.display()
            )
        });
        let stream = read_region_container(&bytes).unwrap();
        let full = decode_region(&stream).unwrap();
        assert!(!full.truncated && full.trailing == 0, "{rel}: full decode");
        // A cut region has no way to say it was cut: the format has no
        // terminator, so a cut landing on a chunk boundary decodes as a clean,
        // shorter region by design. What every cut must satisfy is that
        // nothing panics and that whatever came back is a prefix of the
        // whole: the fully-read chunks equal the file's, and the palettes
        // (first-appearance order) are prefixes of the file's.
        let check = |cut: usize| {
            let d = decode_region(&stream[..cut]).unwrap_or_else(|e| {
                // Only the header can fail; that is the "cut inside the
                // header" case and the only Err a prefix may produce.
                assert!(cut < 5, "{rel}: cut {cut}: {e}");
                full.clone()
            });
            let n = d.region.chunks.len();
            assert!(
                n <= full.region.chunks.len(),
                "{rel}: cut {cut} grew chunks"
            );
            let complete = n - usize::from(d.truncated && n > 0);
            assert_eq!(
                &d.region.chunks[..complete],
                &full.region.chunks[..complete],
                "{rel}: cut {cut}: fully-read chunks differ from the file's"
            );
            assert!(
                full.palettes.states.starts_with(&d.palettes.states)
                    && full.palettes.biomes.starts_with(&d.palettes.biomes),
                "{rel}: cut {cut}: palettes are not a prefix"
            );
            if !d.truncated {
                assert_eq!(d.trailing, 0, "{rel}: cut {cut}: clean decode left bytes");
            }
        };
        for cut in 0..stream.len().min(4000) {
            check(cut);
        }
        // Also sparse cuts across the whole file.
        let mut cut = 0;
        while cut < stream.len() {
            check(cut);
            cut += 97;
        }
    }
}
