//! Region stream encoder. Always emits the current format: major 7, minor 8.
//!
//! Palette handling: indices in the file are region-local, assigned in first
//! appearance order. The encoder walks pixels in serialization order and
//! rebuilds both palettes, re-emitting each entry's **raw bytes** (as decoded)
//! the first time it is referenced, with the pixel's bit 21/22 (or overlay
//! bit 10) set. For an unmodified major-7 region this reproduces the input
//! stream byte-for-byte; merged regions get fresh consistent palettes.

use super::{FormatVersion, WrExt};
use crate::model::*;

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
        let mut params = px.params & !(P_STATE_NEW | P_BIOME_NEW);
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
        // A legacy pixel can carry a biome that its params never flagged (the
        // slope field forced the read). 7.8 has no slope field, so the flag has
        // to be set explicitly or the biome would be dropped on re-encode.
        if px.biome.is_some() {
            params |= P_BIOME;
        }
        // Bits 2-3 and 4 are inert under 7.8 framing, but leaving them set
        // would mislead anything that reads this file as legacy.
        params &= !(P_LEGACY_SLOPE | P_VERTICAL_SLOPE);
        self.out.w_i32(params as i32);

        if params & P_NOT_GRASS != 0
            && let Some(old) = px.state
        {
            self.emit_state(old, pal);
        }
        if let Some(h) = px.legacy_height {
            self.out.w_u8(h);
        }
        if let Some(t) = px.top_height {
            self.out.w_u8(t);
        }
        if params & P_HAS_OVERLAYS != 0 {
            self.out.w_u8(px.overlays.len() as u8);
            for ov in &px.overlays {
                self.overlay(ov, pal);
            }
        }
        if params & P_BIOME != 0 {
            match px.biome {
                Some(BiomeRef::Palette(old)) => {
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
                Some(BiomeRef::LegacyNumeric(v)) => self.out.w_i32(v),
                None => {
                    // params claim a biome but the model has none (corrupt
                    // source); keep the stream self-consistent.
                    self.out.w_i32(0);
                }
            }
        }
    }

    fn overlay(&mut self, ov: &Overlay, pal: &Palettes) {
        let mut params = ov.params & !O_STATE_NEW;
        let is_new = matches!(ov.state, Some(old)
            if (old as usize) < self.state_map.len() && self.state_map[old as usize].is_none());
        if is_new {
            params |= O_STATE_NEW;
        }
        if let Some(op) = ov.legacy_opacity {
            // Decoded from a minor<8 file (bit 3 + trailing i32). A 7.8 reader
            // never consumes that i32, so fold the value into the packed
            // opacity bits 11-14 and drop the legacy flag.
            params &= !O_LEGACY_OPACITY;
            params = (params & !(0xF << 11)) | ((op.clamp(0, 15) as u32) << 11);
        }
        self.out.w_i32(params as i32);
        if params & O_NOT_WATER != 0
            && let Some(old) = ov.state
        {
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
