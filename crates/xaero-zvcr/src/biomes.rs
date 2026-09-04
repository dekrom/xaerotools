//! Biome registry for protocol 769 (Minecraft 1.21.4).
//!
//! zvcr stores biomes as numeric ids into the game's biome registry, and Xaero
//! stores them as namespaced strings, so the importer needs this mapping. The
//! table is transcribed from the world download's own
//! `registries/769/biome_properties.json` — the same registry its writer used —
//! rather than derived independently, so the ids cannot drift from the files.
//!
//! Regenerate for another protocol version from that file; ids are contiguous
//! and the order is the registry order, not alphabetical by accident.

/// Protocol version this table describes; zvcr headers record the same number.
pub const PROTOCOL: u16 = 769;

pub const BIOMES: [&str; 65] = [
    "minecraft:badlands",
    "minecraft:bamboo_jungle",
    "minecraft:basalt_deltas",
    "minecraft:beach",
    "minecraft:birch_forest",
    "minecraft:cherry_grove",
    "minecraft:cold_ocean",
    "minecraft:crimson_forest",
    "minecraft:dark_forest",
    "minecraft:deep_cold_ocean",
    "minecraft:deep_dark",
    "minecraft:deep_frozen_ocean",
    "minecraft:deep_lukewarm_ocean",
    "minecraft:deep_ocean",
    "minecraft:desert",
    "minecraft:dripstone_caves",
    "minecraft:end_barrens",
    "minecraft:end_highlands",
    "minecraft:end_midlands",
    "minecraft:eroded_badlands",
    "minecraft:flower_forest",
    "minecraft:forest",
    "minecraft:frozen_ocean",
    "minecraft:frozen_peaks",
    "minecraft:frozen_river",
    "minecraft:grove",
    "minecraft:ice_spikes",
    "minecraft:jagged_peaks",
    "minecraft:jungle",
    "minecraft:lukewarm_ocean",
    "minecraft:lush_caves",
    "minecraft:mangrove_swamp",
    "minecraft:meadow",
    "minecraft:mushroom_fields",
    "minecraft:nether_wastes",
    "minecraft:ocean",
    "minecraft:old_growth_birch_forest",
    "minecraft:old_growth_pine_taiga",
    "minecraft:old_growth_spruce_taiga",
    "minecraft:pale_garden",
    "minecraft:plains",
    "minecraft:river",
    "minecraft:savanna",
    "minecraft:savanna_plateau",
    "minecraft:small_end_islands",
    "minecraft:snowy_beach",
    "minecraft:snowy_plains",
    "minecraft:snowy_slopes",
    "minecraft:snowy_taiga",
    "minecraft:soul_sand_valley",
    "minecraft:sparse_jungle",
    "minecraft:stony_peaks",
    "minecraft:stony_shore",
    "minecraft:sunflower_plains",
    "minecraft:swamp",
    "minecraft:taiga",
    "minecraft:the_end",
    "minecraft:the_void",
    "minecraft:warm_ocean",
    "minecraft:warped_forest",
    "minecraft:windswept_forest",
    "minecraft:windswept_gravelly_hills",
    "minecraft:windswept_hills",
    "minecraft:windswept_savanna",
    "minecraft:wooded_badlands",
];

/// Namespaced id for a zvcr biome id. Unknown ids fall back to `the_void`,
/// which is what an empty column would carry anyway.
pub fn name(id: u16) -> &'static str {
    BIOMES
        .get(id as usize)
        .copied()
        .unwrap_or("minecraft:the_void")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_ids_resolve() {
        assert_eq!(name(56), "minecraft:the_end");
        assert_eq!(name(34), "minecraft:nether_wastes");
        assert_eq!(name(44), "minecraft:small_end_islands");
        assert_eq!(name(u16::MAX), "minecraft:the_void");
    }
}
