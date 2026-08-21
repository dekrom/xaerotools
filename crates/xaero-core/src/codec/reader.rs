//! Region stream decoder for every save version the game can read.
//!
//! Layout (all big-endian, verified against real 2b2t data):
//! ```text
//! u8   0xFF                      versioned-file marker (absent in pre-versioned saves)
//! i32  (major << 16) | minor
//! u8   flagV2                    major == 2 && minor >= 5 only
//! repeat until EOF:              (no terminator; clean EOF after a chunk)
//!   u8  chunkMarker = (cx << 4) | cz          cx, cz in 0..8
//!   16x tiles (tx 0..4 outer, tz 0..4 inner):
//!     i32 first                  -1 = absent tile, else pixel 0's params
//!     ... 256 pixels, then tile trailers (minor-gated)
//! ```
//!
//! There is no separate "legacy format": the game has one decoder driven by two
//! orthogonal ladders, and we mirror it exactly.
//!
//! * **`minor` selects the framing** — which extra fields exist per pixel, per
//!   overlay and per tile. Everything below minor 8 has quirks.
//! * **`major` selects block/biome identity** — major 0 stores a numeric 1.12
//!   block id + meta and a numeric biome id; majors 1+ store NBT blockstates in
//!   a region palette; majors 4+ also put biomes in a palette.
//!
//! A file is accepted iff `minor <= 8 && major <= 7`, matching the mod's own
//! gate (`MapSaveLoad.loadRegion`). Real 2b2t archives contain 0.4, 0.7, 6.7,
//! 6.8 and 7.8; the legacy majors are roughly half of a long-lived world map.
//!
//! Truncation anywhere returns the partial region with `truncated = true`;
//! the game's own loader has the same tolerance.

use std::collections::HashMap;

use smallvec::SmallVec;

use super::nbt::{decode_java_utf, read_named_nbt};
use super::{CodecError, Eof, FormatVersion, Rd, legacy};
use crate::model::*;

/// The two version ladders, resolved once per region.
///
/// `minor` is an `i32` rather than the on-disk `u16` so that pre-versioned
/// files (no marker, no version word) can carry `-1` and satisfy every
/// `minor < N` gate naturally, exactly as the game's `-1` sentinel does.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Framing {
    pub minor: i32,
    pub major: u16,
    pub flag_v2: bool,
}

impl Framing {
    /// When true, pixel bits 2-3 and overlay bits 8-9 are a live "slope" field
    /// that can consume extra bytes. True for every legacy save and for any
    /// minor-4 file, false for the 6.5+/7.5+ files that dominate modern saves.
    #[inline]
    fn legacy_slope(&self) -> bool {
        self.minor < 5 || (self.major <= 2 && !self.flag_v2)
    }

    /// Bit position of the height field's high nibble.
    #[inline]
    fn height_shift(&self) -> u32 {
        if self.minor >= 4 { 25 } else { 24 }
    }
}

/// Per-region decode state: the palettes being built plus the interning maps
/// that keep legacy numeric ids from pushing one palette entry per pixel.
struct Ctx<'a> {
    palettes: &'a mut Palettes,
    legacy_states: HashMap<&'static str, u32>,
    legacy_biomes: HashMap<&'static str, u32>,
}

impl Ctx<'_> {
    /// Interns a legacy numeric blockstate as a synthetic `{Name: ...}` NBT
    /// palette entry, so downstream rendering and re-encoding treat it exactly
    /// like a modern state.
    fn legacy_state(&mut self, packed: i32) -> u32 {
        let name = legacy::block_name(packed);
        if let Some(&idx) = self.legacy_states.get(name) {
            return idx;
        }
        let idx = self
            .palettes
            .push_state(legacy::synth_state_nbt(name), name.to_string());
        self.legacy_states.insert(name, idx);
        idx
    }

    /// Interns a legacy numeric biome id as a normal biome palette entry.
    fn legacy_biome(&mut self, id: i32) -> u32 {
        let name = legacy::biome_name(id);
        if let Some(&idx) = self.legacy_biomes.get(name) {
            return idx;
        }
        let idx = self
            .palettes
            .push_biome(name.as_bytes().to_vec(), name.to_string());
        self.legacy_biomes.insert(name, idx);
        idx
    }
}

