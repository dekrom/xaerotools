//! Column-algorithm regression tests over a hand-built zvcr container.
//!
//! `region::convert` takes an already-decompressed container, so these build
//! one directly and skip Zstd — the reader is exercised on the real archive,
//! but the *behaviour* worth pinning is which block each column resolves to.

use xaero_core::model::*;
use xaero_zvcr::blockprops::BlockProps;
use xaero_zvcr::column::ColumnOpts;
use xaero_zvcr::{region, zvcr};

/// Builds a container holding exactly one segment in slot 0, every column of
/// which is the given bottom-up stack of blockstate ids.
fn container_with_column(dim: zvcr::Dim, column: &[u16], biome: u16) -> Vec<u8> {
    let sections = dim.sections();
    let height = sections * 16;
    assert_eq!(
        column.len(),
        height,
        "column must span the whole world height"
    );

    // One palette holding every distinct id in the column, so 4-bit entries
    // cover it as long as the test uses at most 16 blocks.
    let mut palette: Vec<u16> = Vec::new();
    for &id in column {
        if !palette.contains(&id) {
            palette.push(id);
        }
    }
    assert!(palette.len() <= 16, "test palette must fit 4-bit entries");
    let index_of = |id: u16| palette.iter().position(|&p| p == id).unwrap() as u64;

    let mut out = Vec::new();
    // Block palette table: one palette. Biome palette table: none (the biome
    // sections below are single-valued, which needs no table entry).
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(palette.len() as u16).to_le_bytes());
    for id in &palette {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());

    for slot in 0..zvcr::SEGMENTS_PER_REGION {
        if slot != 0 {
            out.push(0);
            continue;
        }
        out.push(1);
        for s in 0..sections {
            // One snapshot, section palette, 4 bits per entry.
            out.extend_from_slice(&1u64.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes()); // timestamp
            out.push(1); // section palette
            let longs = 4096 / 16;
            out.extend_from_slice(&(longs as u64).to_le_bytes());
            for l in 0..longs {
                let mut word = 0u64;
                for k in 0..16 {
                    let i = l * 16 + k;
                    // Index within a section is y * 256 + z * 16 + x, so every
                    // column in the chunk gets the same stack.
                    let y = s * 16 + i / 256;
                    word |= index_of(column[y]) << (k * 4);
                }
                out.extend_from_slice(&word.to_le_bytes());
            }
            out.extend_from_slice(&0u32.to_le_bytes()); // palette index
        }
        for _ in 0..sections {
            out.extend_from_slice(&1u64.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes());
            out.push(0); // single-value palette
            out.extend_from_slice(&biome.to_le_bytes());
        }
        out.extend_from_slice(&0u64.to_le_bytes()); // segment states
        out.extend_from_slice(&0u64.to_le_bytes()); // tile entity history
    }
    out
}

fn props() -> BlockProps {
    BlockProps::parse(xaero_zvcr::BLOCKPROPS).expect("baked block table")
}

fn id_of(props: &BlockProps, name: &str) -> u16 {
    (0..props.len() as u32)
        .find(|&i| props.name(i) == name)
        .unwrap_or_else(|| panic!("no such state: {name}")) as u16
}

/// The tile the single segment produced, and its first pixel.
fn first_pixel(converted: &region::Converted) -> (&Pixel, &Tile) {
    let (_, chunk) = converted
        .region
        .region
        .chunks
        .iter()
        .find(|(marker, _)| *marker == 0)
        .expect("tile chunk 0,0");
    let tile = chunk.tiles[0].as_ref().expect("tile 0,0");
    (&tile.pixels[0], tile)
}

fn state_name<'a>(converted: &'a region::Converted, px: &Pixel) -> &'a str {
    match px.state {
        None => "minecraft:grass_block",
        Some(i) => &converted.region.palettes.state_names[i as usize],
    }
}

/// A Nether column: bedrock roof at 127, an air gap, netherrack floor at 119.
fn nether_column(props: &BlockProps) -> Vec<u16> {
    let air = id_of(props, "minecraft:air");
    let bedrock = id_of(props, "minecraft:bedrock");
    let netherrack = id_of(props, "minecraft:netherrack");
    let mut column = vec![air; 256];
    column[..64].fill(netherrack);
    column[119] = netherrack;
    column[120..127].fill(air);
    column[127] = bedrock;
    column
}

