//! Assembles one Anvil region file into one Xaero region.
//!
//! The geometry lines up the same way the zvcr importer's does: an `.mca` and
//! an Xaero region both cover 512x512 blocks, and one Minecraft chunk is one
//! Xaero map tile, so the walk is 1:1 with no resampling. Everything that
//! decides what a block column *looks like* is `xaero_zvcr::column` — the port
//! of `MapWriter.loadPixel` — reused verbatim, so a downloaded overworld is
//! written by exactly the same rules as a downloaded Nether.

use xaero_core::codec::FormatVersion;
use xaero_core::model::*;
use xaero_zvcr::blockprops::BlockProps;
use xaero_zvcr::column::{ColumnOpts, ColumnWriter};
use xaero_zvcr::zvcr::{Dim, SegmentView};

use crate::chunk::{
    self, BLOCKS_PER_SECTION, BiomeIntern, ChunkBuffers, ChunkScratch, ChunkStats, Layout,
};
use crate::mca::{CHUNKS_PER_REGION, Inflater, McaError, RegionFile};
use crate::states::StateIndex;

/// `mapTile.setWorldInterpretationVersion(1)` — what the current mod writes.
const INTERPRETATION_VERSION: u8 = 1;
/// Surface tiles record "no cave slice"; the renderer keys off this to skip
/// cave darkening entirely.
const CAVE_START_SURFACE: i32 = i32::MAX;
/// Xaero's own default "Cave mode depth"; inert on a surface tile, but a tile
/// has to record something.
const CAVE_DEPTH_DEFAULT: u8 = 30;
/// The Y `writeChunk` starts `getSectionBasedHeight`'s search from.
const SECTION_SEARCH_START_Y: i32 = 64;

#[derive(Debug)]
pub enum AnvilError {
    Mca(McaError),
}

impl From<McaError> for AnvilError {
    fn from(e: McaError) -> AnvilError {
        AnvilError::Mca(e)
    }
}

