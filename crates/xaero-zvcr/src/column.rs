//! Port of Xaero's World Map column algorithm — `MapWriter.loadPixel` and
//! `loadPixelHelp` — running over zvcr chunk data instead of a live level.
//!
//! The structure deliberately mirrors the decompiled original: the same walk
//! down the column, the same order of tests, the same order of side effects on
//! `top_h` and the overlay builder. Where a step needs something the game had
//! and a world download does not, the substitution is called out below.
//!
//! # Nether roof removal
//!
//! XaeroPlus's Nether Cave Fix flips only `loadPixel`'s `cave`/`full_cave`
//! *arguments* when the dimension is the Nether — `writeChunk` still treats the
//! tile as a surface tile. With both true the walk enters the first solid block
//! it meets (the bedrock roof), walks out the far side, and maps the first
//! floor under the next air gap. Player altitude never enters into it. The
//! tile trailer still records `cave_start = i32::MAX`, so the result renders on
//! the surface path with no cave darkening — matching what this archive's own
//! Nether regions contain.
//!
//! # Substitutions
//!
//! * **Block light.** zvcr stores blocks, biomes and block entities — no light
//!   arrays — so `world.getBrightness(BLOCK, above)` cannot be reproduced
//!   without a full propagation pass over every column. Instead the emission of
//!   the block above the surface is used, falling back to the surface block's
//!   own emission less one (a source lights the cell beside it at `E - 1`).
//!   Stored light only reaches the renderer through `(9 + max(sun, light)) / 24`
//!   with `sun` starting at 15 and only overlays reducing it, and neither the
//!   Nether nor the End has meaningful overlays, so the value is not visible in
//!   either dimension's output. The glow flag, which *is* visible, comes from
//!   the block's real emission and is exact.
//! * **Overlay merging.** `OverlayBuilder.build` starts a new overlay when the
//!   block's *particle texture* differs from the previous one. Texture identity
//!   needs the client's baked models, so block identity stands in: states of
//!   one block always share a texture, and distinct blocks almost always
//!   differ.
//! * **Translucency.** Xaero's `shouldConsiderBlockTranslucent` lives in a
//!   library that ships with neither jar here, so it is reconstructed as
//!   "translucent render layer and a full-cube shape". That reproduces this
//!   archive's own data exactly: stained glass is an overlay, stained glass
//!   panes and nether portals are not, which is why real Nether regions contain
//!   no overlays at all.
//! * **Biome zoom.** `seg.biome` reads the 4×4×4 quart a column sits in. The
//!   game's `BiomeGetter` goes through `Level.getBiome`, which is
//!   `BiomeManager`'s seeded fuzzy zoom: near a quart boundary it may answer
//!   with a neighbouring quart's biome. Reproducing that needs the world's
//!   hashed biome seed, which the download does not carry, so columns on a
//!   biome boundary can differ. This is the ~99.6% biome agreement measured on
//!   End regions, against 100% in the single-biome Nether.

use smallvec::SmallVec;
use xaero_core::model::*;

use crate::blockprops::{BlockProps, flag};
use crate::zvcr::SegmentView;

/// Xaero's `OverlayBuilder` cap.
const MAX_OVERLAYS: usize = 10;
/// The Y `writeChunk` starts `getSectionBasedHeight`'s search from.
const SECTION_SEARCH_START_Y: i32 = 64;
/// `loadPixel`'s magic gap: five or more blocks below the first transparent
/// state, the run is collapsed into one thick overlay instead of one per block.
const TRANSPARENCY_BLEND_DEPTH: i32 = 5;

#[derive(Debug, Clone, Copy)]
pub struct ColumnOpts {
    /// `cave` as seen *inside* `loadPixel` (the Nether fix forces it true).
    pub cave: bool,
    /// `full_cave` as seen inside `loadPixel`.
    pub full_cave: bool,
    /// Xaero's "Display flowers" setting; the mod defaults it to on.
    pub flowers: bool,
    /// `world.dimensionType().hasSkyLight()` — false for the Nether and the End.
    pub has_sky_light: bool,
    /// `world.getMinY()`, in Minecraft coordinates.
    pub world_bottom_y: i32,
    /// `world.getMaxY() + 1`, exclusive.
    pub world_top_y: i32,
}

impl ColumnOpts {
    /// Settings a vanilla surface dimension is written with.
    pub fn surface(dim: crate::zvcr::Dim) -> ColumnOpts {
        ColumnOpts {
            cave: false,
            full_cave: false,
            flowers: true,
            has_sky_light: dim.has_sky_light(),
            world_bottom_y: dim.min_y(),
            world_top_y: dim.min_y() + dim.height(),
        }
    }
    /// Settings XaeroPlus's Nether Cave Fix produces: roof removal on an
    /// otherwise ordinary surface tile.
    pub fn nether_roof_removal(dim: crate::zvcr::Dim) -> ColumnOpts {
        ColumnOpts {
            cave: true,
            full_cave: true,
            ..ColumnOpts::surface(dim)
        }
    }
}

