//! xaero-core — parser, encoder, renderer and merge logic for Xaero's World Map
//! region data (modern format: save majors 6 and 7, minor <= 8).
//!
//! This crate is kept free of filesystem, async and SQLite dependencies so it
//! can also be compiled to WebAssembly. All I/O happens on byte slices; the
//! caller (CLI/server/wasm shell) owns files and networking.

pub mod codec;
pub mod dimconfig;
pub mod merge;
pub mod model;
pub mod naming;
pub mod render;
pub mod waypoints;

pub use codec::{CodecError, FormatVersion};
pub use codec::{decode_region, encode_region, read_region_container, write_region_container};
pub use model::{BiomeRef, DecodedRegion, Overlay, Palettes, Pixel, Region, Tile, TileChunk};

/// Current write version: everything is re-encoded as major 7, minor 8.
pub const WRITE_MAJOR: u16 = 7;
pub const WRITE_MINOR: u16 = 8;