#[test]
fn nether_roof_removal_maps_the_floor_under_the_roof() {
    let props = props();
    let column = nether_column(&props);
    let container = container_with_column(zvcr::Dim::Nether, &column, 34);

    let converted = region::convert(
        &container,
        zvcr::Dim::Nether,
        &props,
        ColumnOpts::nether_roof_removal(zvcr::Dim::Nether),
    )
    .expect("convert");

    let (px, tile) = first_pixel(&converted);
    // The walk enters the bedrock roof, leaves it on the far side, and stops on
    // the first floor under the next air gap.
    assert_eq!(state_name(&converted, px), "minecraft:netherrack");
    assert_eq!(px.height, 119);
    assert!(px.overlays.is_empty());
    // The tile is still a surface tile: no cave slice is recorded, so nothing
    // downstream applies cave darkening to it.
    assert_eq!(tile.cave_start, i32::MAX);
    assert_eq!(tile.interp_version, 1);
    assert_eq!(
        converted.region.palettes.biome_names[0],
        "minecraft:nether_wastes"
    );
}

#[test]
fn surface_walk_stops_at_the_roof_instead() {
    let props = props();
    let column = nether_column(&props);
    let container = container_with_column(zvcr::Dim::Nether, &column, 34);

    let converted = region::convert(
        &container,
        zvcr::Dim::Nether,
        &props,
        ColumnOpts::surface(zvcr::Dim::Nether),
    )
    .expect("convert");

    let (px, _) = first_pixel(&converted);
    assert_eq!(state_name(&converted, px), "minecraft:bedrock");
    assert_eq!(px.height, 127);
}

#[test]
fn water_becomes_an_overlay_and_lava_becomes_the_surface() {
    let props = props();
    let air = id_of(&props, "minecraft:air");
    let stone = id_of(&props, "minecraft:stone");
    let water = id_of(&props, "minecraft:water[level=0]");
    let lava = id_of(&props, "minecraft:lava[level=0]");

    let mut column = vec![air; 256];
    column[..60].fill(stone);
    column[60] = water;
    let converted = region::convert(
        &container_with_column(zvcr::Dim::End, &column, 56),
        zvcr::Dim::End,
        &props,
        ColumnOpts::surface(zvcr::Dim::End),
    )
    .expect("convert");
    let (px, _) = first_pixel(&converted);
    assert_eq!(state_name(&converted, px), "minecraft:stone");
    assert_eq!(px.height, 59);
    assert_eq!(px.overlays.len(), 1, "water sits on top as an overlay");
    // Water carries no palette entry: the format leaves it implicit.
    assert_eq!(px.overlays[0].state, None);
    // Height and top height differ, so the pixel carries the extra byte.
    assert_eq!(px.top_height, Some(60));

    column[60] = lava;
    let converted = region::convert(
        &container_with_column(zvcr::Dim::End, &column, 56),
        zvcr::Dim::End,
        &props,
        ColumnOpts::surface(zvcr::Dim::End),
    )
    .expect("convert");
    let (px, _) = first_pixel(&converted);
    assert_eq!(state_name(&converted, px), "minecraft:lava");
    assert_eq!(px.height, 60);
    assert!(px.overlays.is_empty(), "lava is opaque, never an overlay");
}

#[test]
fn an_empty_column_reads_as_air_at_the_bottom_of_the_world() {
    let props = props();
    let air = id_of(&props, "minecraft:air");
    let converted = region::convert(
        &container_with_column(zvcr::Dim::End, &vec![air; 256], 56),
        zvcr::Dim::End,
        &props,
        ColumnOpts::surface(zvcr::Dim::End),
    )
    .expect("convert");
    let (px, _) = first_pixel(&converted);
    assert_eq!(state_name(&converted, px), "minecraft:air");
    assert_eq!(px.height, 0);
    assert_eq!(
        converted.non_empty_tiles, 0,
        "pure void is not worth writing"
    );
}

#[test]
fn invisible_blocks_are_walked_through() {
    let props = props();
    let air = id_of(&props, "minecraft:air");
    let stone = id_of(&props, "minecraft:stone");
    let glass = id_of(&props, "minecraft:glass");
    let torch = id_of(&props, "minecraft:torch");

    let mut column = vec![air; 256];
    column[..60].fill(stone);
    column[60] = torch;
    column[61] = glass;
    let converted = region::convert(
        &container_with_column(zvcr::Dim::End, &column, 56),
        zvcr::Dim::End,
        &props,
        ColumnOpts::surface(zvcr::Dim::End),
    )
    .expect("convert");
    let (px, _) = first_pixel(&converted);
    // Plain glass and torches are explicitly invisible to the map, so neither
    // becomes the surface and neither becomes an overlay.
    assert_eq!(state_name(&converted, px), "minecraft:stone");
    assert_eq!(px.height, 59);
    assert!(px.overlays.is_empty());
}
