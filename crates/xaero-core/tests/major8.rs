//! Save major 8: regions written by Xaero's World Map for Minecraft 26.3.
//!
//! 26.3 renamed the root keys of a block state's NBT from `Name`/`Properties`
//! to `id`/`properties`, and the mod marks regions written that way 8.8. The
//! body framing is otherwise 7.8's. The two committed fixtures are the header
//! plus the first tile chunk of real regions the 26.3 build wrote on 2b2t (an
//! overworld one with leaves, logs and non-water overlays, and a Nether one),
//! cut at the chunk boundary so the stream bytes are the game's own.

use std::path::{Path, PathBuf};

use xaero_core::codec::{decode_region, encode_region, read_region_container};

fn fixtures() -> Vec<(PathBuf, Vec<u8>)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/major8");
    let mut out: Vec<(PathBuf, Vec<u8>)> = std::fs::read_dir(&dir)
        .expect("fixture dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("zip"))
        .map(|p| {
            let bytes = std::fs::read(&p).expect("read fixture");
            let stream = read_region_container(&bytes).expect("container");
            (p, stream)
        })
        .collect();
    out.sort();
    assert_eq!(out.len(), 2, "expected the two major-8 fixtures");
    out
}

/// The same bytes under a 7.8 header: the framing is identical, so this reads
/// every palette entry verbatim, exactly as 26.3 wrote it.
fn as_7_8(stream: &[u8]) -> Vec<u8> {
    let mut s = stream.to_vec();
    s[1..5].copy_from_slice(&((7i32 << 16) | 8).to_be_bytes());
    s
}

/// The mod's own 7 -> 8 fix (`OldFormatSupport.fixBlockState262_263`) on one
/// of our respelled entries: the root keys go back in place. Both are the
/// first occurrence, since a state's properties hold only string values.
fn game_fix(state: &[u8]) -> Vec<u8> {
    fn replace_first(v: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
        match v.windows(from.len()).position(|w| w == from) {
            Some(i) => [&v[..i], to, &v[i + from.len()..]].concat(),
            None => v.to_vec(),
        }
    }
    let renamed = replace_first(state, b"\x08\x00\x04Name", b"\x08\x00\x02id");
    replace_first(
        &renamed,
        b"\x0a\x00\x0aProperties",
        b"\x0a\x00\x0aproperties",
    )
}

#[test]
fn fixtures_decode_cleanly_as_8_8() {
    for (path, stream) in fixtures() {
        let dr = decode_region(&stream).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(
            (dr.version.major, dr.version.minor),
            (8, 8),
            "{}",
            path.display()
        );
        assert!(
            !dr.truncated && dr.trailing == 0,
            "{}: truncated={} trailing={}",
            path.display(),
            dr.truncated,
            dr.trailing
        );
        assert_eq!(dr.region.chunks.len(), 1, "{}", path.display());
        assert!(!dr.palettes.states.is_empty(), "{}", path.display());
        for name in &dr.palettes.state_names {
            assert!(
                name.starts_with("minecraft:"),
                "{}: state named {name:?}",
                path.display()
            );
        }
    }
}

#[test]
fn palette_entries_are_the_games_own_rename_reversed() {
    for (path, stream) in fixtures() {
        let v8 = decode_region(&stream).unwrap();
        let verbatim = decode_region(&as_7_8(&stream)).unwrap();
        assert_eq!(v8.region, verbatim.region, "{}", path.display());
        assert_eq!(v8.palettes.biomes, verbatim.palettes.biomes);
        assert_eq!(v8.palettes.states.len(), verbatim.palettes.states.len());
        let mut with_properties = 0;
        for (i, (ours, theirs)) in v8
            .palettes
            .states
            .iter()
            .zip(&verbatim.palettes.states)
            .enumerate()
        {
            assert!(
                theirs.windows(5).any(|w| w == b"\x08\x00\x02id"),
                "{} state {i}: fixture is not 26.3-keyed",
                path.display()
            );
            assert!(
                !ours.windows(5).any(|w| w == b"\x08\x00\x02id"),
                "{} state {i}: id key was not respelled",
                path.display()
            );
            assert_eq!(&game_fix(ours), theirs, "{} state {i}", path.display());
            with_properties += usize::from(ours.windows(10).any(|w| w == b"Properties"));
        }
        assert!(
            with_properties > 0,
            "{}: no state carries properties",
            path.display()
        );
    }
}

#[test]
fn body_framing_is_7_8s_byte_for_byte() {
    for (path, stream) in fixtures() {
        let view = as_7_8(&stream);
        let dr = decode_region(&view).unwrap();
        assert_eq!(encode_region(&dr), view, "{}", path.display());
    }
}

#[test]
fn reencodes_as_stable_7_8_with_the_same_content() {
    for (path, stream) in fixtures() {
        let v8 = decode_region(&stream).unwrap();
        let enc = encode_region(&v8);
        let back = decode_region(&enc).unwrap();
        assert_eq!((back.version.major, back.version.minor), (7, 8));
        assert!(!back.truncated && back.trailing == 0, "{}", path.display());
        assert_eq!(back.region, v8.region, "{}", path.display());
        assert_eq!(back.palettes, v8.palettes, "{}", path.display());
        assert_eq!(encode_region(&back), enc, "{}", path.display());
    }
}
