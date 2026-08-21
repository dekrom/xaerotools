//! Semantic model of a decoded region.
//!
//! Design principle: **passthrough-plus-fields**. Every pixel keeps its raw
//! `params` word exactly as read; typed accessors decode the interesting bits.
//! The encoder re-emits the passthrough bits verbatim and recomputes only the
//! palette-dependent flags (pixel bits 21/22, overlay bit 10), which makes
//! byte-identical re-encoding of major-7 input possible — our strongest
//! regression oracle.
//!
//! Palette entries are stored as the **raw bytes read from the file** (NBT for
//! block states, Java-UTF payload for biomes) plus a decoded name for
//! rendering. Raw bytes are re-emitted verbatim on encode, so we never need a
//! lossless NBT round-trip through a parsed tree.

use smallvec::SmallVec;

/// Region = 8x8 tile chunks; chunk = 4x4 tiles; tile = 16x16 pixels
/// (1 pixel = 1 block column; region = 512x512 blocks).
pub const REGION_CHUNKS: usize = 8;
pub const CHUNK_TILES: usize = 4;
pub const TILE_PIXELS: usize = 16;

// Pixel params bits (see plan digest / MapBlock.getParametres + savePixel).
pub const P_NOT_GRASS: u32 = 1 << 0;
pub const P_HAS_OVERLAYS: u32 = 1 << 1;
/// Legacy framing only (minor < 5): bits 2-3 are a slope field. Value 3 burns a
/// discarded i32; values 1 and 2 force a biome read even without `P_BIOME`.
pub const P_LEGACY_SLOPE: u32 = 3 << 2;
/// Legacy framing only (minor == 2): a trailing vertical-slope byte follows.
pub const P_VERTICAL_SLOPE: u32 = 1 << 4;
pub const P_LEGACY_HEIGHT_BYTE: u32 = 1 << 6;
pub const P_BIOME: u32 = 1 << 20;
pub const P_STATE_NEW: u32 = 1 << 21; // blockstate not yet in palette -> NBT follows
pub const P_BIOME_NEW: u32 = 1 << 22; // biome not yet in palette -> UTF follows
pub const P_BIOME_NUMERIC: u32 = 1 << 23; // with bit 22: the new biome entry is a numeric id
pub const P_TOP_HEIGHT: u32 = 1 << 24; // separate u8 topHeight follows

// Overlay params bits (Overlay.getParametres + saveOverlay).
pub const O_NOT_WATER: u32 = 1 << 0;
pub const O_LEGACY_OPACITY: u32 = 1 << 3; // minor < 8: opacity as i32 follows
pub const O_STATE_NEW: u32 = 1 << 10; // overlay state not yet in palette -> NBT follows

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pixel {
    /// Raw params word as read (bit 21/22 included as they appeared on disk).
    pub params: u32,
    /// Block state palette index; `None` = implicit `minecraft:grass_block`.
    ///
    /// Legacy (major 0) regions store a numeric 1.12 id + meta instead of an
    /// NBT state; the decoder resolves those through the baked id table and
    /// interns them here, so every consumer sees one uniform representation.
    pub state: Option<u32>,
    /// Decoded surface height. Held as a field rather than derived from
    /// `params` because the high nibble sits at bit 25 from minor 4 onward and
    /// at bit 24 before it, and because bit 6 replaces the packed field with an
    /// explicit unsigned byte.
    pub height: i16,
    /// Legacy raw height byte (params bit 6); never written by modern writers.
    pub legacy_height: Option<u8>,
    /// Separate top height byte (params bit 24). The shipped writer truncates
    /// this to u8; we deliberately model it the same way.
    pub top_height: Option<u8>,
    pub overlays: SmallVec<[Overlay; 1]>,
    pub biome: Option<BiomeRef>,
}

impl Pixel {
    pub fn is_grass(&self) -> bool {
        self.params & P_NOT_GRASS == 0
    }
    pub fn light(&self) -> u8 {
        ((self.params >> 8) & 15) as u8
    }
    pub fn is_cave_block(&self) -> bool {
        self.params & (1 << 7) != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiomeRef {
    /// Index into `Palettes::biomes`.
    Palette(u32),
    /// Legacy numeric biome id (params bit 23); passed through untouched.
    LegacyNumeric(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    /// Raw overlay params word as read.
    pub params: u32,
    /// State palette index (shared with block states); `None` = water.
    pub state: Option<u32>,
    /// Legacy opacity i32 (minor < 8 files only); preserved for fidelity.
    pub legacy_opacity: Option<i32>,
}

impl Overlay {
    pub fn light(&self) -> u8 {
        ((self.params >> 4) & 15) as u8
    }
    /// 4-bit opacity (minor >= 8 encoding).
    pub fn opacity(&self) -> u8 {
        ((self.params >> 11) & 15) as u8
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tile {
    /// 256 pixels, serialization order: outer x 0..16, inner z 0..16
    /// (index = x * 16 + z).
    pub pixels: Vec<Pixel>,
    pub interp_version: u8, // trailer, minor >= 4
    pub cave_start: i32,    // trailer, minor >= 6
    pub cave_depth: u8,     // trailer, minor >= 7 (default 32 for older)
}

impl Tile {
    #[inline]
    pub fn pixel(&self, x: usize, z: usize) -> &Pixel {
        &self.pixels[x * TILE_PIXELS + z]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileChunk {
    /// 16 tile slots in serialization order: outer tx 0..4, inner tz 0..4
    /// (index = tx * 4 + tz). `None` = absent tile (i32 -1 on disk).
    pub tiles: Vec<Option<Tile>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Region {
    /// Chunks in **file order** with their marker byte `(cx << 4) | cz`.
    /// Kept as a list (not an 8x8 array) so duplicate or unordered markers in
    /// a file survive re-encoding byte-identically.
    pub chunks: Vec<(u8, TileChunk)>,
}

impl Region {
    /// Last-wins lookup by chunk coordinates (matches game overwrite order).
    pub fn chunk(&self, cx: u8, cz: u8) -> Option<&TileChunk> {
        let marker = (cx << 4) | cz;
        self.chunks
            .iter()
            .rev()
            .find(|(m, _)| *m == marker)
            .map(|(_, c)| c)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Palettes {
    /// Raw NBT bytes (NbtIo framing) per block/overlay state, file order.
    pub states: Vec<Vec<u8>>,
    /// Extracted `Name` of each state (e.g. "minecraft:netherrack"), for rendering.
    pub state_names: Vec<String>,
    /// Raw Java-UTF payload bytes per biome (without the u16 length prefix).
    pub biomes: Vec<Vec<u8>>,
    /// Decoded biome ids (e.g. "minecraft:plains").
    pub biome_names: Vec<String>,
}

impl Palettes {
    pub fn push_state(&mut self, raw: Vec<u8>, name: String) -> u32 {
        self.states.push(raw);
        self.state_names.push(name);
        (self.states.len() - 1) as u32
    }
    pub fn push_biome(&mut self, raw: Vec<u8>, name: String) -> u32 {
        self.biomes.push(raw);
        self.biome_names.push(name);
        (self.biomes.len() - 1) as u32
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedRegion {
    pub version: crate::codec::FormatVersion,
    pub region: Region,
    pub palettes: Palettes,
    /// True when the stream ended mid-structure (partial data returned).
    pub truncated: bool,
    /// Bytes left unconsumed after the decoder stopped (0 on a clean parse).
    pub trailing: usize,
}