pub fn decode_region(stream: &[u8]) -> Result<DecodedRegion, CodecError> {
    let mut rd = Rd::new(stream);
    let first = rd.u8().map_err(|_| CodecError::Empty)?;

    // A pre-versioned save has no marker and no version word: the first byte is
    // already a chunk marker. The game treats these as minor -1, major 0.
    let (framing, pending) = if first == 0xFF {
        let packed = rd.i32().map_err(|_| CodecError::TruncatedHeader)?;
        let major = ((packed >> 16) & 0xFFFF) as u16;
        let minor = packed & 0xFFFF;
        let flag_v2 = if major == 2 && minor >= 5 {
            rd.u8().map_err(|_| CodecError::TruncatedHeader)? == 1
        } else {
            false
        };
        if minor > 8 || major > 7 {
            return Err(CodecError::unsupported(FormatVersion {
                major,
                minor: minor as u16,
            }));
        }
        (
            Framing {
                minor,
                major,
                flag_v2,
            },
            None,
        )
    } else {
        (
            Framing {
                minor: -1,
                major: 0,
                flag_v2: false,
            },
            Some(first),
        )
    };

    let mut out = DecodedRegion {
        // Pre-versioned files report as 0.0; every other file reports what the
        // header said.
        version: FormatVersion {
            major: framing.major,
            minor: framing.minor.max(0) as u16,
        },
        region: Region::default(),
        palettes: Palettes::default(),
        truncated: false,
        trailing: 0,
    };

    let mut ctx = Ctx {
        palettes: &mut out.palettes,
        legacy_states: HashMap::new(),
        legacy_biomes: HashMap::new(),
    };
    let mut pending = pending;

    loop {
        let marker = match pending.take() {
            Some(m) => m,
            None => {
                // A clean region ends exactly at a would-be chunk marker.
                if rd.remaining() == 0 {
                    break;
                }
                match rd.u8() {
                    Ok(m) => m,
                    Err(Eof) => break,
                }
            }
        };
        if marker & 0x88 != 0 {
            // Not a valid (cx<<4)|cz marker: corruption. Stop; report leftovers.
            out.truncated = true;
            out.trailing = rd.remaining() + 1;
            return Ok(out);
        }
        match read_chunk(&mut rd, framing, &mut ctx) {
            Ok(chunk) => out.region.chunks.push((marker, chunk)),
            Err(PartialChunk(chunk)) => {
                out.region.chunks.push((marker, chunk));
                out.truncated = true;
                out.trailing = rd.remaining();
                return Ok(out);
            }
        }
    }
    out.trailing = rd.remaining();
    Ok(out)
}

/// A chunk cut short by EOF; carries whatever tiles decoded fully.
struct PartialChunk(TileChunk);

fn read_chunk(
    rd: &mut Rd<'_>,
    framing: Framing,
    ctx: &mut Ctx<'_>,
) -> Result<TileChunk, PartialChunk> {
    let mut tiles: Vec<Option<Tile>> = Vec::with_capacity(16);
    for _ in 0..16 {
        let first = match rd.i32() {
            Ok(v) => v,
            Err(Eof) => {
                while tiles.len() < 16 {
                    tiles.push(None);
                }
                return Err(PartialChunk(TileChunk { tiles }));
            }
        };
        if first == -1 {
            tiles.push(None);
            continue;
        }
        match read_tile(rd, first as u32, framing, ctx) {
            Ok(tile) => tiles.push(Some(tile)),
            Err(Eof) => {
                while tiles.len() < 16 {
                    tiles.push(None);
                }
                return Err(PartialChunk(TileChunk { tiles }));
            }
        }
    }
    Ok(TileChunk { tiles })
}

fn read_tile(
    rd: &mut Rd<'_>,
    first_params: u32,
    framing: Framing,
    ctx: &mut Ctx<'_>,
) -> Result<Tile, Eof> {
    let mut pixels = Vec::with_capacity(256);
    pixels.push(read_pixel(rd, first_params, framing, ctx)?);
    for _ in 1..256 {
        let params = rd.i32()? as u32;
        pixels.push(read_pixel(rd, params, framing, ctx)?);
    }
    let interp_version = if framing.minor >= 4 { rd.u8()? } else { 0 };
    let cave_start = if framing.minor >= 6 { rd.i32()? } else { 0 };
    let cave_depth = if framing.minor >= 7 { rd.u8()? } else { 32 };
    Ok(Tile {
        pixels,
        interp_version,
        cave_start,
        cave_depth,
    })
}

