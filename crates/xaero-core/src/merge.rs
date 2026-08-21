//! Tile-granular merge of two decoded regions.
//!
//! Rules (per plan): the merged region takes every tile present in the
//! preferred ("newer") source; tiles absent there are filled from the other
//! source. Palette indices are region-local, so pixels are remapped into a
//! fresh combined palette (deduplicated on raw bytes).

use std::collections::HashMap;

use crate::model::*;

/// Merges `primary` over `secondary` (primary wins where both have a tile).
/// Returns a region + palettes ready for `encode_region`.
pub fn merge_regions(primary: &DecodedRegion, secondary: &DecodedRegion) -> DecodedRegion {
    let mut out_pal = Palettes::default();
    let mut state_dedup: HashMap<&[u8], u32> = HashMap::new();
    let mut biome_dedup: HashMap<&[u8], u32> = HashMap::new();

    // Last-wins chunk views of both sources.
    let chunk_of = |dr: &DecodedRegion, cx: u8, cz: u8| dr.region.chunk(cx, cz).cloned();

    let mut chunks = Vec::new();
    for cx in 0u8..8 {
        for cz in 0u8..8 {
            let pc = chunk_of(primary, cx, cz);
            let sc = chunk_of(secondary, cx, cz);
            if pc.is_none() && sc.is_none() {
                continue;
            }
            let mut tiles: Vec<Option<Tile>> = Vec::with_capacity(16);
            for t in 0..16 {
                let from_primary = pc.as_ref().and_then(|c| c.tiles[t].clone());
                let (tile, src) = match from_primary {
                    Some(tile) => (Some(tile), primary),
                    None => (sc.as_ref().and_then(|c| c.tiles[t].clone()), secondary),
                };
                tiles.push(tile.map(|tile| {
                    remap_tile(
                        tile,
                        &src.palettes,
                        &mut out_pal,
                        &mut state_dedup,
                        &mut biome_dedup,
                    )
                }));
            }
            if tiles.iter().all(|t| t.is_none()) {
                continue;
            }
            chunks.push(((cx << 4) | cz, TileChunk { tiles }));
        }
    }

    // SAFETY of the dedup maps' borrowed keys: they reference bytes owned by
    // `out_pal`, which only grows (Vec push) — but Vec reallocation moves the
    // outer Vec, not the boxed byte buffers, so &[u8] into the heap blocks
    // stays valid. To keep this obviously sound without unsafe, the maps
    // actually borrow from the SOURCE palettes (which outlive this call).
    DecodedRegion {
        version: crate::codec::FormatVersion {
            major: crate::WRITE_MAJOR,
            minor: crate::WRITE_MINOR,
        },
        region: Region { chunks },
        palettes: out_pal,
        truncated: false,
        trailing: 0,
    }
}

