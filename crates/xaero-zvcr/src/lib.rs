//! Imports zvcr3d world downloads into Xaero's World Map region format.
//!
//! zvcr3d is the archival format the 2b2t 1M world download ships in: whole
//! regions Zstd-compressed in one frame, blocks and biomes stored as global
//! registry ids in reverse-delta snapshot chains. It carries blocks, biomes and
//! block entities — no light, no heightmaps, no entities.
//!
//! An Xaero region covers the same 512x512 blocks as a zvcr region and a zvcr
//! segment is one Minecraft chunk, which is one Xaero map tile, so conversion
//! is a straight 1:1 walk with no resampling. The work is in reproducing what
//! the game would have written for each block column, which `column` does by
//! porting `MapWriter.loadPixel` against a table of real block behaviour
//! extracted from the Minecraft jar.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use xaero_zvcr::{blockprops::BlockProps, region, zvcr};
//!
//! let props = BlockProps::parse(xaero_zvcr::BLOCKPROPS)?;
//! let bytes = std::fs::read("r.0.0.zvcr3d")?;
//! let (header, container) = zvcr::open(&bytes)?;
//! let opts = region::opts_for(header.dim, true);
//! let converted = region::convert(&container, header.dim, &props, opts)?;
//! let stream = xaero_core::encode_region(&converted.region);
//! std::fs::write("0_0.zip", xaero_core::write_region_container(&stream)?)?;
//! # Ok(())
//! # }
//! ```

pub mod biomes;
pub mod blockprops;
pub mod column;
pub mod region;
pub mod zvcr;

/// The baked block-behaviour table; see `tools/xaero-blockprops`.
pub static BLOCKPROPS: &[u8] = include_bytes!("../../../assets/blockprops.bin");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baked_table_loads_and_matches_the_download_registry() {
        let props = blockprops::BlockProps::parse(BLOCKPROPS).expect("parse blockprops.bin");
        assert_eq!(props.mc_version(), "1.21.4");
        // 1.21.4 has exactly this many block states; the world download's own
        // registry agrees, which is what makes the shared id space safe.
        assert_eq!(props.len(), 27866);
        assert_eq!(props.name(props.air()), "minecraft:air");
        assert_eq!(props.block_name(0), "minecraft:air");
    }

    #[test]
    fn state_nbt_is_what_xaeros_palette_expects() {
        let props = blockprops::BlockProps::parse(BLOCKPROPS).unwrap();
        let id = (0..props.len() as u32)
            .find(|&i| props.name(i) == "minecraft:netherrack")
            .expect("netherrack");
        // Named root compound, empty name, one String entry "Name", TAG_End.
        let nbt = props.nbt(id);
        assert_eq!(nbt[0], 10);
        assert_eq!(&nbt[1..3], &[0, 0]);
        assert_eq!(*nbt.last().unwrap(), 0);
        let text = String::from_utf8_lossy(nbt);
        assert!(text.contains("Name"));
        assert!(text.contains("minecraft:netherrack"));
        assert!(!text.contains("Properties"));

        let lit = (0..props.len() as u32)
            .find(|&i| props.name(i) == "minecraft:redstone_wall_torch[facing=north,lit=true]")
            .expect("redstone wall torch");
        let text = String::from_utf8_lossy(props.nbt(lit));
        assert!(text.contains("Properties"));
        assert!(text.contains("facing"));
        assert!(text.contains("north"));
    }
}
