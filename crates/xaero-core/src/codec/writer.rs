//! Region stream encoder. Always emits the current format: major 7, minor 8.
//!
//! Palette handling: indices in the file are region-local, assigned in first
//! appearance order. The encoder walks pixels in serialization order and
//! rebuilds both palettes, re-emitting each entry's **raw bytes** (as decoded)
//! the first time it is referenced, with the pixel's bit 21/22 (or overlay
//! bit 10) set. For an unmodified major-7 region this reproduces the input
//! stream byte-for-byte; merged regions get fresh consistent palettes.
//!
//! Every other field the 7.8 loader derives from a params word is rebuilt
//! from the typed model rather than copied: a pixel read from an older framing
//! (height nibble at bit 24, a bit-6 height byte, bit-23 numeric biomes,
//! overlay words with the legacy extra-i32 flags) would otherwise be framed
//! for the wrong reader and desynchronise the stream.

use super::{FormatVersion, WrExt};
use crate::model::*;

/// Pixel params bits carried over from the source word: light (bits 8-11)
/// and the bits no writer has ever used. Everything else is rebuilt.
const P_PASSTHROUGH: u32 = !(P_NOT_GRASS
    | P_HAS_OVERLAYS
    | P_LEGACY_SLOPE
    | P_VERTICAL_SLOPE
    | P_LEGACY_HEIGHT_BYTE
    | (0xFF << 12)
    | P_BIOME
    | P_STATE_NEW
    | P_BIOME_NEW
    | P_BIOME_NUMERIC
    | P_TOP_HEIGHT
    | (0xF << 25));

/// Overlay params bits carried over: light (4-7), packed opacity (11-14) and
/// the unused bits. Bits 1, 2 and 8-9 are legacy flags the 7.8 loader still
/// honours — each burns an i32 this writer never emits — so they are rebuilt
/// (cleared) along with bit 3 and the palette flag.
const O_PASSTHROUGH: u32 =
    !(O_NOT_WATER | (1 << 1) | (1 << 2) | O_LEGACY_OPACITY | (3 << 8) | O_STATE_NEW);

pub fn encode_region(dr: &DecodedRegion) -> Vec<u8> {
    // Rough pre-size: header + chunks; pixels dominate at ~5 bytes each.
    let mut out = Vec::with_capacity(1024 + dr.region.chunks.len() * 16 * 256 * 6);
    out.w_u8(0xFF);
    out.w_i32(
        FormatVersion {
            major: crate::WRITE_MAJOR,
            minor: crate::WRITE_MINOR,
        }
        .packed(),
    );

    let mut enc = Enc {
        out,
        state_map: vec![None; dr.palettes.states.len()],
        biome_map: vec![None; dr.palettes.biomes.len()],
        next_state: 0,
        next_biome: 0,
    };

    for (marker, chunk) in &dr.region.chunks {
        enc.out.w_u8(*marker);
        for slot in &chunk.tiles {
            match slot {
                None => enc.out.w_i32(-1),
                Some(tile) => enc.tile(tile, &dr.palettes),
            }
        }
    }
    enc.out
}

struct Enc {
    out: Vec<u8>,
    /// old palette index -> new palette index (assigned on first emission)
    state_map: Vec<Option<u32>>,
    biome_map: Vec<Option<u32>>,
    next_state: u32,
    next_biome: u32,
}

impl Enc {
    fn tile(&mut self, tile: &Tile, pal: &Palettes) {
        for px in &tile.pixels {
            self.pixel(px, pal);
        }
        self.out.w_u8(tile.interp_version);
        self.out.w_i32(tile.cave_start);
        self.out.w_u8(tile.cave_depth);
    }