fn read_pixel(
    rd: &mut Rd<'_>,
    params: u32,
    framing: Framing,
    ctx: &mut Ctx<'_>,
) -> Result<Pixel, Eof> {
    let mut px = Pixel {
        params,
        state: None,
        height: 0,
        legacy_height: None,
        top_height: None,
        overlays: SmallVec::new(),
        biome: None,
    };

    // 1. Block state.
    if params & P_NOT_GRASS != 0 {
        if framing.major == 0 {
            let packed = rd.i32()?;
            px.state = Some(ctx.legacy_state(packed));
        } else if params & P_STATE_NEW != 0 {
            let nbt = read_named_nbt(rd)?;
            let name = nbt.name.unwrap_or_default();
            px.state = Some(ctx.palettes.push_state(nbt.raw, name));
        } else {
            px.state = Some(rd.i32()? as u32);
        }
    }

    // 2. Height: an explicit unsigned byte, or the packed signed 12-bit field
    //    whose high nibble moved from bit 24 to bit 25 in minor 4.
    if params & P_LEGACY_HEIGHT_BYTE != 0 {
        let h = rd.u8()?;
        px.legacy_height = Some(h);
        px.height = h as i16;
    } else {
        let raw = ((params >> 12) & 0xFF) | (((params >> framing.height_shift()) & 0xF) << 8);
        px.height = (((raw as i32) << 20) >> 20) as i16;
    }

    // 3. Top height (minor 4 introduced both the flag and the byte).
    if framing.minor >= 4 && params & P_TOP_HEIGHT != 0 {
        px.top_height = Some(rd.u8()?);
    }

    // 4. Overlays.
    if params & P_HAS_OVERLAYS != 0 {
        let n = rd.u8()?;
        px.overlays.reserve(n as usize);
        for _ in 0..n {
            px.overlays.push(read_overlay(rd, framing, ctx)?);
        }
    }

    // 5. Biome. In legacy framing, pixel bits 2-3 are a slope field: value 3
    //    burns a discarded i32, and values 1 and 2 force a biome read even
    //    without the biome flag.
    let slope = if framing.legacy_slope() {
        (params >> 2) & 3
    } else {
        0
    };
    if slope == 3 {
        rd.i32()?;
    }
    if slope == 1 || slope == 2 || params & P_BIOME != 0 {
        if framing.major < 4 {
            // Numeric biome id; minor 3 added a 255 escape to a full i32.
            let v = rd.u8()? as i32;
            let id = if framing.minor < 3 || v < 255 {
                v
            } else {
                rd.i32()?
            };
            px.biome = Some(BiomeRef::Palette(ctx.legacy_biome(id)));
        } else if params & P_BIOME_NEW != 0 {
            // A new palette entry, written either as a numeric id or as text.
            if params & P_BIOME_NUMERIC != 0 {
                let id = rd.i32()?;
                px.biome = Some(BiomeRef::Palette(ctx.legacy_biome(id)));
            } else {
                let len = rd.u16()? as usize;
                let raw = rd.take(len)?.to_vec();
                let name = decode_java_utf(&raw);
                px.biome = Some(BiomeRef::Palette(ctx.palettes.push_biome(raw, name)));
            }
        } else {
            px.biome = Some(BiomeRef::Palette(rd.i32()? as u32));
        }
    }

    // 6. Vertical slope byte (minor 2 only).
    if framing.minor == 2 && params & P_VERTICAL_SLOPE != 0 {
        rd.u8()?;
    }

    Ok(px)
}

fn read_overlay(rd: &mut Rd<'_>, framing: Framing, ctx: &mut Ctx<'_>) -> Result<Overlay, Eof> {
    let params = rd.i32()? as u32;
    let mut ov = Overlay {
        params,
        state: None,
        legacy_opacity: None,
    };
    if params & O_NOT_WATER != 0 {
        if framing.major == 0 {
            let packed = rd.i32()?;
            ov.state = Some(ctx.legacy_state(packed));
        } else if params & O_STATE_NEW != 0 {
            let nbt = read_named_nbt(rd)?;
            let name = nbt.name.unwrap_or_default();
            ov.state = Some(ctx.palettes.push_state(nbt.raw, name));
        } else {
            ov.state = Some(rd.i32()? as u32);
        }
    }
    // Pre-versioned saves had an extra i32 behind bit 1.
    if framing.minor < 1 && params & (1 << 1) != 0 {
        rd.i32()?;
    }
    // Overlay bits 8-9 are the slope field; value 2 burns an i32, and so does
    // bit 2. Neither is minor-gated in the game's loader.
    let ov_slope = if framing.legacy_slope() {
        (params >> 8) & 3
    } else {
        0
    };
    if ov_slope == 2 || params & (1 << 2) != 0 {
        rd.i32()?;
    }
    // Opacity moved from a trailing i32 into params bits 11-14 in minor 8.
    if framing.minor < 8 && params & O_LEGACY_OPACITY != 0 {
        ov.legacy_opacity = Some(rd.i32()?);
    }
    Ok(ov)
}
