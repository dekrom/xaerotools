//! Legacy (major-0) block and biome identity tables.
//!
//! Majors 0..=3 store a numeric 1.12 block id + metadata instead of an NBT
//! blockstate, and majors 0..=3 store a numeric biome id instead of a name.
//! Xaero resolves both through tables baked into the mod jar; we bake the same
//! data so a legacy region decodes to the exact names the game would show.
//!
//! `assets/legacy_block_ids.bin` is generated from
//! `assets/xaeroworldmap/vanilla_states.dat` inside the world-map jar, with the
//! mod's own `fixBlock(state, 1)` renames already applied (`grass_path` ->
//! `dirt_path`, `sign` -> `oak_sign`, `wall_sign` -> `oak_wall_sign`,
//! `stone_slab` -> `smooth_stone_slab`). Every resulting name is present in
//! `assets/colortable.bin`, so legacy rendering needs no new colour data.
//!
//! Regenerate with:
//! ```text
//! unzip -p xaeroworldmap-fabric-*.jar assets/xaeroworldmap/vanilla_states.dat
//! ```
//!
//! This module is WASM-clean: the table is embedded at compile time and parsed
//! lazily into a static, with no filesystem access.

use std::sync::OnceLock;

/// `XLB1` table: 4096 dense slots indexed by `(id << 4) | meta`.
static RAW: &[u8] = include_bytes!("../../../../assets/legacy_block_ids.bin");

const SLOTS: usize = 4096;
const NO_ENTRY: u16 = 0xFFFF;

struct LegacyBlocks {
    names: Vec<String>,
    /// index = (id << 4) | meta -> index into `names`, or `NO_ENTRY`.
    table: Vec<u16>,
}

fn table() -> &'static LegacyBlocks {
    static T: OnceLock<LegacyBlocks> = OnceLock::new();
    T.get_or_init(|| {
        parse(RAW).unwrap_or_else(|| LegacyBlocks {
            names: Vec::new(),
            table: vec![NO_ENTRY; SLOTS],
        })
    })
}

fn parse(buf: &[u8]) -> Option<LegacyBlocks> {
    if buf.len() < 6 || &buf[0..4] != b"XLB1" {
        return None;
    }
    let count = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let mut pos = 6;
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
        let len = u16::from_be_bytes([*buf.get(pos)?, *buf.get(pos + 1)?]) as usize;
        pos += 2;
        let bytes = buf.get(pos..pos + len)?;
        pos += len;
        names.push(String::from_utf8_lossy(bytes).into_owned());
    }
    let mut tbl = Vec::with_capacity(SLOTS);
    for _ in 0..SLOTS {
        let v = u16::from_be_bytes([*buf.get(pos)?, *buf.get(pos + 1)?]);
        pos += 2;
        tbl.push(v);
    }
    Some(LegacyBlocks { names, table: tbl })
}

/// Resolves a legacy packed block word to a modern blockstate name.
///
/// `packed` is the raw i32 read from the stream: `id = packed & 0xFFF`,
/// `meta = (packed >> 12) & 0xFFFFF` (`OldFormatSupport.getStateForId`).
/// Unknown ids fall back to `minecraft:air`, matching the mod. Unlike the mod
/// we fall back to that id's meta-0 entry before giving up, which is baked into
/// the table and strictly better for rendering.
pub fn block_name(packed: i32) -> &'static str {
    let id = (packed & 0xFFF) as usize;
    let meta = ((packed >> 12) & 0xFFFFF) as usize;
    // 1.12 metadata is four bits; a wider value (or a negative word, whose
    // sign bits land here) names no state the table ever had, and the mod
    // resolves it to air.
    if meta > 0xF {
        return "minecraft:air";
    }
    let t = table();
    let idx = (id << 4) | meta;
    match t.table.get(idx) {
        Some(&n) if n != NO_ENTRY => t
            .names
            .get(n as usize)
            .map(|s| s.as_str())
            .unwrap_or("minecraft:air"),
        _ => "minecraft:air",
    }
}