/// Rewrites a tile's palette references from `src` palettes into `out`.
fn remap_tile<'a>(
    mut tile: Tile,
    src: &'a Palettes,
    out: &mut Palettes,
    state_dedup: &mut HashMap<&'a [u8], u32>,
    biome_dedup: &mut HashMap<&'a [u8], u32>,
) -> Tile {
    fn map_state<'a>(
        idx: u32,
        src: &'a Palettes,
        out: &mut Palettes,
        dedup: &mut HashMap<&'a [u8], u32>,
    ) -> u32 {
        match src.states.get(idx as usize) {
            None => idx, // dangling in a corrupt source: pass through
            Some(raw) => *dedup.entry(raw.as_slice()).or_insert_with(|| {
                out.push_state(raw.clone(), src.state_names[idx as usize].clone())
            }),
        }
    }
    for px in &mut tile.pixels {
        if let Some(s) = px.state {
            px.state = Some(map_state(s, src, out, state_dedup));
        }
        for ov in &mut px.overlays {
            if let Some(s) = ov.state {
                ov.state = Some(map_state(s, src, out, state_dedup));
            }
        }
        if let Some(BiomeRef::Palette(b)) = px.biome {
            px.biome = Some(BiomeRef::Palette(match src.biomes.get(b as usize) {
                None => b,
                Some(raw) => *biome_dedup.entry(raw.as_slice()).or_insert_with(|| {
                    out.push_biome(raw.clone(), src.biome_names[b as usize].clone())
                }),
            }));
        }
    }
    tile
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode_region, encode_region};
    use smallvec::smallvec;

    fn tiny_region(chunk_marker: u8, tile_slot: usize, height: i16, biome: &str) -> DecodedRegion {
        let mut palettes = Palettes::default();
        // one non-grass pixel state: {Name:"minecraft:stone"}
        let mut nbt = vec![0x0A, 0x00, 0x00, 0x08, 0x00, 0x04];
        nbt.extend_from_slice(b"Name");
        nbt.extend_from_slice(&[0x00, 0x0F]);
        nbt.extend_from_slice(b"minecraft:stone");
        nbt.push(0x00);
        let st = palettes.push_state(nbt, "minecraft:stone".into());
        let bi = palettes.push_biome(biome.as_bytes().to_vec(), biome.into());
        let mk_pixel = |first: bool| {
            let h = (height as i32) & 0xFFF;
            let mut params: u32 = P_NOT_GRASS | P_BIOME;
            params |= ((h as u32) & 0xFF) << 12;
            params |= (((h as u32) >> 8) & 0xF) << 25;
            if first {
                params |= P_STATE_NEW | P_BIOME_NEW;
            }
            Pixel {
                params,
                state: Some(st),
                height: 0,
                legacy_height: None,
                top_height: None,
                overlays: smallvec![],
                biome: Some(BiomeRef::Palette(bi)),
            }
        };
        let mut pixels = vec![mk_pixel(true)];
        pixels.extend((1..256).map(|_| mk_pixel(false)));
        let tile = Tile {
            pixels,
            interp_version: 1,
            cave_start: 0,
            cave_depth: 32,
        };
        let mut tiles: Vec<Option<Tile>> = (0..16).map(|_| None).collect();
        tiles[tile_slot] = Some(tile);
        DecodedRegion {
            version: crate::codec::FormatVersion { major: 7, minor: 8 },
            region: Region {
                chunks: vec![(chunk_marker, TileChunk { tiles })],
            },
            palettes,
            truncated: false,
            trailing: 0,
        }
    }

    #[test]
    fn merge_disjoint_and_conflicting() {
        let a = tiny_region(0x00, 0, 100, "minecraft:plains"); // chunk(0,0) tile 0
        let b = tiny_region(0x00, 1, 50, "minecraft:desert"); // chunk(0,0) tile 1
        let c = tiny_region(0x23, 5, -7, "minecraft:plains"); // chunk(2,3) tile 5

        // a+b: same chunk, disjoint tiles -> both present
        let m = merge_regions(&a, &b);
        let stream = encode_region(&m);
        let d = decode_region(&stream).unwrap();
        assert!(!d.truncated);
        let ch = d.region.chunk(0, 0).unwrap();
        assert!(ch.tiles[0].is_some() && ch.tiles[1].is_some());
        assert_eq!(ch.tiles[0].as_ref().unwrap().pixels[0].height, 100);
        assert_eq!(ch.tiles[1].as_ref().unwrap().pixels[0].height, 50);
        assert_eq!(d.palettes.state_names, vec!["minecraft:stone"]); // deduped
        assert_eq!(
            d.palettes.biome_names,
            vec!["minecraft:plains", "minecraft:desert"]
        );

        // conflict: same chunk+tile, primary wins
        let a2 = tiny_region(0x00, 0, 42, "minecraft:plains");
        let m2 = merge_regions(&a2, &a);
        let d2 = decode_region(&encode_region(&m2)).unwrap();
        assert_eq!(
            d2.region.chunk(0, 0).unwrap().tiles[0]
                .as_ref()
                .unwrap()
                .pixels[0]
                .height,
            42
        );

        // different chunks merge side by side, negative heights survive
        let m3 = merge_regions(&a, &c);
        let d3 = decode_region(&encode_region(&m3)).unwrap();
        assert!(d3.region.chunk(0, 0).is_some());
        assert_eq!(
            d3.region.chunk(2, 3).unwrap().tiles[5]
                .as_ref()
                .unwrap()
                .pixels[0]
                .height,
            -7
        );
    }
}
