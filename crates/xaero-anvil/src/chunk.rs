//! Unpacks one saved chunk into the flat block/biome arrays the column
//! algorithm reads.
//!
//! The target layout is deliberately the one `xaero_zvcr::zvcr::SegmentView`
//! describes — `sections * 4096` blockstate ids indexed `y * 256 + z * 16 + x`
//! within a section, `sections * 64` biome ids indexed `y * 16 + z * 4 + x` —
//! because that is what `xaero_zvcr::column` walks. Anvil stores exactly the
//! same orders, so a section drops straight in.
//!
//! Blocks come out as ids into the baked block table (global, cross-referenced
//! by name). Biomes come out as ids into a per-region name table: Anvil names
//! its biomes, Xaero stores names too, and nothing in between needs a numeric
//! registry the download does not ship.

use std::collections::{BTreeSet, HashMap};

use smallvec::SmallVec;
use xaero_zvcr::blockprops::BlockProps;

use crate::nbt::{self, Cursor, NbtError};
use crate::states::{Props, StateIndex, canonical};

pub const BLOCKS_PER_SECTION: usize = 16 * 16 * 16;
pub const BIOMES_PER_SECTION: usize = 4 * 4 * 4;
/// `Strategy.SECTION_STATES`: above this many distinct states in one section
/// the game stops writing palette indices and writes global registry ids.
const MAX_STATE_PALETTE_BITS: u32 = 8;
/// `Strategy.SECTION_BIOMES`: same switch, three bits up.
const MAX_BIOME_PALETTE_BITS: u32 = 3;

#[derive(Debug)]
pub enum ChunkError {
    Nbt(NbtError),
    Malformed(String),
}

impl From<NbtError> for ChunkError {
    fn from(e: NbtError) -> ChunkError {
        ChunkError::Nbt(e)
    }
}

impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkError::Nbt(e) => write!(f, "{e}"),
            ChunkError::Malformed(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ChunkError {}

/// Counts of things that were readable but not fully representable. None of
/// these fail a region — they are reported so a run that hits them is not
/// silently believed.
#[derive(Debug, Default, Clone, Copy)]
pub struct ChunkStats {
    /// Sections holding more than 256 distinct states, which the game writes
    /// as *its own* registry ids rather than palette indices. Those ids belong
    /// to the download's Minecraft version, not the baked table's, so the
    /// section is left as air rather than filled with the wrong blocks.
    pub sections_state_global: usize,
    /// Same switch for biomes, above eight distinct biomes in one section.
    pub sections_biome_global: usize,
    /// Packed indices pointing outside their palette (corrupt data).
    pub bad_palette_indices: usize,
    /// Palette entries naming a state the baked table does not have.
    pub unknown_states: usize,
    /// Chunks whose `Status` says the world generator never finished them.
    pub chunks_unfinished: usize,
}

impl ChunkStats {
    pub fn merge(&mut self, o: &ChunkStats) {
        self.sections_state_global += o.sections_state_global;
        self.sections_biome_global += o.sections_biome_global;
        self.bad_palette_indices += o.bad_palette_indices;
        self.unknown_states += o.unknown_states;
        self.chunks_unfinished += o.chunks_unfinished;
    }
}

/// Where a dimension's sections sit in Minecraft coordinates.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub sections: usize,
    pub min_y: i32,
}

impl Layout {
    pub fn for_dim(dim: xaero_zvcr::zvcr::Dim) -> Layout {
        Layout {
            sections: dim.sections(),
            min_y: dim.min_y(),
        }
    }
    #[inline]
    fn min_section_y(&self) -> i32 {
        self.min_y >> 4
    }
}

/// The flat arrays one chunk is unpacked into, reused for every chunk.
pub struct ChunkBuffers {
    pub blocks: Vec<u16>,
    pub biomes: Vec<u16>,
    /// Per section: whether its palette holds anything but air. Over-reporting
    /// is harmless (it only costs a scan); under-reporting would hide terrain,
    /// so this is taken from the palette, never from a sampled subset.
    pub nonair: Vec<bool>,
}

impl ChunkBuffers {
    pub fn new(layout: Layout) -> ChunkBuffers {
        ChunkBuffers {
            blocks: vec![0; layout.sections * BLOCKS_PER_SECTION],
            biomes: vec![0; layout.sections * BIOMES_PER_SECTION],
            nonair: vec![false; layout.sections],
        }
    }
}

/// Per-region biome name table. Xaero's palette is a list of names, so the
/// numbers here never leave this crate; they exist only because the column
/// algorithm passes biomes around as `u16`.
pub struct BiomeIntern {
    names: Vec<String>,
    index: HashMap<String, u16>,
}

impl Default for BiomeIntern {
    fn default() -> Self {
        Self::new()
    }
}