    fn pixel(&mut self, px: &Pixel, pal: &Palettes) {
        let mut params = px.params & P_PASSTHROUGH;
        if px.state.is_some() {
            params |= P_NOT_GRASS;
        }
        // The game flags overlays even when the count it then writes is
        // zero, so an empty list keeps the source flag (and its zero byte).
        if !px.overlays.is_empty() || px.params & P_HAS_OVERLAYS != 0 {
            params |= P_HAS_OVERLAYS;
        }
        // Signed 12 bits at the minor-8 positions: low byte at 12, high
        // nibble at 25. A bit-6 height byte from a legacy file lands here too.
        let h = (px.height as i32 as u32) & 0xFFF;
        params |= ((h & 0xFF) << 12) | ((h >> 8) << 25);
        if px.top_height.is_some() {
            params |= P_TOP_HEIGHT;
        }
        // A legacy pixel can carry a biome that its params never flagged (the
        // slope field forced the read). 7.8 has no slope field, so the flag
        // comes from the model or the biome would be dropped on re-encode.
        if px.biome.is_some() {
            params |= P_BIOME;
        }
        let state_is_new = matches!(px.state, Some(old)
            if (old as usize) < self.state_map.len() && self.state_map[old as usize].is_none());
        if state_is_new {
            params |= P_STATE_NEW;
        }
        let biome_is_new = matches!(px.biome, Some(BiomeRef::Palette(old))
            if (old as usize) < self.biome_map.len() && self.biome_map[old as usize].is_none());
        if biome_is_new {
            params |= P_BIOME_NEW;
        }
        self.out.w_i32(params as i32);

        if let Some(old) = px.state {
            self.emit_state(old, pal);
        }
        if let Some(t) = px.top_height {
            self.out.w_u8(t);
        }
        if params & P_HAS_OVERLAYS != 0 {
            // The loader reads a byte count; the game itself never writes
            // more than ten, so anything past 255 is a broken model, not data.
            let n = px.overlays.len().min(u8::MAX as usize);
            self.out.w_u8(n as u8);
            for ov in px.overlays.iter().take(n) {
                self.overlay(ov, pal);
            }
        }
        if let Some(BiomeRef::Palette(old)) = px.biome {
            let oldi = old as usize;
            if oldi < self.biome_map.len() {
                match self.biome_map[oldi] {
                    Some(new) => self.out.w_i32(new as i32),
                    None => {
                        let raw = &pal.biomes[oldi];
                        self.out.w_u16(raw.len() as u16);
                        self.out.extend_from_slice(raw);
                        self.biome_map[oldi] = Some(self.next_biome);
                        self.next_biome += 1;
                    }
                }
            } else {
                // Dangling index in a corrupt source: pass through.
                self.out.w_i32(old as i32);
            }
        }
    }

    fn overlay(&mut self, ov: &Overlay, pal: &Palettes) {
        let mut params = ov.params & O_PASSTHROUGH;
        if ov.state.is_some() {
            params |= O_NOT_WATER;
        }
        let is_new = matches!(ov.state, Some(old)
            if (old as usize) < self.state_map.len() && self.state_map[old as usize].is_none());
        if is_new {
            params |= O_STATE_NEW;
        }
        if let Some(op) = ov.legacy_opacity {
            // Decoded from a minor<8 file (bit 3 + trailing i32). A 7.8 reader
            // never consumes that i32, so fold the value into the packed
            // opacity bits 11-14; the legacy flag is already gone.
            params = (params & !(0xF << 11)) | ((op.clamp(0, 15) as u32) << 11);
        }
        self.out.w_i32(params as i32);
        if let Some(old) = ov.state {
            self.emit_state(old, pal);
        }
    }