impl std::fmt::Display for AnvilError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnvilError::Mca(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AnvilError {}

#[derive(Debug, Default, Clone)]
pub struct RegionStats {
    pub chunks_present: usize,
    pub chunks_mapped: usize,
    /// Chunks that would not read. One bad chunk leaves that tile absent; it
    /// never fails the region.
    pub chunks_failed: usize,
    pub chunk: ChunkStats,
    /// First failure seen, so a run that reports failures can say why.
    pub first_error: Option<String>,
}

impl RegionStats {
    pub fn merge(&mut self, o: &RegionStats) {
        self.chunks_present += o.chunks_present;
        self.chunks_mapped += o.chunks_mapped;
        self.chunks_failed += o.chunks_failed;
        self.chunk.merge(&o.chunk);
        if self.first_error.is_none() {
            self.first_error = o.first_error.clone();
        }
    }
}

pub struct Converted {
    pub region: DecodedRegion,
    /// Tiles that held at least one mappable block. A region where this is
    /// zero is not worth writing.
    pub non_empty_tiles: usize,
    pub tiles: usize,
    /// The newest time the save recorded for any chunk in this region (Unix
    /// seconds, 0 when the file carried none).
    pub newest_timestamp: u64,
    pub stats: RegionStats,
}

/// Buffers one worker reuses for every region it converts.
pub struct Scratch {
    inflate: Inflater,
    nbt: Vec<u8>,
    bufs: ChunkBuffers,
    chunk: ChunkScratch,
}

impl Scratch {
    pub fn new(dim: Dim) -> Scratch {
        Scratch {
            inflate: Inflater::new(),
            nbt: Vec::with_capacity(256 * 1024),
            bufs: ChunkBuffers::new(Layout::for_dim(dim)),
            chunk: ChunkScratch::new(),
        }
    }
    /// Blockstate names this worker met that the baked table does not have
    /// (bounded sample). Empty is the expected result.
    pub fn unknown_states(&self) -> &std::collections::BTreeSet<String> {
        &self.chunk.unknown_states
    }
}

/// Converts one `.mca` file's bytes into the Xaero region they describe.
pub fn convert(
    mca: &[u8],
    dim: Dim,
    props: &BlockProps,
    states: &StateIndex,
    opts: ColumnOpts,
    scratch: &mut Scratch,
) -> Result<Converted, AnvilError> {
    let layout = Layout::for_dim(dim);
    let file = RegionFile::new(mca)?;
    let mut biomes = BiomeIntern::new();
    // One writer for the whole region: `prev_material` is state the mod
    // deliberately carries between pixels, and resetting it per chunk would
    // split overlays the game would have merged.
    let mut writer = ColumnWriter::new(props, opts);
    let mut slots: Vec<Option<Tile>> = (0..CHUNKS_PER_REGION).map(|_| None).collect();
    let mut stats = RegionStats::default();
    let mut newest: u32 = 0;
    let mut non_empty_tiles = 0usize;
    let air = props.air();

    for index in 0..CHUNKS_PER_REGION {
        let (scheme, payload) = match file.raw(index) {
            Ok(Some(raw)) => raw,
            Ok(None) => continue,
            Err(e) => {
                note(&mut stats, index, &e);
                continue;
            }
        };
        stats.chunks_present += 1;
        if let Err(e) = scratch.inflate.chunk(scheme, payload, &mut scratch.nbt) {
            note(&mut stats, index, &e);
            continue;
        }
        match chunk::parse(
            &scratch.nbt,
            layout,
            props,
            states,
            &mut scratch.bufs,
            &mut biomes,
            &mut scratch.chunk,
            &mut stats.chunk,
        ) {
            Ok(chunk::Outcome::Mapped) => {}
            Ok(chunk::Outcome::Unfinished) => continue,
            Err(e) => {
                note(&mut stats, index, &e);
                continue;
            }
        }

        let ts = file.timestamp(index);
        if ts > newest {
            newest = ts;
        }
        let seg = SegmentView {
            blocks: &scratch.bufs.blocks,
            biomes: &scratch.bufs.biomes,
            sections: layout.sections,
        };
        let (tile, mappable) =
            convert_chunk(&seg, &scratch.bufs.nonair, props, &mut writer, opts, air);
        if mappable {
            non_empty_tiles += 1;
        }
        stats.chunks_mapped += 1;
        // Anvil indexes chunks z-major; the tile layout below is x-major.
        let (cx, cz) = (index & 31, index >> 5);
        slots[cx * 32 + cz] = Some(tile);
    }

    let tiles = slots.iter().filter(|s| s.is_some()).count();
    Ok(Converted {
        region: assemble(slots, props, &biomes),
        non_empty_tiles,
        tiles,
        newest_timestamp: newest as u64,
        stats,
    })
}

fn note(stats: &mut RegionStats, index: usize, e: &dyn std::fmt::Display) {
    stats.chunks_failed += 1;
    if stats.first_error.is_none() {
        stats.first_error = Some(format!("chunk {index}: {e}"));
    }
}

/// `MapWriter.writeChunk` for one chunk: one pixel per block column, in the
/// serialization order the tile expects (outer x, inner z).
fn convert_chunk(
    seg: &SegmentView<'_>,
    nonair: &[bool],
    props: &BlockProps,
    writer: &mut ColumnWriter<'_>,
    opts: ColumnOpts,
    air: u32,
) -> (Tile, bool) {
    let min_y = opts.world_bottom_y;
    let section_height = section_based_height(nonair, min_y);
    let mut pixels = Vec::with_capacity(256);
    let mut mappable = false;

    for x in 0..16 {
        for z in 0..16 {
            let mapped = world_surface_height(seg, nonair, props, x, z, min_y);
            let mut start = if mapped < min_y {
                section_height
            } else {
                mapped
            };
            if start >= opts.world_top_y {
                start = opts.world_top_y - 1;
            }
            let px = writer.pixel(seg, x, z, start);
            if px.state != Some(air) || !px.overlays.is_empty() {
                mappable = true;
            }
            pixels.push(px);
        }
    }

    (
        Tile {
            pixels,
            interp_version: INTERPRETATION_VERSION,
            cave_start: CAVE_START_SURFACE,
            cave_depth: CAVE_DEPTH_DEFAULT,
        },
        mappable,
    )
}

/// `MapWriter.getSectionBasedHeight`, off the per-section "holds anything but
/// air" flags rather than a fresh scan of every block.
fn section_based_height(nonair: &[bool], min_y: i32) -> i32 {
    let sections = nonair.len();
    if sections == 0 {
        return 0;
    }
    let start = (((SECTION_SEARCH_START_Y - min_y) >> 4) as usize).min(sections - 1);
    let mut result = 0;
    for (s, _) in nonair.iter().enumerate().skip(start).filter(|(_, n)| **n) {
        result = min_y + (s as i32) * 16 + 15;
    }
    if start > 0
        && result == 0
        && let Some(s) = nonair[..start].iter().rposition(|&n| n)
    {
        result = min_y + (s as i32) * 16 + 15;
    }
    result
}

/// `chunk.getHeight(Heightmap.Types.WORLD_SURFACE, x, z)`: the Y of the topmost
/// non-air block, or `min_y - 1` for an empty column.
///
/// Same answer as walking every level from the top, without walking the empty
/// sky above the terrain — which on a 384-block overworld is most of it.
fn world_surface_height(
    seg: &SegmentView<'_>,
    nonair: &[bool],
    props: &BlockProps,
    x: usize,
    z: usize,
    min_y: i32,
) -> i32 {
    for s in (0..nonair.len()).rev() {
        if !nonair[s] {
            continue;
        }
        let base = s * BLOCKS_PER_SECTION;
        for yy in (0..16usize).rev() {
            if !props.is_air(seg.blocks[base + yy * 256 + z * 16 + x] as u32) {
                return min_y + (s as i32) * 16 + yy as i32;
            }
        }
    }
    min_y - 1
}

/// Interns global ids into region-local palettes and lays the tiles out in the
/// chunk/tile order the container expects.
fn assemble(slots: Vec<Option<Tile>>, props: &BlockProps, biomes: &BiomeIntern) -> DecodedRegion {
    use std::collections::HashMap;

    let mut palettes = Palettes::default();
    let mut state_map: HashMap<u32, u32> = HashMap::new();
    let mut biome_map: HashMap<u32, u32> = HashMap::new();

    let mut intern_state = |global: u32, palettes: &mut Palettes| -> u32 {
        *state_map.entry(global).or_insert_with(|| {
            palettes.push_state(
                props.nbt(global).to_vec(),
                props.block_name(global).to_string(),
            )
        })
    };

    let mut slots = slots;
    for tile in slots.iter_mut().flatten() {
        for px in &mut tile.pixels {
            if let Some(g) = px.state {
                px.state = Some(intern_state(g, &mut palettes));
            }
            for ov in &mut px.overlays {
                if let Some(g) = ov.state {
                    ov.state = Some(intern_state(g, &mut palettes));
                }
            }
        }
    }
    for tile in slots.iter_mut().flatten() {
        for px in &mut tile.pixels {
            if let Some(BiomeRef::Palette(local)) = px.biome {
                let idx = *biome_map.entry(local).or_insert_with(|| {
                    let name = biomes.name(local as u16);
                    palettes.push_biome(name.as_bytes().to_vec(), name.to_string())
                });
                px.biome = Some(BiomeRef::Palette(idx));
            }
        }
    }

    let mut chunks = Vec::with_capacity(64);
    for cx in 0..8u8 {
        for cz in 0..8u8 {
            let mut tiles = Vec::with_capacity(16);
            let mut any = false;
            for tx in 0..4usize {
                for tz in 0..4usize {
                    let sx = cx as usize * 4 + tx;
                    let sz = cz as usize * 4 + tz;
                    let tile = slots[sx * 32 + sz].take();
                    any |= tile.is_some();
                    tiles.push(tile);
                }
            }
            if any {
                chunks.push(((cx << 4) | cz, TileChunk { tiles }));
            }
        }
    }

    DecodedRegion {
        version: FormatVersion {
            major: xaero_core::WRITE_MAJOR,
            minor: xaero_core::WRITE_MINOR,
        },
        region: Region { chunks },
        palettes,
        truncated: false,
        trailing: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg_of<'a>(blocks: &'a [u16], biomes: &'a [u16], sections: usize) -> SegmentView<'a> {
        SegmentView {
            blocks,
            biomes,
            sections,
        }
    }

    /// The optimized height helpers must answer exactly what the zvcr ones do;
    /// they are the same algorithm with the empty sections skipped.
    #[test]
    fn heights_agree_with_the_zvcr_originals() {
        let props = BlockProps::parse(xaero_zvcr::BLOCKPROPS).unwrap();
        let air = props.air() as u16;
        let stone = (0..props.len() as u32)
            .find(|&i| props.name(i) == "minecraft:stone")
            .unwrap() as u16;
        let sections = 24usize;
        let min_y = -64;

        for &top in &[0usize, 1, 70, 200, 383] {
            let mut blocks = vec![air; sections * BLOCKS_PER_SECTION];
            // Fill every column from the bottom up to `top`.
            for y in 0..=top {
                for z in 0..16 {
                    for x in 0..16 {
                        blocks[(y >> 4) * BLOCKS_PER_SECTION + (y & 15) * 256 + z * 16 + x] = stone;
                    }
                }
            }
            let biomes = vec![0u16; sections * 64];
            let seg = seg_of(&blocks, &biomes, sections);
            let nonair: Vec<bool> = (0..sections)
                .map(|s| {
                    blocks[s * BLOCKS_PER_SECTION..(s + 1) * BLOCKS_PER_SECTION]
                        .iter()
                        .any(|&b| !props.is_air(b as u32))
                })
                .collect();

            assert_eq!(
                section_based_height(&nonair, min_y),
                xaero_zvcr::column::section_based_height(&seg, min_y, &props, props.air()),
                "section height, top={top}"
            );
            for (x, z) in [(0usize, 0usize), (7, 9), (15, 15)] {
                assert_eq!(
                    world_surface_height(&seg, &nonair, &props, x, z, min_y),
                    xaero_zvcr::column::world_surface_height(&seg, &props, x, z, min_y),
                    "surface height, top={top} at {x},{z}"
                );
            }
        }
    }

    #[test]
    fn an_all_air_column_reads_as_the_void() {
        let props = BlockProps::parse(xaero_zvcr::BLOCKPROPS).unwrap();
        let sections = 24usize;
        let blocks = vec![props.air() as u16; sections * BLOCKS_PER_SECTION];
        let biomes = vec![0u16; sections * 64];
        let seg = seg_of(&blocks, &biomes, sections);
        let nonair = vec![false; sections];
        assert_eq!(section_based_height(&nonair, -64), 0);
        assert_eq!(world_surface_height(&seg, &nonair, &props, 3, 4, -64), -65);
    }
}
