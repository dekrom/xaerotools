//! Region -> 512x512 RGBA renderer.
//!
//! Matches the in-game look: biome-blended tints, front-to-back overlay
//! compositing with sunlight attenuation, floor light under overlays, terrain
//! depth, cross-product slope shading with the per-dimension shadow color,
//! and display brightness for cave/nether layers (the game applies that last
//! step in its draw shader; we emit final pixels, so it is baked here).

mod colortable;

pub use colortable::{BiomeColors, BlockColor, ColorTable, Tint};

use crate::model::*;

#[derive(Debug, Clone)]
pub struct RenderOpts {
    /// Slope-shading strength, 0.0 (flat) to ~1.5. Default 1.0.
    pub height_shade: f32,
    /// Dimension ambient light, added to the 0.375 display floor of cave and
    /// full-cave tiles. Nether 0.1, everything else 0.0.
    pub dim_ambient: f32,
    /// Dimension logical height; scales terrain depth on full-cave tiles.
    /// Nether 128, everything else 384.
    pub logical_height: u32,
    /// Forces every tile's cave mode when Some (i32::MIN = full cave, other
    /// values = cave layer at that Y). None = each tile's stored value.
    pub cave_override: Option<i32>,
    /// Paint unknown/missing blocks magenta instead of best-effort colors.
    pub debug_missing: bool,
    /// See-through roof, mirroring XaeroPlus's Transparent Obsidian Roof.
    /// `None` paints what the region stores.
    pub roof: Option<RoofAlpha>,
}

/// Overlay opacities for the see-through roof, 0..255 as XaeroPlus states
/// them (its own defaults are 150 for obsidian and 10 for snow).
///
/// The rule is a block test, not a height test: XaeroPlus's `MixinMapPixel`
/// swaps the alpha of every obsidian, crying-obsidian and snow *overlay* at
/// draw time, because the Y limit already did its work when the region was
/// written — it decided which blocks became overlays at all. A region mapped
/// without that setting stores the roof as opaque terrain, and nothing here
/// can see under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoofAlpha {
    pub obsidian: u8,
    pub snow: u8,
}

impl Default for RenderOpts {
    fn default() -> Self {
        RenderOpts {
            height_shade: 1.0,
            dim_ambient: 0.0,
            logical_height: 384,
            cave_override: None,
            debug_missing: false,
            roof: None,
        }
    }
}

const SIZE: usize = 512;

/// Blocks the map treats as light sources: color boosted to a minimum
/// brightness and lit at full block light regardless of stored light.
fn is_glow_block(name: &str) -> bool {
    matches!(
        name,
        "minecraft:lava"
            | "minecraft:glowstone"
            | "minecraft:shroomlight"
            | "minecraft:sea_lantern"
            | "minecraft:jack_o_lantern"
            | "minecraft:lantern"
            | "minecraft:soul_lantern"
            | "minecraft:campfire"
            | "minecraft:soul_campfire"
            | "minecraft:fire"
            | "minecraft:soul_fire"
            | "minecraft:torch"
            | "minecraft:wall_torch"
            | "minecraft:soul_torch"
            | "minecraft:soul_wall_torch"
            | "minecraft:nether_portal"
            | "minecraft:end_rod"
            | "minecraft:beacon"
            | "minecraft:conduit"
            | "minecraft:crying_obsidian"
            | "minecraft:ochre_froglight"
            | "minecraft:verdant_froglight"
            | "minecraft:pearlescent_froglight"
            | "minecraft:lava_cauldron"
            | "minecraft:light"
            | "minecraft:end_gateway"
    )
}

fn is_air_block(name: &str) -> bool {
    matches!(
        name,
        "minecraft:air" | "minecraft:cave_air" | "minecraft:void_air"
    )
}

/// (9 + max(sun, light)) / 24 — the game's block brightness curve.
#[inline]
fn block_brightness(light: u8, sun: i32) -> f32 {
    (9 + sun.max(light as i32)) as f32 / 24.0
}

/// Which see-through-roof opacity a block takes, when that is switched on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RoofKind {
    None,
    Obsidian,
    Snow,
}

