//! Parser and lookup for the baked XCT1 color table (see tools/xaero-colorgen).

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    None,
    Grass,
    Foliage,
    DryFoliage,
    Water,
}

#[derive(Debug, Clone, Copy)]
pub struct BlockColor {
    pub rgba: [u8; 4],
    pub tint: Tint,
    /// True when colorgen had no texture (gray placeholder) or the block is
    /// unknown to the table entirely.
    pub missing: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct BiomeColors {
    pub grass: [u8; 3],
    pub foliage: [u8; 3],
    pub water: [u8; 3],
    pub dry_foliage: [u8; 3],
}

const DEFAULT_BIOME: BiomeColors = BiomeColors {
    grass: [0x91, 0xBD, 0x59],
    foliage: [0x77, 0xAB, 0x2F],
    water: [0x3F, 0x76, 0xE4],
    dry_foliage: [0xB0, 0x84, 0x54],
};

pub struct ColorTable {
    pub mc_version: String,
    blocks: HashMap<String, BlockColor>,
    aliases: HashMap<String, String>,
    biomes: HashMap<String, BiomeColors>,
    fallback_biome: BiomeColors,
}

impl ColorTable {
    pub fn parse(bytes: &[u8]) -> Result<ColorTable, String> {
        let mut p = P { b: bytes, i: 0 };
        if p.take(4)? != b"XCT1" {
            return Err("bad magic".into());
        }
        let fmt = p.u16()?;
        if fmt != 1 {
            return Err(format!("unsupported XCT1 format {fmt}"));
        }
        let mc_version = p.str()?;
        let nblocks = p.u32()? as usize;
        let mut blocks = HashMap::with_capacity(nblocks);
        for _ in 0..nblocks {
            let name = p.str()?;
            let rgba = [p.u8()?, p.u8()?, p.u8()?, p.u8()?];
            let tint = match p.u8()? {
                1 => Tint::Grass,
                2 => Tint::Foliage,
                3 => Tint::DryFoliage,
                4 => Tint::Water,
                _ => Tint::None,
            };
            let missing = p.u8()? != 0;
            blocks.insert(
                name,
                BlockColor {
                    rgba,
                    tint,
                    missing,
                },
            );
        }
        let naliases = p.u32()? as usize;
        let mut aliases = HashMap::with_capacity(naliases);
        for _ in 0..naliases {
            let from = p.str()?;
            let to = p.str()?;
            aliases.insert(from, to);
        }
        let nbiomes = p.u32()? as usize;
        let mut biomes = HashMap::with_capacity(nbiomes);
        for _ in 0..nbiomes {
            let name = p.str()?;
            let g = [p.u8()?, p.u8()?, p.u8()?];
            let f = [p.u8()?, p.u8()?, p.u8()?];
            let w = [p.u8()?, p.u8()?, p.u8()?];
            let d = [p.u8()?, p.u8()?, p.u8()?];
            biomes.insert(
                name,
                BiomeColors {
                    grass: g,
                    foliage: f,
                    water: w,
                    dry_foliage: d,
                },
            );
        }
        let fallback_biome = biomes
            .get("minecraft:plains")
            .copied()
            .unwrap_or(DEFAULT_BIOME);
        Ok(ColorTable {
            mc_version,
            blocks,
            aliases,
            biomes,
            fallback_biome,
        })
    }

    /// Looks up a block by full id ("minecraft:stone"). Falls back through the
    /// alias table, then name heuristics, and finally a flagged gray.
    pub fn block(&self, name: &str) -> BlockColor {
        if let Some(c) = self.blocks.get(name) {
            return *c;
        }
        if let Some(target) = self.aliases.get(name)
            && let Some(c) = self.blocks.get(target)
        {
            return *c;
        }
        // Renames older than the baked alias table's window; old archives
        // still carry these ids.
        if let Some(target) = legacy_rename(name)
            && let Some(c) = self.blocks.get(target)
        {
            return *c;
        }
        self.heuristic(name)
    }

    fn heuristic(&self, name: &str) -> BlockColor {
        let short = name.strip_prefix("minecraft:").unwrap_or(name);
        // Derived shapes reuse their base material's color.
        for suffix in [
            "_stairs",
            "_slab",
            "_wall",
            "_fence_gate",
            "_fence",
            "_button",
            "_pressure_plate",
            "_trapdoor",
            "_door",
        ] {
            if let Some(base) = short.strip_suffix(suffix) {
                for candidate in [
                    format!("minecraft:{base}"),
                    format!("minecraft:{base}_planks"),
                    format!("minecraft:{base}_block"),
                ] {
                    if let Some(c) = self.blocks.get(&candidate) {
                        return *c;
                    }
                }
            }
        }
        // Category colors for entirely unknown (likely modded/renamed) blocks.
        let by_category = if short.ends_with("_leaves") {
            self.blocks.get("minecraft:oak_leaves").copied()
        } else if short.ends_with("_log") || short.ends_with("_wood") || short.ends_with("_planks")
        {
            self.blocks.get("minecraft:oak_planks").copied()
        } else if short.ends_with("_ore") || short.ends_with("stone") {
            self.blocks.get("minecraft:stone").copied()
        } else if short.contains("water") {
            self.blocks.get("minecraft:water").copied()
        } else {
            None
        };
        match by_category {
            Some(mut c) => {
                c.missing = true;
                c
            }
            None => BlockColor {
                rgba: [0x7F, 0x7F, 0x7F, 0xFF],
                tint: Tint::None,
                missing: true,
            },
        }
    }

    pub fn biome(&self, name: &str) -> BiomeColors {
        self.biomes
            .get(name)
            .copied()
            .unwrap_or(self.fallback_biome)
    }

    pub fn fallback_biome(&self) -> BiomeColors {
        self.fallback_biome
    }
}

fn legacy_rename(name: &str) -> Option<&'static str> {
    Some(match name {
        "minecraft:grass_path" => "minecraft:dirt_path",
        _ => return None,
    })
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.b.len() - self.i < n {
            return Err("truncated color table".into());
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn str(&mut self) -> Result<String, String> {
        let n = self.u16()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|e| e.to_string())
    }
}