/// Reusable scratch for one thread. Holds exactly the mutable state the
/// original keeps on the `MapWriter` instance between calls.
pub struct ColumnWriter<'a> {
    props: &'a BlockProps,
    opts: ColumnOpts,
    top_h: i32,
    first_transparent_state_y: i32,
    overlays: SmallVec<[BuildOverlay; MAX_OVERLAYS]>,
    overlay_biome: Option<u16>,
    /// Stands in for `OverlayBuilder.prevMaterial`, which the mod deliberately
    /// carries across pixels rather than resetting per column.
    prev_material: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
struct BuildOverlay {
    state: u32,
    light: u8,
    opacity: i32,
}

impl<'a> ColumnWriter<'a> {
    pub fn new(props: &'a BlockProps, opts: ColumnOpts) -> ColumnWriter<'a> {
        ColumnWriter {
            props,
            opts,
            top_h: 0,
            first_transparent_state_y: 0,
            overlays: SmallVec::new(),
            overlay_biome: None,
            prev_material: None,
        }
    }

    /// `MapWriter.loadPixel` for one block column of a segment.
    ///
    /// `x`/`z` are chunk-local (0..16); `start_height` is the Minecraft Y the
    /// walk begins at, as `writeChunk` computes it.
    pub fn pixel(&mut self, seg: &SegmentView<'_>, x: usize, z: usize, start_height: i32) -> Pixel {
        let props = self.props;
        let o = self.opts;
        let low_y = o.world_bottom_y;

        self.overlays.clear();
        self.overlay_biome = None;

        let mut underair = !o.cave || o.full_cave;
        let mut should_enter_ground = o.full_cave;
        let mut opaque_state: Option<u32> = None;
        let mut working_light: u8 = 0;
        let working_sky_light: u8 = if o.has_sky_light { 15 } else { 0 };
        self.top_h = low_y;
        self.first_transparent_state_y = low_y;

        let mut should_extend = false;
        let mut transparent_skip_y = 0i32;
        let mut h = start_height;

        while h >= low_y {
            let state = self.block_at(seg, x, h, z);
            let has_fluid = props.has_fluid(state);
            let fluid_block = props.fluid_legacy(state);

            // A transparent run at least five blocks deep collapses: trace down
            // to where it ends and charge the whole depth to one overlay.
            should_extend = !should_extend
                && !self.overlays.is_empty()
                && self.first_transparent_state_y - h >= TRANSPARENCY_BLEND_DEPTH;
            if should_extend {
                transparent_skip_y = h - 1;
                while transparent_skip_y >= low_y {
                    let trace = self.block_at(seg, x, transparent_skip_y, z);
                    if props.has_fluid(trace) {
                        if !self.fluid_should_overlay(trace) {
                            break;
                        }
                        if !props.is_air(trace)
                            && props.block_of(trace) == props.block_of(props.fluid_legacy(trace))
                        {
                            transparent_skip_y -= 1;
                            continue;
                        }
                    }
                    if !self.state_should_overlay(trace) {
                        break;
                    }
                    transparent_skip_y -= 1;
                }
            }

            working_light = self.block_light(seg, x, h, z);

            // The fluid pass is skipped entirely while the walk is still
            // looking for ground to enter, which is what lets roof removal
            // dive past lava sitting on the Nether roof.
            if has_fluid && !(o.cave && should_enter_ground) {
                underair = true;
                if self.pixel_help(
                    seg,
                    fluid_block,
                    Some(state),
                    working_light,
                    working_sky_light,
                    x,
                    z,
                    h,
                    transparent_skip_y,
                    should_extend,
                    underair,
                ) {
                    opaque_state = Some(state);
                    break;
                }
            }

            if props.is_air(state) {
                underair = true;
            } else if underair && props.block_of(state) != props.block_of(fluid_block) {
                if o.cave && should_enter_ground {
                    let soft = props.flags(state)
                        & (flag::IGNITED_BY_LAVA | flag::CAN_BE_REPLACED | flag::PUSH_DESTROY)
                        != 0
                        || self.state_should_overlay(state);
                    if !soft {
                        underair = false;
                        should_enter_ground = false;
                    }
                } else if self.pixel_help(
                    seg,
                    state,
                    None,
                    working_light,
                    working_sky_light,
                    x,
                    z,
                    h,
                    transparent_skip_y,
                    should_extend,
                    underair,
                ) {
                    opaque_state = Some(state);
                    break;
                }
            }

            h = if should_extend {
                transparent_skip_y
            } else {
                h - 1
            };
        }

        if h < low_y {
            h = low_y;
        }

        let state = opaque_state.unwrap_or_else(|| props.air());
        let mut light = 0u8;
        if opaque_state.is_some() {
            light = working_light;
            if o.cave && light < 15 && self.overlays.is_empty() && working_sky_light > light {
                light = working_sky_light;
            }
        } else {
            // Nothing mappable in the whole column: the void reads as air at
            // the bottom of the world.
            h = o.world_bottom_y;
        }

        let biome = self
            .overlay_biome
            .unwrap_or_else(|| seg.biome(x, self.zvcr_y(self.top_h), z));

        self.finish(state, h, self.top_h, biome, light)
    }

    /// Builds the `Pixel` the encoder wants, packing the parameter word exactly
    /// as `MapBlock.getParametres` does.
    fn finish(&self, state: u32, height: i32, top_height: i32, biome: u16, light: u8) -> Pixel {
        let props = self.props;
        let is_grass = props.flags(state) & flag::GRASS_BLOCK != 0;
        let mut params = 0u32;
        if !is_grass {
            params |= P_NOT_GRASS;
        }
        if !self.overlays.is_empty() {
            params |= P_HAS_OVERLAYS;
        }
        params |= (light as u32 & 0xF) << 8;
        params |= (height as u32 & 0xFF) << 12;
        params |= P_BIOME;
        if height != top_height {
            params |= P_TOP_HEIGHT;
        }
        params |= ((height >> 8) as u32 & 0xF) << 25;

        let overlays = self
            .overlays
            .iter()
            .map(|o| {
                let is_water = props.flags(o.state) & flag::WATER_BLOCK != 0;
                let mut p = 0u32;
                if !is_water {
                    p |= O_NOT_WATER;
                }
                p |= (o.light as u32 & 0xF) << 4;
                p |= (o.opacity.clamp(0, 15) as u32) << 11;
                Overlay {
                    params: p,
                    // Water is implicit in the format and carries no palette
                    // entry, exactly as `saveOverlay` writes it.
                    state: if is_water { None } else { Some(o.state) },
                    legacy_opacity: None,
                }
            })
            .collect();

        Pixel {
            params,
            state: if is_grass { None } else { Some(state) },
            height: height as i16,
            legacy_height: None,
            // The shipped writer truncates top height to a byte; matching that
            // keeps our files indistinguishable from the mod's own.
            top_height: if height != top_height {
                Some(top_height as u8)
            } else {
                None
            },
            overlays,
            biome: Some(BiomeRef::Palette(biome as u32)),
        }
    }

    /// `MapWriter.loadPixelHelp`. Returns true when the walk should stop here.
    #[allow(clippy::too_many_arguments)]
    fn pixel_help(
        &mut self,
        seg: &SegmentView<'_>,
        state: u32,
        // `Some` when this call came from the fluid pass: the overlay test then
        // asks about the fluid, not the block holding it.
        fluid_carrier: Option<u32>,
        light: u8,
        sky_light: u8,
        x: usize,
        z: usize,
        h: i32,
        transparent_skip_y: i32,
        should_extend: bool,
        underair: bool,
    ) -> bool {
        let props = self.props;
        let o = self.opts;
        if self.is_invisible(state) {
            return false;
        }

        let overlays = match fluid_carrier {
            Some(carrier) => self.fluid_should_overlay(carrier),
            None => self.state_should_overlay(state),
        };
        if overlays {
            if o.cave && !underair {
                return false;
            }
            if h > self.top_h {
                self.top_h = h;
            }
            let mut overlay_light = light;
            if self.overlays.is_empty() {
                self.first_transparent_state_y = h;
                if o.cave && sky_light > overlay_light {
                    overlay_light = sky_light;
                }
            }
            if should_extend {
                if let Some(cur) = self.overlays.last_mut() {
                    let dampening = props.light_block(cur.state) as i32;
                    let add = dampening * (h - transparent_skip_y);
                    cur.opacity = (cur.opacity + add.min(15)).min(15);
                }
            } else {
                let biome = self
                    .overlay_biome
                    .unwrap_or_else(|| seg.biome(x, self.zvcr_y(h), z));
                self.build_overlay(state, props.light_block(state) as i32, overlay_light, biome);
            }
            return false;
        }

        if props.flags(state) & flag::HAS_MAP_COLOR == 0 {
            return false;
        }
        if o.cave && !underair {
            return true;
        }
        if h > self.top_h {
            self.top_h = h;
        }
        true
    }

    /// `OverlayBuilder.build`.
    fn build_overlay(&mut self, state: u32, opacity: i32, light: u8, biome: u16) {
        let current = self.overlays.last().copied();
        let mut material = None;
        let mut changed = false;
        if current.is_none_or(|c| c.state != state) {
            let m = self.props.block_of(state);
            material = Some(m);
            changed = material != self.prev_material;
        }
        if self.overlays.len() < MAX_OVERLAYS && (current.is_none() || changed) {
            if self.overlay_biome.is_none() {
                self.overlay_biome = Some(biome);
            }
            self.overlays.push(BuildOverlay {
                state,
                light,
                opacity: 0,
            });
        }
        if let Some(cur) = self.overlays.last_mut() {
            cur.opacity = (cur.opacity + opacity.min(15)).min(15);
        }
        if changed {
            self.prev_material = material;
        }
    }

    /// `MapWriter.isInvisible`. The mod's `buggedStates` list is a runtime
    /// blacklist of states whose map colour threw; the extractor already
    /// resolves that case into a cleared `HAS_MAP_COLOR` flag.
    fn is_invisible(&self, state: u32) -> bool {
        let f = self.props.flags(state);
        if f & flag::RENDER_INVISIBLE != 0 {
            return true;
        }
        if f & (flag::TORCH | flag::SHORT_GRASS | flag::GLASS_OR_PANE) != 0 {
            return true;
        }
        let is_flower = f & flag::FLOWERISH != 0;
        if f & flag::DOUBLE_PLANT != 0 && !is_flower {
            return true;
        }
        if is_flower && !self.opts.flowers {
            return true;
        }
        false
    }

    /// `MapWriter.shouldOverlay` for a block state.
    fn state_should_overlay(&self, state: u32) -> bool {
        let f = self.props.flags(state);
        if f & (flag::AIR | flag::TRANSPARENT_CLASS) != 0 {
            return true;
        }
        f & flag::TRANSLUCENT_LAYER != 0 && f & flag::SHAPE_FULL_BLOCK != 0
    }

    /// `MapWriter.shouldOverlay` for the fluid a state carries. Water is
    /// translucent, lava is not — which is why Nether lava lakes are surfaces
    /// rather than overlays.
    fn fluid_should_overlay(&self, state: u32) -> bool {
        self.props.flags(state) & flag::FLUID_TRANSLUCENT != 0
    }

    #[inline]
    fn block_at(&self, seg: &SegmentView<'_>, x: usize, y: i32, z: usize) -> u32 {
        let zy = y - self.opts.world_bottom_y;
        if zy < 0 || zy >= (seg.sections * 16) as i32 {
            return self.props.air();
        }
        seg.block(x, zy as usize, z) as u32
    }

    #[inline]
    fn zvcr_y(&self, y: i32) -> usize {
        (y - self.opts.world_bottom_y).max(0) as usize
    }

    /// Stand-in for `world.getBrightness(BLOCK, pos above the surface)`; see
    /// the module note on substitutions.
    fn block_light(&self, seg: &SegmentView<'_>, x: usize, h: i32, z: usize) -> u8 {
        let above = self.props.emission(self.block_at(seg, x, h + 1, z));
        if above > 0 {
            return above;
        }
        self.props
            .emission(self.block_at(seg, x, h, z))
            .saturating_sub(1)
    }
}

/// `MapWriter.getSectionBasedHeight`: the top of the highest non-empty section
/// at or above the search start, falling back to the highest one below it.
/// Used when a column's heightmap says it holds no blocks at all.
pub fn section_based_height(
    seg: &SegmentView<'_>,
    min_y: i32,
    props: &BlockProps,
    _air: u32,
) -> i32 {
    let sections = seg.sections;
    if sections == 0 {
        return 0;
    }
    let has_only_air = |s: usize| {
        let base = s * crate::zvcr::BLOCKS_PER_SECTION;
        seg.blocks[base..base + crate::zvcr::BLOCKS_PER_SECTION]
            .iter()
            .all(|&b| props.is_air(b as u32))
    };
    let start = (((SECTION_SEARCH_START_Y - min_y) >> 4) as usize).min(sections - 1);
    let mut result = 0;
    for s in start..sections {
        if !has_only_air(s) {
            result = min_y + (s as i32) * 16 + 15;
        }
    }
    if start > 0 && result == 0 {
        for s in (0..start).rev() {
            if !has_only_air(s) {
                result = min_y + (s as i32) * 16 + 15;
                break;
            }
        }
    }
    result
}

/// `chunk.getHeight(Heightmap.Types.WORLD_SURFACE, x, z)`: the Y of the topmost
/// non-air block, or `min_y - 1` when the column is empty.
pub fn world_surface_height(
    seg: &SegmentView<'_>,
    props: &BlockProps,
    x: usize,
    z: usize,
    min_y: i32,
) -> i32 {
    for zy in (0..seg.sections * 16).rev() {
        if !props.is_air(seg.block(x, zy, z) as u32) {
            return min_y + zy as i32;
        }
    }
    min_y - 1
}