/// Everything the shader needs about one palette state, resolved once per
/// region so the pixel loop never touches the color table's hash maps.
#[derive(Clone, Copy)]
struct StateInfo {
    color: BlockColor,
    air: bool,
    glowing: bool,
    /// Overlay transparency 0..1: the table's alpha byte, or the vanilla
    /// per-material fallback when the table has none.
    alpha: f32,
    roof: RoofKind,
}

fn state_info(color: BlockColor, name: &str) -> StateInfo {
    let alpha = if color.rgba[3] != 0 {
        color.rgba[3] as f32 / 255.0
    } else if name.contains("water") || name.contains("lava") {
        191.0 / 255.0
    } else if name.contains("ice") {
        216.0 / 255.0
    } else {
        127.0 / 255.0
    };
    StateInfo {
        color,
        air: is_air_block(name),
        glowing: is_glow_block(name),
        alpha,
        roof: match name {
            "minecraft:obsidian" | "minecraft:crying_obsidian" => RoofKind::Obsidian,
            "minecraft:snow" => RoofKind::Snow,
            _ => RoofKind::None,
        },
    }
}

/// Renders a decoded region into a SIZE x SIZE RGBA buffer.
/// Image x = world X within the region, image y = world Z (north at top).
pub fn render_region(dr: &DecodedRegion, ct: &ColorTable, opts: &RenderOpts) -> Vec<u8> {
    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    let mut heights = vec![i16::MIN; SIZE * SIZE];
    let mut biome_grid = vec![u16::MAX; SIZE * SIZE];

    // Last-wins chunk placement (files may repeat a marker).
    let mut slots: [Option<&TileChunk>; 64] = [None; 64];
    for (marker, chunk) in &dr.region.chunks {
        let cx = (marker >> 4) as usize;
        let cz = (marker & 0x0F) as usize;
        slots[cx * 8 + cz] = Some(chunk);
    }

    // Per-palette-entry resolutions, so blending taps and the pixel loop cost
    // array reads rather than hash lookups.
    let biomes: Vec<BiomeColors> = dr
        .palettes
        .biome_names
        .iter()
        .map(|n| ct.biome(n))
        .collect();
    let states: Vec<StateInfo> = dr
        .palettes
        .state_names
        .iter()
        .map(|n| {
            let name = if n.is_empty() { "minecraft:stone" } else { n };
            state_info(ct.block(name), name)
        })
        .collect();
    let grass = state_info(ct.block("minecraft:grass_block"), "minecraft:grass_block");
    let stone = state_info(ct.block("minecraft:stone"), "minecraft:stone");
    let water = {
        let mut w = ct.block("minecraft:water");
        w.tint = Tint::Water;
        state_info(w, "minecraft:water")
    };

    // Pass 1: heights and biome indices for the whole region, so slope and
    // blend taps can cross tile and chunk boundaries.
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
                    heights[idx] = pixel.height;
                    if let Some(BiomeRef::Palette(i)) = pixel.biome
                        && (i as usize) < biomes.len()
                    {
                        biome_grid[idx] = i as u16;
                    }
                }
            }
        }
    }

    let (vs_grid, ds_grid) = slope_grids(&heights);

    let sh = Shader {
        biomes: &biomes,
        states: &states,
        grass,
        stone,
        water,
        fallback_biome: ct.fallback_biome(),
        biome_grid: &biome_grid,
        vs_grid: &vs_grid,
        ds_grid: &ds_grid,
        opts,
    };

    // Pass 2: shade.
    for (slot, chunk) in slots.iter().enumerate() {
        let Some(chunk) = chunk else { continue };
        let cx = slot / 8;
        let cz = slot % 8;
        for (t, tile) in chunk.tiles.iter().enumerate() {
            let Some(tile) = tile else { continue };
            let tx = t / 4;
            let tz = t % 4;
            let cave_start = opts.cave_override.unwrap_or(tile.cave_start);
            for px in 0..TILE_PIXELS {
                for pz in 0..TILE_PIXELS {
                    let pixel = tile.pixel(px, pz);
                    let wx = cx * 64 + tx * 16 + px;
                    let wz = cz * 64 + tz * 16 + pz;
                    let idx = wz * SIZE + wx;
                    let color = sh.shade(pixel, wx, wz, cave_start, tile.cave_depth);
                    rgba[idx * 4..idx * 4 + 4].copy_from_slice(&color);
                }
            }
        }
    }
    rgba
}