impl BiomeIntern {
    pub fn new() -> BiomeIntern {
        let mut b = BiomeIntern {
            names: Vec::new(),
            index: HashMap::new(),
        };
        // Id 0 is what an unwritten biome slot reads as, and the void is what
        // an unwritten column is.
        b.intern("minecraft:the_void");
        b
    }

    pub fn intern(&mut self, name: &str) -> u16 {
        if let Some(&id) = self.index.get(name) {
            return id;
        }
        // A region cannot plausibly hold 65 536 biomes; if one claims to, the
        // extras collapse onto the void rather than wrapping onto each other.
        if self.names.len() >= u16::MAX as usize {
            return 0;
        }
        let id = self.names.len() as u16;
        self.names.push(name.to_string());
        self.index.insert(name.to_string(), id);
        id
    }

    pub fn name(&self, id: u16) -> &str {
        self.names
            .get(id as usize)
            .map(|s| s.as_str())
            .unwrap_or("minecraft:the_void")
    }
}

/// Scratch a single worker reuses across every chunk it converts.
pub struct ChunkScratch {
    pub key: String,
    pub unknown_states: BTreeSet<String>,
}

impl Default for ChunkScratch {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkScratch {
    pub fn new() -> ChunkScratch {
        ChunkScratch {
            key: String::with_capacity(128),
            unknown_states: BTreeSet::new(),
        }
    }
}

pub enum Outcome {
    /// The chunk holds world data and the buffers now describe it.
    Mapped,
    /// The generator never finished this chunk; mapping it would paint
    /// half-built terrain over ground nobody has seen.
    Unfinished,
}

/// Fills `bufs` from one chunk's NBT.
#[allow(clippy::too_many_arguments)]
pub fn parse(
    nbt_bytes: &[u8],
    layout: Layout,
    props: &BlockProps,
    states: &StateIndex,
    bufs: &mut ChunkBuffers,
    biomes: &mut BiomeIntern,
    scratch: &mut ChunkScratch,
    stats: &mut ChunkStats,
) -> Result<Outcome, ChunkError> {
    let air = states.air();
    bufs.blocks.fill(air);
    bufs.biomes.fill(0);
    bufs.nonair.fill(false);

    let mut c = Cursor::new(nbt_bytes);
    c.open_root()?;
    // A chunk with no `Status` at all predates the field; the download's own
    // chunks all carry it, and everything that does must say `full`.
    let mut finished = true;
    loop {
        let tag = c.u8()?;
        if tag == nbt::TAG_END {
            break;
        }
        let key = c.raw_str()?;
        match key {
            b"Status" if tag == nbt::TAG_STRING => {
                let s = c.raw_str()?;
                finished = s == b"minecraft:full" || s == b"full";
            }
            b"sections" if tag == nbt::TAG_LIST => {
                sections(&mut c, layout, props, states, bufs, biomes, scratch, stats)?;
            }
            _ => c.skip(tag)?,
        }
    }
    if !finished {
        stats.chunks_unfinished += 1;
        return Ok(Outcome::Unfinished);
    }
    Ok(Outcome::Mapped)
}

#[allow(clippy::too_many_arguments)]
fn sections<'a>(
    c: &mut Cursor<'a>,
    layout: Layout,
    props: &BlockProps,
    states: &StateIndex,
    bufs: &mut ChunkBuffers,
    biomes: &mut BiomeIntern,
    scratch: &mut ChunkScratch,
    stats: &mut ChunkStats,
) -> Result<(), ChunkError> {
    let elem = c.u8()?;
    let n = c.array_len()?;
    if n > 0 && elem != nbt::TAG_COMPOUND {
        return Err(ChunkError::Malformed(format!(
            "sections is a list of tag {elem}, not compounds"
        )));
    }
    for _ in 0..n {
        section(c, layout, props, states, bufs, biomes, scratch, stats)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn section<'a>(
    c: &mut Cursor<'a>,
    layout: Layout,
    props: &BlockProps,
    states: &StateIndex,
    bufs: &mut ChunkBuffers,
    biomes: &mut BiomeIntern,
    scratch: &mut ChunkScratch,
    stats: &mut ChunkStats,
) -> Result<(), ChunkError> {
    let mut y: Option<i32> = None;
    let mut state_palette: SmallVec<[u16; 64]> = SmallVec::new();
    let mut state_data: Option<&[u8]> = None;
    let mut biome_palette: SmallVec<[u16; 16]> = SmallVec::new();
    let mut biome_data: Option<&[u8]> = None;

    loop {
        let tag = c.u8()?;
        if tag == nbt::TAG_END {
            break;
        }
        let key = c.raw_str()?;
        match key {
            b"Y" if tag == nbt::TAG_BYTE => y = Some(c.i8()? as i32),
            b"Y" if tag == nbt::TAG_INT => y = Some(c.i32()?),
            b"block_states" if tag == nbt::TAG_COMPOUND => {
                paletted(c, &mut state_data, |cur, len| {
                    state_palette.reserve(len);
                    for _ in 0..len {
                        state_palette.push(state_entry(cur, states, scratch, stats)?);
                    }
                    Ok(())
                })?;
            }
            b"biomes" if tag == nbt::TAG_COMPOUND => {
                paletted(c, &mut biome_data, |cur, len| {
                    biome_palette.reserve(len);
                    for _ in 0..len {
                        biome_palette.push(biomes.intern(cur.str()?));
                    }
                    Ok(())
                })?;
            }
            _ => c.skip(tag)?,
        }
    }

    // Sections outside the dimension's build range exist only to carry light
    // for the columns just above and below it; they hold no blocks to map.
    let Some(y) = y else { return Ok(()) };
    let idx = y - layout.min_section_y();
    if idx < 0 || idx as usize >= layout.sections {
        return Ok(());
    }
    let s = idx as usize;

    let base = s * BLOCKS_PER_SECTION;
    let dst = &mut bufs.blocks[base..base + BLOCKS_PER_SECTION];
    match (state_palette.len(), state_data) {
        (0, _) => {}
        (1, None) => {
            dst.fill(state_palette[0]);
            bufs.nonair[s] = !props.is_air(state_palette[0] as u32);
        }
        (len, Some(longs)) => {
            let want = nbt::ceil_log2(len).max(4);
            if want > MAX_STATE_PALETTE_BITS {
                stats.sections_state_global += 1;
            } else {
                let bits = bits_for(longs.len(), BLOCKS_PER_SECTION, want)?;
                expand(longs, bits, dst, &state_palette, states.air(), stats);
                bufs.nonair[s] = state_palette.iter().any(|&v| !props.is_air(v as u32));
            }
        }
        (_, None) => {
            return Err(ChunkError::Malformed(
                "block_states has a multi-entry palette and no data".into(),
            ));
        }
    }

    let base = s * BIOMES_PER_SECTION;
    let dst = &mut bufs.biomes[base..base + BIOMES_PER_SECTION];
    match (biome_palette.len(), biome_data) {
        (0, _) => {}
        (1, None) => dst.fill(biome_palette[0]),
        (len, Some(longs)) => {
            let want = nbt::ceil_log2(len);
            if want > MAX_BIOME_PALETTE_BITS {
                stats.sections_biome_global += 1;
                dst.fill(biome_palette[0]);
            } else {
                let bits = bits_for(longs.len(), BIOMES_PER_SECTION, want.max(1))?;
                expand(longs, bits, dst, &biome_palette, biome_palette[0], stats);
            }
        }
        (_, None) => {
            return Err(ChunkError::Malformed(
                "biomes has a multi-entry palette and no data".into(),
            ));
        }
    }
    Ok(())
}

/// Walks a `{palette, data}` compound, handing the palette list to `read` and
/// capturing the packed data. The two keys arrive in `HashMap` order, so
/// neither may assume the other has been seen.
fn paletted<'a, F>(
    c: &mut Cursor<'a>,
    data: &mut Option<&'a [u8]>,
    mut read: F,
) -> Result<(), ChunkError>
where
    F: FnMut(&mut Cursor<'a>, usize) -> Result<(), ChunkError>,
{
    loop {
        let tag = c.u8()?;
        if tag == nbt::TAG_END {
            break;
        }
        let key = c.raw_str()?;
        match key {
            b"palette" if tag == nbt::TAG_LIST => {
                let elem = c.u8()?;
                let n = c.array_len()?;
                if n > 0 && elem == nbt::TAG_END {
                    return Err(ChunkError::Malformed(
                        "palette list has no element type".into(),
                    ));
                }
                read(c, n)?;
            }
            b"data" if tag == nbt::TAG_LONG_ARRAY => *data = Some(c.long_array()?),
            _ => c.skip(tag)?,
        }
    }
    Ok(())
}

/// One `{Name, Properties}` palette entry, resolved to a baked-table id.
fn state_entry(
    c: &mut Cursor<'_>,
    states: &StateIndex,
    scratch: &mut ChunkScratch,
    stats: &mut ChunkStats,
) -> Result<u16, ChunkError> {
    let mut name: Option<&str> = None;
    let mut pairs: Props = SmallVec::new();
    loop {
        let tag = c.u8()?;
        if tag == nbt::TAG_END {
            break;
        }
        let key = c.raw_str()?;
        match key {
            b"Name" if tag == nbt::TAG_STRING => name = Some(c.str()?),
            b"Properties" if tag == nbt::TAG_COMPOUND => loop {
                let t = c.u8()?;
                if t == nbt::TAG_END {
                    break;
                }
                let k = c.str()?;
                if t == nbt::TAG_STRING {
                    pairs.push((k, c.str()?));
                } else {
                    c.skip(t)?;
                }
            },
            _ => c.skip(tag)?,
        }
    }
    let Some(name) = name else {
        return Err(ChunkError::Malformed("palette entry without a Name".into()));
    };
    canonical(&mut scratch.key, name, &mut pairs);
    match states.get(&scratch.key) {
        Some(id) => Ok(id),
        None => {
            stats.unknown_states += 1;
            // Keep a bounded sample of the names so a run that hits this says
            // *what* it could not resolve, without growing without limit.
            if scratch.unknown_states.len() < 256 {
                scratch.unknown_states.insert(scratch.key.clone());
            }
            Ok(states.air())
        }
    }
}

/// The bit width the packed array must have been written at. Minecraft's rule
/// is `max(floor, ceillog2(palette))`, which is checked first; the scan after
/// it only exists so a file written by some other tool's rounding is read
/// rather than rejected.
fn bits_for(data_bytes: usize, entries: usize, want: u32) -> Result<u32, ChunkError> {
    if !data_bytes.is_multiple_of(8) {
        return Err(ChunkError::Malformed(format!(
            "packed data is {data_bytes} bytes, not a whole number of longs"
        )));
    }
    let longs = data_bytes / 8;
    if nbt::packed_longs(entries, want) == longs {
        return Ok(want);
    }
    for bits in want..=16 {
        if nbt::packed_longs(entries, bits) == longs {
            return Ok(bits);
        }
    }
    Err(ChunkError::Malformed(format!(
        "{longs} longs hold no whole number of {entries} entries at {want} bits or wider"
    )))
}

/// Unpacks `dst.len()` entries out of the long array and maps them through the
/// palette. Walks longs rather than indices so the inner loop has no division.
fn expand(
    longs: &[u8],
    bits: u32,
    dst: &mut [u16],
    palette: &[u16],
    fallback: u16,
    stats: &mut ChunkStats,
) {
    let per_long = 64 / bits as usize;
    let mask = (1u64 << bits) - 1;
    let mut i = 0usize;
    for word in longs.as_chunks::<8>().0 {
        let mut w = u64::from_be_bytes(*word);
        for _ in 0..per_long {
            if i == dst.len() {
                return;
            }
            let p = (w & mask) as usize;
            w >>= bits;
            dst[i] = match palette.get(p) {
                Some(&v) => v,
                None => {
                    stats.bad_palette_indices += 1;
                    fallback
                }
            };
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_widths_match_real_sections() {
        // 33 states -> 6 bits -> 410 longs, straight off a real 1.21.1 chunk.
        assert_eq!(bits_for(410 * 8, BLOCKS_PER_SECTION, 6).unwrap(), 6);
        // Single-biome sections carry no data at all; two biomes are one bit.
        assert_eq!(bits_for(8, BIOMES_PER_SECTION, 1).unwrap(), 1);
        assert!(bits_for(7, BLOCKS_PER_SECTION, 4).is_err());
        assert!(bits_for(8, BLOCKS_PER_SECTION, 4).is_err());
    }

    #[test]
    fn expansion_reads_entries_low_bits_first() {
        // Two 4-bit entries per nibble order: palette indices 1,2,3,0,...
        let mut word = 0u64;
        for (i, v) in [1u64, 2, 3].iter().enumerate() {
            word |= v << (4 * i);
        }
        let longs = word.to_be_bytes();
        let mut dst = [0u16; 16];
        let mut stats = ChunkStats::default();
        expand(&longs, 4, &mut dst, &[10, 11, 12, 13], 99, &mut stats);
        assert_eq!(&dst[..4], &[11, 12, 13, 10]);
        assert_eq!(stats.bad_palette_indices, 0);
    }

    #[test]
    fn an_index_past_the_palette_degrades_and_is_counted() {
        let longs = 5u64.to_be_bytes();
        let mut dst = [0u16; 16];
        let mut stats = ChunkStats::default();
        expand(&longs, 4, &mut dst, &[10, 11], 99, &mut stats);
        assert_eq!(dst[0], 99);
        assert_eq!(stats.bad_palette_indices, 1);
    }

    #[test]
    fn biome_ids_are_stable_per_name() {
        let mut b = BiomeIntern::new();
        assert_eq!(b.name(0), "minecraft:the_void");
        let plains = b.intern("minecraft:plains");
        assert_eq!(b.intern("minecraft:plains"), plains);
        assert_ne!(b.intern("minecraft:desert"), plains);
        assert_eq!(b.name(plains), "minecraft:plains");
    }
}