/// Resolves a legacy numeric biome id to a modern biome name.
///
/// From `OldFormatSupport`'s `biomesById` table with the Caves & Cliffs
/// renames (`fixBiome1718`) already folded in. Unknown ids fall back to
/// `minecraft:plains`, exactly as the mod does.
pub fn biome_name(id: i32) -> &'static str {
    match id {
        0 => "minecraft:ocean",
        1 => "minecraft:plains",
        2 => "minecraft:desert",
        3 => "minecraft:windswept_hills",
        4 => "minecraft:forest",
        5 => "minecraft:taiga",
        6 => "minecraft:swamp",
        7 => "minecraft:river",
        8 => "minecraft:nether_wastes",
        9 => "minecraft:the_end",
        10 => "minecraft:frozen_ocean",
        11 => "minecraft:frozen_river",
        12 => "minecraft:snowy_plains",
        13 => "minecraft:snowy_plains",
        14 => "minecraft:mushroom_fields",
        15 => "minecraft:mushroom_fields",
        16 => "minecraft:beach",
        17 => "minecraft:desert",
        18 => "minecraft:forest",
        19 => "minecraft:taiga",
        20 => "minecraft:windswept_hills",
        21 => "minecraft:jungle",
        22 => "minecraft:jungle",
        23 => "minecraft:sparse_jungle",
        24 => "minecraft:deep_ocean",
        25 => "minecraft:stony_shore",
        26 => "minecraft:snowy_beach",
        27 => "minecraft:birch_forest",
        28 => "minecraft:birch_forest",
        29 => "minecraft:dark_forest",
        30 => "minecraft:snowy_taiga",
        31 => "minecraft:snowy_taiga",
        32 => "minecraft:old_growth_pine_taiga",
        33 => "minecraft:old_growth_pine_taiga",
        34 => "minecraft:windswept_forest",
        35 => "minecraft:savanna",
        36 => "minecraft:savanna_plateau",
        37 => "minecraft:badlands",
        38 => "minecraft:wooded_badlands",
        39 => "minecraft:badlands",
        40 => "minecraft:small_end_islands",
        41 => "minecraft:end_midlands",
        42 => "minecraft:end_highlands",
        43 => "minecraft:end_barrens",
        44 => "minecraft:warm_ocean",
        45 => "minecraft:lukewarm_ocean",
        46 => "minecraft:cold_ocean",
        47 => "minecraft:warm_ocean",
        48 => "minecraft:deep_lukewarm_ocean",
        49 => "minecraft:deep_cold_ocean",
        50 => "minecraft:deep_frozen_ocean",
        127 => "minecraft:the_void",
        129 => "minecraft:sunflower_plains",
        130 => "minecraft:desert",
        131 => "minecraft:windswept_gravelly_hills",
        132 => "minecraft:flower_forest",
        133 => "minecraft:taiga",
        134 => "minecraft:swamp",
        140 => "minecraft:ice_spikes",
        149 => "minecraft:jungle",
        151 => "minecraft:sparse_jungle",
        155 => "minecraft:old_growth_birch_forest",
        156 => "minecraft:old_growth_birch_forest",
        157 => "minecraft:dark_forest",
        158 => "minecraft:snowy_taiga",
        160 => "minecraft:old_growth_spruce_taiga",
        161 => "minecraft:old_growth_spruce_taiga",
        162 => "minecraft:windswept_gravelly_hills",
        163 => "minecraft:windswept_savanna",
        164 => "minecraft:windswept_savanna",
        165 => "minecraft:eroded_badlands",
        166 => "minecraft:wooded_badlands",
        167 => "minecraft:badlands",
        168 => "minecraft:bamboo_jungle",
        169 => "minecraft:bamboo_jungle",
        170 => "minecraft:soul_sand_valley",
        171 => "minecraft:crimson_forest",
        172 => "minecraft:warped_forest",
        173 => "minecraft:basalt_deltas",
        174 => "minecraft:dripstone_caves",
        175 => "minecraft:lush_caves",
        177 => "minecraft:meadow",
        178 => "minecraft:grove",
        179 => "minecraft:snowy_slopes",
        180 => "minecraft:frozen_peaks",
        181 => "minecraft:jagged_peaks",
        182 => "minecraft:stony_peaks",
        _ => "minecraft:plains",
    }
}

/// Builds the minimal NBT compound `{Name: "<name>"}` in `NbtIo` framing, so a
/// legacy state can live in the region palette and be re-encoded as a modern
/// blockstate.
pub fn synth_state_nbt(name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len() + 16);
    out.push(0x0A); // TAG_Compound
    out.extend_from_slice(&0u16.to_be_bytes()); // empty root name
    out.push(0x08); // TAG_String
    out.extend_from_slice(&4u16.to_be_bytes());
    out.extend_from_slice(b"Name");
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0x00); // TAG_End
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_parses_and_resolves_known_ids() {
        assert_eq!(block_name(1), "minecraft:stone");
        assert_eq!(block_name(2), "minecraft:grass_block");
        assert_eq!(block_name(9), "minecraft:water");
        // id 0 = air, and an id the 1.12 table never had falls back to air.
        assert_eq!(block_name(0), "minecraft:air");
        assert_eq!(block_name(4095), "minecraft:air");
    }

    #[test]
    fn meta_is_the_high_nibble_and_falls_back_to_meta_zero() {
        // wool id 35, meta 14 = red wool in 1.12.
        assert_eq!(block_name(35 | (14 << 12)), "minecraft:red_wool");
        // An unrecorded meta resolves via the baked meta-0 fallback.
        assert!(block_name(1 | (13 << 12)).starts_with("minecraft:"));
        // Metadata wider than four bits (and a negative word) is no state at
        // all: air, never a neighbouring id's slot.
        assert_eq!(block_name(35 | (16 << 12)), "minecraft:air");
        assert_eq!(block_name(35 | (0x10E << 12)), "minecraft:air");
        assert_eq!(block_name(-1), "minecraft:air");
    }

    #[test]
    fn renamed_blocks_use_modern_names() {
        let names = &table().names;
        for dead in [
            "minecraft:grass_path",
            "minecraft:sign",
            "minecraft:wall_sign",
        ] {
            assert!(!names.iter().any(|n| n == dead), "{dead} should be renamed");
        }
    }

    #[test]
    fn biomes_cover_the_table_and_default_to_plains() {
        assert_eq!(biome_name(0), "minecraft:ocean");
        assert_eq!(biome_name(1), "minecraft:plains");
        // Caves & Cliffs rename applied, not the 1.12 name.
        assert_eq!(biome_name(3), "minecraft:windswept_hills");
        assert_eq!(biome_name(9999), "minecraft:plains");
    }

    #[test]
    fn synth_nbt_is_a_named_compound() {
        let raw = synth_state_nbt("minecraft:stone");
        assert_eq!(raw[0], 0x0A);
        assert_eq!(*raw.last().unwrap(), 0x00);
        let Ok(parsed) = crate::codec::nbt::read_named_nbt(&mut crate::codec::Rd::new(&raw)) else {
            panic!("synthesised NBT must parse");
        };
        assert_eq!(parsed.name.as_deref(), Some("minecraft:stone"));
    }
}