/// Per-pixel slope deltas: vs = h - h(x, z-1), ds = h - h(x-1, z-1). Where a
/// north/northwest neighbor is missing (region border, empty tile), the pixel
/// adopts the final slopes of its southeast neighbor — scanned in reverse so
/// the adoption chains — which is what keeps region borders seamless.
fn slope_grids(heights: &[i16]) -> (Vec<i8>, Vec<i8>) {
    let mut vs = vec![0i8; SIZE * SIZE];
    let mut ds = vec![0i8; SIZE * SIZE];
    let mut known = vec![false; SIZE * SIZE];
    for z in 1..SIZE {
        for x in 1..SIZE {
            let idx = z * SIZE + x;
            let h = heights[idx];
            let n = heights[idx - SIZE];
            let nw = heights[idx - SIZE - 1];
            if h == i16::MIN || n == i16::MIN || nw == i16::MIN {
                continue;
            }
            vs[idx] = (h as i32 - n as i32).clamp(-128, 127) as i8;
            ds[idx] = (h as i32 - nw as i32).clamp(-128, 127) as i8;
            known[idx] = true;
        }
    }
    for z in (0..SIZE).rev() {
        for x in (0..SIZE).rev() {
            let idx = z * SIZE + x;
            if known[idx] || z + 1 >= SIZE || x + 1 >= SIZE {
                continue;
            }
            let se = (z + 1) * SIZE + (x + 1);
            vs[idx] = vs[se];
            ds[idx] = ds[se];
        }
    }
    (vs, ds)
}

struct Shader<'a> {
    biomes: &'a [BiomeColors],
    states: &'a [StateInfo],
    grass: StateInfo,
    stone: StateInfo,
    water: StateInfo,
    fallback_biome: BiomeColors,
    biome_grid: &'a [u16],
    vs_grid: &'a [i8],
    ds_grid: &'a [i8],
    opts: &'a RenderOpts,
}

