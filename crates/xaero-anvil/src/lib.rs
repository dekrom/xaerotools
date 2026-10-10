//! Imports Minecraft's own Anvil saves (`region/r.<x>.<z>.mca`) into Xaero's
//! World Map region format.
//!
//! This is the sibling of `xaero-zvcr`: same destination, same column
//! algorithm, different source container. A WorldTools-style download ships a
//! plain Minecraft world, so what this crate adds over that one is the Anvil
//! container, the chunk NBT, and the palette resolution — Anvil names its
//! blockstates where zvcr numbers them, so every palette entry is matched
//! against the baked table by name rather than by registry id, which is also
//! what lets a 1.21.1 download be read against a 1.21.4 table.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use xaero_anvil::{region, states::StateIndex};
//! use xaero_zvcr::{blockprops::BlockProps, zvcr::Dim};
//!
//! let props = BlockProps::parse(xaero_zvcr::BLOCKPROPS)?;
//! let states = StateIndex::build(&props)?;
//! let mut scratch = region::Scratch::new(Dim::Overworld);
//! let bytes = std::fs::read("r.0.0.mca")?;
//! let opts = xaero_zvcr::region::opts_for(Dim::Overworld, true);
//! let converted = region::convert(&bytes, Dim::Overworld, &props, &states, opts, &mut scratch)?;
//! let stream = xaero_core::encode_region(&converted.region);
//! std::fs::write("0_0.zip", xaero_core::write_region_container(&stream)?)?;
//! # Ok(())
//! # }
//! ```

pub mod chunk;
pub mod mca;
pub mod nbt;
pub mod region;
pub mod states;
