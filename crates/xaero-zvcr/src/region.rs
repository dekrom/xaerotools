//! Assembles one zvcr region into one Xaero region.
//!
//! The two formats agree on geometry: a zvcr region and an Xaero region both
//! cover 512x512 blocks, and a zvcr segment is one Minecraft chunk, which is
//! one Xaero map tile. So the mapping is 1:1 with no resampling —
//! segment `(sx, sz)` becomes tile `(sx & 3, sz & 3)` of tile chunk
//! `(sx >> 2, sz >> 2)`.
//!
//! Pixels come out of the column algorithm carrying *global* blockstate ids and
//! zvcr biome ids. This module interns them into the per-region palettes the
//! format wants; the encoder then renumbers to first-appearance order on write.

use std::collections::HashMap;

use xaero_core::codec::FormatVersion;
use xaero_core::model::*;

use crate::biomes;
use crate::blockprops::BlockProps;
use crate::column::{self, ColumnOpts, ColumnWriter};
use crate::zvcr::{Dim, Reader, SegmentView, ZvcrError};

/// `mapTile.setWorldInterpretationVersion(1)` — what the current mod writes.
const INTERPRETATION_VERSION: u8 = 1;
/// Surface tiles record "no cave slice"; the renderer keys off this to skip
/// cave darkening entirely.
const CAVE_START_SURFACE: i32 = i32::MAX;
/// Xaero's own default "Cave mode depth". Inert on a surface tile, but a tile
/// has to record something, and matching the mod's default keeps merged output
/// homogeneous.
const CAVE_DEPTH_DEFAULT: u8 = 30;

pub struct Converted {
    pub region: DecodedRegion,
    /// Tiles that held at least one mappable block. A region where this is zero
    /// is pure void and is not worth writing.
    pub non_empty_tiles: usize,
    pub tiles: usize,
    /// When the download last observed this region (Unix seconds, 0 if the
    /// container recorded no timestamp).
    pub newest_timestamp: u64,
}

/// Runs the column algorithm over every segment of one decompressed zvcr
/// container and returns the Xaero region it produces.
pub fn convert(
    container: &[u8],
    dim: Dim,
    props: &BlockProps,
    opts: ColumnOpts,
) -> Result<Converted, ZvcrError> {
    let mut reader = Reader::new(container, dim)?;
    let mut slots: Vec<Option<Tile>> = (0..crate::zvcr::SEGMENTS_PER_REGION)
        .map(|_| None)
        .collect();
    let mut writer = ColumnWriter::new(props, opts);
    let air = props.air();
    let mut non_empty_tiles = 0usize;

    reader.for_each_segment(|slot, seg| {
        let (tile, mappable) = convert_segment(seg, props, &mut writer, opts, air);
        if mappable {
            non_empty_tiles += 1;
        }
        slots[slot] = Some(tile);
    })?;

    let remaining = reader.remaining();
    if remaining != 0 {
        return Err(ZvcrError::Malformed(format!(
            "{remaining} bytes left after the last segment"
        )));
    }

    let tiles = slots.iter().filter(|s| s.is_some()).count();
    let newest_timestamp = reader.newest_timestamp();
    Ok(Converted {
        region: assemble(slots, props),
        non_empty_tiles,
        tiles,
        newest_timestamp,
    })
}

fn convert_segment(
    seg: &SegmentView<'_>,
    props: &BlockProps,
    writer: &mut ColumnWriter<'_>,
    opts: ColumnOpts,
    air: u32,
) -> (Tile, bool) {
    let min_y = opts.world_bottom_y;
    // `writeChunk` computes this once per chunk and reuses it for every column
    // whose heightmap says the column is empty.
    let section_height = column::section_based_height(seg, min_y, props, air);
    let mut pixels = Vec::with_capacity(256);
    let mut mappable = false;

    // Serialization order is outer x, inner z.
    for x in 0..16 {
        for z in 0..16 {
            let mapped = column::world_surface_height(seg, props, x, z, min_y);
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

/// Interns global ids into region-local palettes and lays the tiles out in the
/// chunk/tile order the container expects.
fn assemble(slots: Vec<Option<Tile>>, props: &BlockProps) -> DecodedRegion {
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
    // Biomes live in their own palette, so they are interned in a second pass
    // rather than fighting the state closure for the borrow.
    for tile in slots.iter_mut().flatten() {
        for px in &mut tile.pixels {
            if let Some(BiomeRef::Palette(zvcr_id)) = px.biome {
                let idx = *biome_map.entry(zvcr_id).or_insert_with(|| {
                    let name = biomes::name(zvcr_id as u16);
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
            // A tile chunk with nothing in it is simply absent from the file.
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

/// Column settings for a dimension: the Nether gets XaeroPlus's roof removal,
/// everything else the plain surface walk.
pub fn opts_for(dim: Dim, nether_roof_removal: bool) -> ColumnOpts {
    if dim == Dim::Nether && nether_roof_removal {
        ColumnOpts::nether_roof_removal(dim)
    } else {
        ColumnOpts::surface(dim)
    }
}

/// Where a region file lands under an Xaero world folder.
pub fn region_file_name(rx: i32, rz: i32) -> String {
    format!("{rx}_{rz}.zip")
}
