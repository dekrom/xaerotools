//! Region -> 512x512 RGBA renderer.

mod colortable;

pub use colortable::{BiomeColors, BlockColor, ColorTable, Tint};

use crate::model::*;

#[derive(Debug, Clone, Copy)]
pub enum LightMode {
    /// Surface look: block light ignored (sun-lit).
    Ignore,
    /// Cave/nether look: darkness scaled by stored block light.
    Multiply,
}

#[derive(Debug, Clone)]
pub struct RenderOpts {
    /// Slope-shading strength, 0.0 (flat) to ~1.5. Default 1.0.
    pub height_shade: f32,
    pub light_mode: LightMode,
    /// Paint unknown/missing blocks magenta instead of best-effort colors.
    pub debug_missing: bool,
}

impl Default for RenderOpts {
    fn default() -> Self {
        RenderOpts {
            height_shade: 1.0,
            light_mode: LightMode::Ignore,
            debug_missing: false,
        }
    }
}

const SIZE: usize = 512;

/// Renders a decoded region into a SIZE x SIZE RGBA buffer.
/// Image x = world X within the region, image y = world Z (north at top).
pub fn render_region(dr: &DecodedRegion, ct: &ColorTable, opts: &RenderOpts) -> Vec<u8> {
    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    let mut heights = vec![i16::MIN; SIZE * SIZE];

    // Last-wins chunk placement (files may repeat a marker).
    let mut slots: [Option<&TileChunk>; 64] = [None; 64];
    for (marker, chunk) in &dr.region.chunks {
        let cx = (marker >> 4) as usize;
        let cz = (marker & 0x0F) as usize;
        slots[cx * 8 + cz] = Some(chunk);
    }

    for (slot, chunk) in slots.iter().enumerate() {
        let Some(chunk) = chunk else { continue };
        let cx = slot / 8;
        let cz = slot % 8;
        for (t, tile) in chunk.tiles.iter().enumerate() {
            let Some(tile) = tile else { continue };
            let tx = t / 4;
            let tz = t % 4;
            for px in 0..TILE_PIXELS {
                for pz in 0..TILE_PIXELS {
                    let pixel = tile.pixel(px, pz);
                    let wx = cx * 64 + tx * 16 + px;
                    let wz = cz * 64 + tz * 16 + pz;
                    let idx = wz * SIZE + wx;
                    let color = shade_pixel(pixel, dr, ct, opts);
                    rgba[idx * 4..idx * 4 + 4].copy_from_slice(&color);
                    heights[idx] = pixel.height;
                }
            }
        }
    }

    if opts.height_shade > 0.0 {
        apply_slope_shading(&mut rgba, &heights, opts.height_shade);
    }
    rgba
}

fn state_name(dr: &DecodedRegion, idx: Option<u32>) -> &str {
    match idx {
        None => "minecraft:grass_block",
        Some(i) => dr
            .palettes
            .state_names
            .get(i as usize)
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("minecraft:stone"),
    }
}

fn biome_colors(dr: &DecodedRegion, ct: &ColorTable, biome: &Option<BiomeRef>) -> BiomeColors {
    match biome {
        Some(BiomeRef::Palette(i)) => match dr.palettes.biome_names.get(*i as usize) {
            Some(name) => ct.biome(name),
            None => ct.fallback_biome(),
        },
        _ => ct.fallback_biome(),
    }
}

fn shade_pixel(px: &Pixel, dr: &DecodedRegion, ct: &ColorTable, opts: &RenderOpts) -> [u8; 4] {
    let biome = biome_colors(dr, ct, &px.biome);
    let block = ct.block(state_name(dr, px.state));
    if block.missing && opts.debug_missing {
        return [0xFF, 0x00, 0xFF, 0xFF];
    }
    let mut color = apply_tint(block, &biome);

    // Overlays composite over the base block, bottom-up. Water depth (overlay
    // count) darkens the result slightly for the classic deep-water read.
    for ov in &px.overlays {
        let oc = match ov.state {
            None => {
                let mut water = ct.block("minecraft:water");
                water.tint = Tint::Water;
                apply_tint(water, &biome)
            }
            Some(i) => apply_tint(ct.block(state_name(dr, Some(i))), &biome),
        };
        let alpha = (0.35 + ov.opacity() as f32 * 0.03).min(0.92);
        for c in 0..3 {
            color[c] = (oc[c] as f32 * alpha + color[c] as f32 * (1.0 - alpha)) as u8;
        }
        color[3] = 255;
    }
    if px.overlays.len() > 1 {
        let depth = (px.overlays.len() - 1).min(6) as f32;
        let f = 1.0 - depth * 0.045;
        for c in color.iter_mut().take(3) {
            *c = (*c as f32 * f) as u8;
        }
    }

    if let LightMode::Multiply = opts.light_mode {
        let f = 0.25 + 0.75 * (px.light() as f32 / 15.0);
        for c in color.iter_mut().take(3) {
            *c = (*c as f32 * f) as u8;
        }
    }

    if color[3] == 0 && px.overlays.is_empty() {
        // Fully transparent block (air in a cave layer): leave transparent.
        return [0, 0, 0, 0];
    }
    color[3] = 255;
    color
}

fn apply_tint(block: BlockColor, biome: &BiomeColors) -> [u8; 4] {
    let t = match block.tint {
        Tint::None => return block.rgba,
        Tint::Grass => biome.grass,
        Tint::Foliage => biome.foliage,
        Tint::DryFoliage => biome.dry_foliage,
        Tint::Water => biome.water,
    };
    [
        ((block.rgba[0] as u16 * t[0] as u16) / 255) as u8,
        ((block.rgba[1] as u16 * t[1] as u16) / 255) as u8,
        ((block.rgba[2] as u16 * t[2] as u16) / 255) as u8,
        block.rgba[3],
    ]
}

/// Xaero-style relief: brighten slopes rising toward north/west, darken the
/// opposite. Region borders clamp to the pixel itself (1px seam accepted).
fn apply_slope_shading(rgba: &mut [u8], heights: &[i16], strength: f32) {
    for z in 0..SIZE {
        for x in 0..SIZE {
            let idx = z * SIZE + x;
            let h = heights[idx];
            if h == i16::MIN || rgba[idx * 4 + 3] == 0 {
                continue;
            }
            let west = if x > 0 { heights[idx - 1] } else { i16::MIN };
            let north = if z > 0 { heights[idx - SIZE] } else { i16::MIN };
            let west = if west == i16::MIN { h } else { west };
            let north = if north == i16::MIN { h } else { north };
            let slope = (h as i32 - west as i32) + (h as i32 - north as i32);
            if slope == 0 {
                continue;
            }
            let f = 1.0 + (slope.clamp(-8, 8) as f32) * 0.02 * strength;
            for c in 0..3 {
                let v = (rgba[idx * 4 + c] as f32 * f).clamp(0.0, 255.0);
                rgba[idx * 4 + c] = v as u8;
            }
        }
    }
}