    fn emit_state(&mut self, old: u32, pal: &Palettes) {
        let oldi = old as usize;
        if oldi < self.state_map.len() {
            match self.state_map[oldi] {
                Some(new) => self.out.w_i32(new as i32),
                None => {
                    self.out.extend_from_slice(&pal.states[oldi]);
                    self.state_map[oldi] = Some(self.next_state);
                    self.next_state += 1;
                }
            }
        } else {
            self.out.w_i32(old as i32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::decode_region;

    /// One chunk (marker 0) whose first tile holds `pixels` (each a hand-built
    /// pixel record, params word included) padded with plain grass pixels,
    /// then the tile trailer and fifteen absent tiles.
    fn stream(major: u16, minor: u16, pixels: &[Vec<u8>], trailer: &[u8]) -> Vec<u8> {
        assert!(
            major != 2,
            "major 2 carries a flagV2 byte this helper does not write"
        );
        let mut s = vec![0xFF];
        s.extend_from_slice(&FormatVersion { major, minor }.packed().to_be_bytes());
        s.push(0x00);
        for i in 0..256 {
            match pixels.get(i) {
                Some(p) => s.extend_from_slice(p),
                None => s.extend_from_slice(&0i32.to_be_bytes()),
            }
        }
        s.extend_from_slice(trailer);
        for _ in 0..15 {
            s.extend_from_slice(&(-1i32).to_be_bytes());
        }
        s
    }

    fn params(bits: u32) -> Vec<u8> {
        (bits as i32).to_be_bytes().to_vec()
    }

    fn round_trip(s: &[u8]) -> (DecodedRegion, DecodedRegion) {
        let before = decode_region(s).expect("decode");
        assert!(
            !before.truncated && before.trailing == 0,
            "hand-built stream must decode cleanly"
        );
        let enc = encode_region(&before);
        let after = decode_region(&enc).expect("re-decode");
        assert!(
            !after.truncated && after.trailing == 0,
            "re-encoded stream desynchronised: truncated={} trailing={}",
            after.truncated,
            after.trailing
        );
        assert_eq!(after.version.major, crate::WRITE_MAJOR);
        assert_eq!(after.version.minor, crate::WRITE_MINOR);
        (before, after)
    }

    fn tile(dr: &DecodedRegion) -> &Tile {
        dr.region.chunks[0].1.tiles[0].as_ref().expect("tile 0")
    }

    /// Minor < 4 keeps the height's high nibble at bit 24, where 7.8 keeps the
    /// top-height flag. Height 300 has that bit set: passed through, the 7.8
    /// reader would eat a phantom top-height byte and desynchronise.
    #[test]
    fn legacy_height_nibble_moves_to_bit_25() {
        let h = 300u32; // 0x12C
        let word = ((h & 0xFF) << 12) | ((h >> 8) << 24);
        let s = stream(3, 3, &[params(word), params(word)], &[]);
        let (before, after) = round_trip(&s);
        assert_eq!(tile(&before).pixels[0].height, 300);
        assert_eq!(tile(&before).pixels[0].top_height, None);
        let px = &tile(&after).pixels[0];
        assert_eq!(px.height, 300);
        assert_eq!(px.top_height, None);
        assert_eq!(px.params & P_TOP_HEIGHT, 0);
        assert_eq!((px.params >> 25) & 0xF, 1);
    }

    /// A bit-6 height byte is folded into the packed field; a negative height
    /// survives the sign extension both ways.
    #[test]
    fn legacy_height_byte_and_negative_heights_survive() {
        let mut byte_px = params(P_LEGACY_HEIGHT_BYTE);
        byte_px.push(200);
        let neg = (-40i32 as u32) & 0xFFF;
        let neg_px = params(((neg & 0xFF) << 12) | ((neg >> 8) << 24));
        let s = stream(1, 1, &[byte_px, neg_px], &[]);
        let (before, after) = round_trip(&s);
        assert_eq!(tile(&before).pixels[0].height, 200);
        assert_eq!(tile(&before).pixels[1].height, -40);
        assert_eq!(tile(&after).pixels[0].height, 200);
        assert_eq!(tile(&after).pixels[0].params & P_LEGACY_HEIGHT_BYTE, 0);
        assert_eq!(tile(&after).pixels[0].legacy_height, None);
        assert_eq!(tile(&after).pixels[1].height, -40);
    }

    /// Overlay bit 2 makes every loader burn an i32 after the word. The
    /// decoder consumed it from the legacy file; the encoder must not leave
    /// the flag on a word it writes no i32 behind.
    #[test]
    fn legacy_overlay_extra_word_flag_is_cleared() {
        let mut px = params(P_HAS_OVERLAYS);
        px.push(1); // one overlay
        px.extend_from_slice(&(((1u32 << 2) | (5 << 4)) as i32).to_be_bytes());
        px.extend_from_slice(&0xDEADBEEFu32.to_be_bytes()); // the burned i32
        // minor 7 trailer: interp u8, cave_start i32, cave_depth u8
        let mut trailer = vec![0u8];
        trailer.extend_from_slice(&i32::MAX.to_be_bytes());
        trailer.push(32);
        let s = stream(5, 7, &[px], &trailer);
        let (before, after) = round_trip(&s);
        let ob = &tile(&before).pixels[0].overlays[0];
        assert_eq!(ob.legacy_opacity, Some(1));
        assert_eq!(ob.light(), 5);
        let oa = &tile(&after).pixels[0].overlays[0];
        assert_eq!(oa.params & (1 << 2), 0);
        assert_eq!(oa.light(), 5);
        assert_eq!(oa.effective_opacity(), 1);
        assert_eq!(oa.state, None);
        assert_eq!(tile(&after).cave_start, i32::MAX);
    }

    /// Majors 4-5 wrote new biome entries as numeric ids (bit 22 + bit 23).
    /// Two ids that resolve to one modern name are still two palette entries
    /// in the game, so a later index must line up; and the re-encoded stream
    /// writes them as names without bit 23.
    #[test]
    fn numeric_new_biomes_keep_one_entry_per_occurrence() {
        let new_numeric = P_BIOME | P_BIOME_NEW | P_BIOME_NUMERIC;
        let mut p0 = params(new_numeric);
        p0.extend_from_slice(&12i32.to_be_bytes()); // snowy_plains
        let mut p1 = params(new_numeric);
        p1.extend_from_slice(&13i32.to_be_bytes()); // also snowy_plains
        let mut p2 = params(new_numeric);
        p2.extend_from_slice(&4i32.to_be_bytes()); // forest
        let mut p3 = params(P_BIOME);
        p3.extend_from_slice(&2i32.to_be_bytes()); // index 2 = forest
        let s = stream(4, 4, &[p0, p1, p2, p3], &[0]);
        let (before, after) = round_trip(&s);
        assert_eq!(
            before.palettes.biome_names,
            vec![
                "minecraft:snowy_plains",
                "minecraft:snowy_plains",
                "minecraft:forest"
            ]
        );
        assert_eq!(
            tile(&before).pixels[3].biome,
            Some(BiomeRef::Palette(2)),
            "index 2 must be forest, not dangling"
        );
        for (i, want) in [
            "minecraft:snowy_plains",
            "minecraft:snowy_plains",
            "minecraft:forest",
            "minecraft:forest",
        ]
        .iter()
        .enumerate()
        {
            let px = &tile(&after).pixels[i];
            let Some(BiomeRef::Palette(b)) = px.biome else {
                panic!("pixel {i} lost its biome");
            };
            assert_eq!(after.palettes.biome_names[b as usize], *want, "pixel {i}");
            assert_eq!(px.params & P_BIOME_NUMERIC, 0);
        }
    }

    /// A hand-built pixel whose word claims a state or overlays it does not
    /// carry encodes as what the model says, never as a word with nothing
    /// behind it.
    #[test]
    fn flags_follow_the_model_not_the_word() {
        let mut dr = decode_region(&stream(7, 8, &[], &[0, 0x7F, 0xFF, 0xFF, 0xFF, 32])).unwrap();
        {
            let t = dr.region.chunks[0].1.tiles[0].as_mut().unwrap();
            t.pixels[0] = Pixel {
                params: P_NOT_GRASS | P_HAS_OVERLAYS | P_BIOME | P_TOP_HEIGHT,
                state: None,
                height: 7,
                legacy_height: None,
                top_height: None,
                overlays: smallvec::SmallVec::new(),
                biome: None,
            };
            t.pixels[1] = Pixel {
                params: 0,
                state: None,
                height: 0,
                legacy_height: None,
                top_height: None,
                overlays: std::iter::repeat_n(
                    Overlay {
                        params: 0,
                        state: None,
                        legacy_opacity: None,
                    },
                    300,
                )
                .collect(),
                biome: None,
            };
        }
        let after = decode_region(&encode_region(&dr)).unwrap();
        assert!(!after.truncated && after.trailing == 0);
        let p0 = &tile(&after).pixels[0];
        assert!(p0.is_grass());
        assert_eq!(p0.height, 7);
        assert_eq!(p0.top_height, None);
        assert_eq!(p0.biome, None);
        assert_eq!(tile(&after).pixels[1].overlays.len(), 255);
    }
}