impl Shader<'_> {
    fn state(&self, idx: Option<u32>) -> &StateInfo {
        match idx {
            None => &self.grass,
            Some(i) => self.states.get(i as usize).unwrap_or(&self.stone),
        }
    }

    /// Biome tint averaged over the plus-shaped kernel around (x, z), per
    /// channel; taps outside the region or without a biome are skipped, and
    /// when none resolve the fallback biome stands in.
    fn blended_tint(&self, x: usize, z: usize, tint: Tint) -> [u8; 3] {
        let channel = |b: &BiomeColors| match tint {
            Tint::Grass => b.grass,
            Tint::Foliage => b.foliage,
            Tint::DryFoliage => b.dry_foliage,
            Tint::Water => b.water,
            Tint::None => unreachable!(),
        };
        const TAPS: [(i32, i32); 5] = [(0, 0), (-1, 0), (1, 0), (0, -1), (0, 1)];
        let mut sum = [0u32; 3];
        let mut total = 0u32;
        for (dx, dz) in TAPS {
            let tx = x as i32 + dx;
            let tz = z as i32 + dz;
            if !(0..SIZE as i32).contains(&tx) || !(0..SIZE as i32).contains(&tz) {
                continue;
            }
            let bi = self.biome_grid[tz as usize * SIZE + tx as usize];
            if bi == u16::MAX {
                continue;
            }
            let c = channel(&self.biomes[bi as usize]);
            sum[0] += c[0] as u32;
            sum[1] += c[1] as u32;
            sum[2] += c[2] as u32;
            total += 1;
        }
        if total == 0 {
            return channel(&self.fallback_biome);
        }
        [
            (sum[0] / total) as u8,
            (sum[1] / total) as u8,
            (sum[2] / total) as u8,
        ]
    }

    /// Block color x blended biome tint, as float channels.
    fn tinted(&self, info: &StateInfo, x: usize, z: usize) -> [f32; 3] {
        let c = info.color.rgba;
        if info.color.tint == Tint::None {
            return [c[0] as f32, c[1] as f32, c[2] as f32];
        }
        let t = self.blended_tint(x, z, info.color.tint);
        [
            ((c[0] as u16 * t[0] as u16) / 255) as f32,
            ((c[1] as u16 * t[1] as u16) / 255) as f32,
            ((c[2] as u16 * t[2] as u16) / 255) as f32,
        ]
    }

    fn shade(&self, px: &Pixel, x: usize, z: usize, cave_start: i32, cave_depth: u8) -> [u8; 4] {
        let info = self.state(px.state);
        let air = info.air;
        if air && px.overlays.is_empty() {
            // Air with nothing over it: transparent, not void-colored — holes
            // read better than filled void on a web map.
            return [0, 0, 0, 0];
        }
        let glowing = !air && info.glowing;

        let mut base = if air {
            [0.0; 3]
        } else {
            if info.color.missing && self.opts.debug_missing {
                return [0xFF, 0x00, 0xFF, 0xFF];
            }
            self.tinted(info, x, z)
        };
        let mut top_light = px.light() as i32;
        if glowing {
            let total = base[0] + base[1] + base[2];
            let brightener = (407.0 / total.max(1.0)).max(1.0);
            for c in &mut base {
                *c = (*c * brightener).trunc();
            }
            top_light = 15;
        }

        // Overlays front-to-back: each layer adds its lit color scaled by the
        // transparency left over from the layers above, and eats sunlight for
        // the layers (and floor) below.
        let mut sun: i32 = 15;
        let mut trans_mult = 1.0f32;
        let mut acc = [0.0f32; 3];
        for (i, ov) in px.overlays.iter().enumerate() {
            let oinfo = match ov.state {
                None => &self.water,
                Some(_) => self.state(ov.state),
            };
            let mut oc = self.tinted(oinfo, x, z);
            if oinfo.glowing {
                let total = oc[0] + oc[1] + oc[2];
                let brightener = (407.0 / total.max(1.0)).max(1.0);
                for c in &mut oc {
                    *c = (*c * brightener).trunc();
                }
            }
            if i == 0 {
                top_light = ov.light() as i32;
            }
            // A roof overlay draws at the opacity the instance shows it with,
            // not its own — that is the whole of the see-through-roof rule.
            let alpha = match (self.opts.roof, oinfo.roof) {
                (Some(r), RoofKind::Obsidian) => r.obsidian as f32 / 255.0,
                (Some(r), RoofKind::Snow) => r.snow as f32 / 255.0,
                _ => oinfo.alpha,
            };
            let intensity = block_brightness(ov.light(), sun) * alpha * trans_mult;
            acc[0] += oc[0] * intensity;
            acc[1] += oc[1] * intensity;
            acc[2] += oc[2] * intensity;
            sun = (sun - ov.effective_opacity()).max(0);
            trans_mult *= 1.0 - alpha;
        }
        let floor_bright = if !px.overlays.is_empty() && !glowing && !air {
            block_brightness(px.light(), sun)
        } else {
            1.0
        };

        // Terrain depth: higher ground is brighter, within a narrow band.
        let mut depth = 1.0f32;
        if !air && !glowing {
            depth = if cave_start == i32::MAX {
                px.height as f32 / 63.0
            } else if cave_start == i32::MIN {
                0.7 + 0.3 * px.height as f32 / self.opts.logical_height as f32
            } else {
                let bottom = cave_start - cave_depth as i32;
                0.7 + 0.3 * (px.height as i32 - bottom) as f32 / cave_depth as f32
            };
            depth = depth.clamp(0.9, 1.0);
        }

        // Slope shading: light from the northwest, per-channel ambient in the
        // dimension's shadow color. Applies to the base term only.
        let mut slope = [1.0f32; 3];
        if !air {
            let idx = z * SIZE + x;
            let vs = self.vs_grid[idx] as i32;
            let ds = self.ds_grid[idx] as i32;
            let (amb_colored, amb_white, max_direct) = if glowing {
                (0.0, 1.0, 0.22222224)
            } else {
                (0.2, 0.5, 0.6666667)
            };
            let mut cos = 0.0f32;
            let cross_z = -(vs as f32);
            if cross_z < 1.0 {
                if vs == 1 && ds == 1 {
                    cos = 1.0;
                } else {
                    let cross_x = (vs - ds) as f32;
                    let cast = 1.0 - cross_z;
                    let mag = (cross_x * cross_x + 1.0 + cross_z * cross_z).sqrt();
                    cos = (cast / mag) / std::f32::consts::SQRT_2;
                }
            }
            let direct = if cos == 1.0 {
                max_direct
            } else if cos > 0.0 {
                (cos * 10.0).ceil() / 10.0 * max_direct * 0.88388
            } else {
                0.0
            };
            let white = amb_white + direct;
            let shadow = if self.opts.dim_ambient > 0.0 {
                [1.0, 0.0, 0.0]
            } else {
                [0.518, 0.678, 1.0]
            };
            for c in 0..3 {
                let f = shadow[c] * amb_colored + white;
                slope[c] = 1.0 + (f - 1.0) * self.opts.height_shade;
            }
        }

        let mut out = [0u8; 4];
        for c in 0..3 {
            let v = base[c] * floor_bright * slope[c] * depth * trans_mult + acc[c];
            out[c] = v.clamp(0.0, 255.0) as u8;
        }
        // Cave and full-cave tiles carry no sky, so the stored light is the
        // picture: bake the display brightness the game's shader would apply.
        if cave_start != i32::MAX {
            let light_alpha = if top_light == 0 {
                0.0
            } else {
                (9 + top_light) as f32 / 24.0
            };
            let f = light_alpha.max(0.375 + self.opts.dim_ambient);
            for c in out.iter_mut().take(3) {
                *c = (*c as f32 * f) as u8;
            }
        }
        out[3] = 255;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::FormatVersion;

    /// One tile of stone under an obsidian overlay — the shape a spawn-roof
    /// region has once XaeroPlus records the roof as an overlay.
    fn roofed_region() -> DecodedRegion {
        let pixel = Pixel {
            params: P_HAS_OVERLAYS,
            state: Some(1),
            height: 250,
            legacy_height: None,
            top_height: None,
            overlays: smallvec::smallvec![Overlay {
                params: 0,
                state: Some(0),
                legacy_opacity: None,
            }],
            biome: None,
        };
        let tile = Tile {
            pixels: vec![pixel; TILE_PIXELS * TILE_PIXELS],
            interp_version: 0,
            cave_start: i32::MAX,
            cave_depth: 32,
        };
        let mut tiles = vec![None; CHUNK_TILES * CHUNK_TILES];
        tiles[0] = Some(tile);
        DecodedRegion {
            version: FormatVersion { major: 7, minor: 8 },
            region: Region {
                chunks: vec![(0, TileChunk { tiles })],
            },
            palettes: Palettes {
                states: vec![Vec::new(), Vec::new()],
                state_names: vec!["minecraft:obsidian".into(), "minecraft:stone".into()],
                biomes: Vec::new(),
                biome_names: Vec::new(),
            },
            truncated: false,
            trailing: 0,
        }
    }

    fn first_pixel(dr: &DecodedRegion, opts: &RenderOpts) -> [u8; 4] {
        let ct = ColorTable::parse(include_bytes!("../../../../assets/colortable.bin"))
            .expect("embedded color table");
        let rgba = render_region(dr, &ct, opts);
        [rgba[0], rgba[1], rgba[2], rgba[3]]
    }

    /// The see-through roof is the difference between painting the obsidian
    /// and painting what it covers. Without it the pixel is obsidian dark;
    /// with it the stone underneath carries most of the colour.
    #[test]
    fn roof_alpha_lets_the_floor_through() {
        let dr = roofed_region();
        let solid = first_pixel(&dr, &RenderOpts::default());
        let seen_through = first_pixel(
            &dr,
            &RenderOpts {
                roof: Some(RoofAlpha {
                    obsidian: 95,
                    snow: 10,
                }),
                ..Default::default()
            },
        );
        // Obsidian is (15, 10, 24) in the table; stone is (125, 125, 125).
        assert!(solid[0] < 40, "roof off should paint obsidian: {solid:?}");
        assert!(
            seen_through[0] > 60,
            "roof on should paint mostly stone: {seen_through:?}"
        );
        assert!(seen_through[1] > solid[1] + 40);
    }

    /// A block that is not part of a roof keeps its own opacity either way.
    #[test]
    fn roof_alpha_leaves_other_overlays_alone() {
        let mut dr = roofed_region();
        dr.palettes.state_names[0] = "minecraft:water".into();
        let plain = first_pixel(&dr, &RenderOpts::default());
        let with_roof = first_pixel(
            &dr,
            &RenderOpts {
                roof: Some(RoofAlpha {
                    obsidian: 95,
                    snow: 10,
                }),
                ..Default::default()
            },
        );
        assert_eq!(plain, with_roof);
    }
}
