//! Reader for the zvcr3d world-download format (release 1, version byte 7).
//!
//! Layout, per the format spec: a plain header (`zvcr3d`, version, dimension,
//! protocol) followed by one Zstd frame holding the whole region container —
//! two palette tables and 1024 optional segments, one per Minecraft chunk.
//!
//! Every block and biome section is a *reverse delta chain*: snapshot 0 is the
//! newest complete state and each later snapshot patches backwards in time.
//! We only ever want the newest, so older snapshots are parsed for their
//! length and discarded.
//!
//! Only the fields the importer needs are decoded; segment states and tile
//! entities are skipped (the map has no use for either).

use std::io::Read;

pub const MAGIC: &[u8; 6] = b"zvcr3d";
/// zvcr 1.0.0.0. Earlier version bytes are experimental formats the reference
/// implementation itself dropped support for.
pub const SUPPORTED_VERSION: u8 = 7;

pub const SEGMENTS_PER_REGION: usize = 1024;
pub const BLOCKS_PER_SECTION: usize = 16 * 16 * 16;
pub const BIOMES_PER_SECTION: usize = 4 * 4 * 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dim {
    Overworld,
    Nether,
    End,
}

impl Dim {
    pub fn from_byte(b: u8) -> Option<Dim> {
        match b {
            0 => Some(Dim::Overworld),
            1 => Some(Dim::Nether),
            2 => Some(Dim::End),
            _ => None,
        }
    }
    /// Chunk sections stacked in this dimension.
    pub fn sections(self) -> usize {
        match self {
            Dim::Overworld => 24,
            Dim::Nether | Dim::End => 16,
        }
    }
    /// Minecraft Y of the bottom of the world. zvcr Y levels are unsigned and
    /// start at 0, so `minecraft_y = zvcr_y + min_y`.
    pub fn min_y(self) -> i32 {
        match self {
            Dim::Overworld => -64,
            Dim::Nether | Dim::End => 0,
        }
    }
    pub fn height(self) -> i32 {
        (self.sections() * 16) as i32
    }
    /// The folder Xaero's World Map stores this dimension under.
    pub fn xaero_folder(self) -> &'static str {
        match self {
            Dim::Overworld => "null",
            Dim::Nether => "DIM-1",
            Dim::End => "DIM1",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Dim::Overworld => "overworld",
            Dim::Nether => "nether",
            Dim::End => "end",
        }
    }
    /// Whether the dimension has sky light — `false` for both the Nether and
    /// the End, which is why neither ever records a sky-light contribution.
    pub fn has_sky_light(self) -> bool {
        matches!(self, Dim::Overworld)
    }
}

#[derive(Debug)]
pub enum ZvcrError {
    NotZvcr,
    UnsupportedVersion(u8),
    UnknownDimension(u8),
    Zstd(String),
    Truncated(&'static str),
    Malformed(String),
}

impl std::fmt::Display for ZvcrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZvcrError::NotZvcr => write!(f, "not a zvcr3d file (bad magic)"),
            ZvcrError::UnsupportedVersion(v) => {
                write!(
                    f,
                    "zvcr version byte {v}, only {SUPPORTED_VERSION} (1.0.0.0) is supported"
                )
            }
            ZvcrError::UnknownDimension(d) => write!(f, "unknown dimension type {d}"),
            ZvcrError::Zstd(e) => write!(f, "zstd decompression failed: {e}"),
            ZvcrError::Truncated(what) => write!(f, "container truncated inside {what}"),
            ZvcrError::Malformed(m) => write!(f, "malformed container: {m}"),
        }
    }
}

impl std::error::Error for ZvcrError {}

#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub version: u8,
    pub dim: Dim,
    pub protocol: u16,
}

/// Reads the plain header and inflates the Zstd-compressed region container.
pub fn open(file: &[u8]) -> Result<(Header, Vec<u8>), ZvcrError> {
    if file.len() < 10 || &file[0..6] != MAGIC {
        return Err(ZvcrError::NotZvcr);
    }
    let version = file[6];
    if version != SUPPORTED_VERSION {
        return Err(ZvcrError::UnsupportedVersion(version));
    }
    let dim = Dim::from_byte(file[7]).ok_or(ZvcrError::UnknownDimension(file[7]))?;
    let protocol = u16::from_le_bytes([file[8], file[9]]);

    let mut decoder = ruzstd::decoding::StreamingDecoder::new(&file[10..])
        .map_err(|e| ZvcrError::Zstd(e.to_string()))?;
    let mut container = Vec::new();
    decoder
        .read_to_end(&mut container)
        .map_err(|e| ZvcrError::Zstd(e.to_string()))?;
    Ok((
        Header {
            version,
            dim,
            protocol,
        },
        container,
    ))
}

/// One decoded segment (Minecraft chunk) at its newest snapshot.
///
/// Buffers are owned by the reader and reused across segments, so a view is
/// only valid until the next one is produced.
pub struct SegmentView<'a> {
    /// `sections * 4096` global blockstate ids, section 0 lowest, index within
    /// a section `y * 256 + z * 16 + x`.
    pub blocks: &'a [u16],
    /// `sections * 64` biome ids, index within a section `y * 16 + z * 4 + x`.
    pub biomes: &'a [u16],
    pub sections: usize,
}

impl SegmentView<'_> {
    /// Blockstate id at a local column position; `y` is a zvcr (unsigned) level.
    #[inline]
    pub fn block(&self, x: usize, y: usize, z: usize) -> u16 {
        self.blocks[(y >> 4) * BLOCKS_PER_SECTION + (y & 15) * 256 + z * 16 + x]
    }
    /// Biome id covering a local column position (biomes are one per 4x4x4).
    #[inline]
    pub fn biome(&self, x: usize, y: usize, z: usize) -> u16 {
        self.biomes[(y >> 4) * BIOMES_PER_SECTION + ((y & 15) >> 2) * 16 + (z >> 2) * 4 + (x >> 2)]
    }
}

pub struct Reader<'a> {
    b: &'a [u8],
    i: usize,
    sections: usize,
    block_palettes: Vec<(usize, u16)>, // (offset into `b` of entries, length)
    biome_palettes: Vec<(usize, u16)>,
    blocks: Vec<u16>,
    biomes: Vec<u16>,
    newest_timestamp: u64,
}

impl<'a> Reader<'a> {
    pub fn new(container: &'a [u8], dim: Dim) -> Result<Reader<'a>, ZvcrError> {
        let sections = dim.sections();
        let mut r = Reader {
            b: container,
            i: 0,
            sections,
            block_palettes: Vec::new(),
            biome_palettes: Vec::new(),
            blocks: vec![0; sections * BLOCKS_PER_SECTION],
            biomes: vec![0; sections * BIOMES_PER_SECTION],
            newest_timestamp: 0,
        };
        r.block_palettes = r.palette_table()?;
        r.biome_palettes = r.palette_table()?;
        Ok(r)
    }

    /// Walks all 1024 segment slots in order, calling `f` for each one that is
    /// present with its slot index (`localX * 32 + localZ`).
    pub fn for_each_segment(
        &mut self,
        mut f: impl FnMut(usize, &SegmentView<'_>),
    ) -> Result<(), ZvcrError> {
        for slot in 0..SEGMENTS_PER_REGION {
            if self.u8()? == 0 {
                continue;
            }
            for s in 0..self.sections {
                let snap = self.newest_snapshot()?;
                let out = s * BLOCKS_PER_SECTION;
                self.unpack(snap, BLOCKS_PER_SECTION, true, out)?;
            }
            for s in 0..self.sections {
                let snap = self.newest_snapshot()?;
                let out = s * BIOMES_PER_SECTION;
                self.unpack(snap, BIOMES_PER_SECTION, false, out)?;
            }
            self.skip_segment_info()?;
            self.skip_tile_entities()?;
            let view = SegmentView {
                blocks: &self.blocks,
                biomes: &self.biomes,
                sections: self.sections,
            };
            f(slot, &view);
        }
        Ok(())
    }

    /// Newest snapshot timestamp seen anywhere in the region (Unix seconds),
    /// i.e. when this region was last observed on the server. Carrying it onto
    /// the output file's mtime is what lets an mtime-based merge weigh
    /// downloaded data against your own by when each was actually seen.
    pub fn newest_timestamp(&self) -> u64 {
        self.newest_timestamp
    }

    /// Bytes left unread after the last segment. A clean parse consumes the
    /// container exactly; anything else means our reading of the format is
    /// wrong for this file, which is worth surfacing rather than ignoring.
    pub fn remaining(&self) -> usize {
        self.b.len() - self.i
    }

    // ---------------------------------------------------------------- parse --

    fn palette_table(&mut self) -> Result<Vec<(usize, u16)>, ZvcrError> {
        let n = self.u32()? as usize;
        // Every entry costs at least its two length bytes, so a count larger
        // than that is a lie the reservation must not honour.
        let mut out = Vec::with_capacity(n.min(self.remaining() / 2));
        for _ in 0..n {
            let len = self.u16()?;
            let off = self.i;
            self.skip(len as usize * 2)?;
            out.push((off, len));
        }
        Ok(out)
    }

    /// Reads a packed delta chain, returning the newest snapshot and skipping
    /// the reverse deltas behind it.
    fn newest_snapshot(&mut self) -> Result<Snapshot, ZvcrError> {
        let n = self.u64()?;
        if n == 0 {
            return Err(ZvcrError::Malformed("empty delta chain".into()));
        }
        let mut newest = None;
        for _ in 0..n {
            let timestamp = self.u64()?;
            let snap = match self.u8()? {
                0 => Snapshot::Single(self.u16()?),
                _ => {
                    let longs = length_of(self.u64()?, "packed array")?;
                    let off = self.i;
                    self.skip(longs.checked_mul(8).ok_or_else(|| {
                        ZvcrError::Malformed(format!("packed array of {longs} longs"))
                    })?)?;
                    let palette = self.u32()?;
                    Snapshot::Packed {
                        off,
                        longs,
                        palette,
                    }
                }
            };
            if newest.is_none() {
                newest = Some(snap);
                self.newest_timestamp = self.newest_timestamp.max(timestamp);
            }
        }
        Ok(newest.expect("chain length checked above"))
    }

    fn skip_segment_info(&mut self) -> Result<(), ZvcrError> {
        let n = length_of(self.u64()?, "segment states")?;
        // Each state is a type byte plus a u64 timestamp.
        self.skip(
            n.checked_mul(9)
                .ok_or_else(|| ZvcrError::Malformed(format!("{n} segment states")))?,
        )
    }

    fn skip_tile_entities(&mut self) -> Result<(), ZvcrError> {
        let snapshots = self.u64()?;
        for _ in 0..snapshots {
            let _timestamp = self.u64()?;
            let count = self.u64()?;
            for _ in 0..count {
                let _packed_pos = self.u32()?;
                if self.u8()? != 0 {
                    let _type_id = self.u32()?;
                    let nbt_len = length_of(self.u64()?, "tile entity nbt")?;
                    self.skip(nbt_len)?;
                }
            }
        }
        Ok(())
    }

    /// Expands one snapshot into `self.blocks` / `self.biomes` at `out`.
    fn unpack(
        &mut self,
        snap: Snapshot,
        count: usize,
        into_blocks: bool,
        out: usize,
    ) -> Result<(), ZvcrError> {
        let dst = if into_blocks {
            &mut self.blocks
        } else {
            &mut self.biomes
        };
        match snap {
            Snapshot::Single(v) => {
                dst[out..out + count].fill(v);
                Ok(())
            }
            Snapshot::Packed {
                off,
                longs,
                palette,
            } => {
                // Direct mode stores raw values at a fixed 16 bits; otherwise
                // entries index a shared palette and the width follows its
                // length, rounded up to a nibble boundary.
                let (bits, table) = if palette == u32::MAX {
                    (16u32, None)
                } else {
                    let palettes = if into_blocks {
                        &self.block_palettes
                    } else {
                        &self.biome_palettes
                    };
                    let &(poff, plen) = palettes.get(palette as usize).ok_or_else(|| {
                        ZvcrError::Malformed(format!("palette index {palette} out of range"))
                    })?;
                    let bits = if plen <= 16 {
                        4
                    } else if plen <= 256 {
                        8
                    } else {
                        16
                    };
                    (bits, Some((poff, plen as usize)))
                };
                let per_long = 64 / bits as usize;
                let need = count.div_ceil(per_long);
                if longs < need {
                    return Err(ZvcrError::Malformed(format!(
                        "packed array holds {longs} longs, need {need}"
                    )));
                }
                let mask = if bits == 64 {
                    u64::MAX
                } else {
                    (1u64 << bits) - 1
                };
                let mut written = 0usize;
                for l in 0..need {
                    let base = off + l * 8;
                    let word = u64::from_le_bytes(
                        self.b[base..base + 8]
                            .try_into()
                            .map_err(|_| ZvcrError::Truncated("packed data"))?,
                    );
                    for k in 0..per_long {
                        if written == count {
                            break;
                        }
                        let raw = ((word >> (k * bits as usize)) & mask) as u16;
                        let value = match table {
                            None => raw,
                            Some((poff, plen)) => {
                                let idx = raw as usize;
                                if idx >= plen {
                                    return Err(ZvcrError::Malformed(format!(
                                        "palette entry {idx} beyond length {plen}"
                                    )));
                                }
                                let at = poff + idx * 2;
                                u16::from_le_bytes([self.b[at], self.b[at + 1]])
                            }
                        };
                        dst[out + written] = value;
                        written += 1;
                    }
                }
                Ok(())
            }
        }
    }

    // ------------------------------------------------------------ primitives --

    fn skip(&mut self, n: usize) -> Result<(), ZvcrError> {
        if self.b.len() - self.i < n {
            return Err(ZvcrError::Truncated("segment data"));
        }
        self.i += n;
        Ok(())
    }
    fn u8(&mut self) -> Result<u8, ZvcrError> {
        let v = *self
            .b
            .get(self.i)
            .ok_or(ZvcrError::Truncated("header byte"))?;
        self.i += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16, ZvcrError> {
        let i = self.i;
        self.skip(2)?;
        Ok(u16::from_le_bytes([self.b[i], self.b[i + 1]]))
    }
    fn u32(&mut self) -> Result<u32, ZvcrError> {
        let i = self.i;
        self.skip(4)?;
        Ok(u32::from_le_bytes(self.b[i..i + 4].try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, ZvcrError> {
        let i = self.i;
        self.skip(8)?;
        Ok(u64::from_le_bytes(self.b[i..i + 8].try_into().unwrap()))
    }
}

/// A length field from the file as a `usize`, or `Malformed` when it cannot be
/// one (a 64-bit count on a 32-bit host); the callers then bounds-check it.
fn length_of(v: u64, what: &str) -> Result<usize, ZvcrError> {
    usize::try_from(v).map_err(|_| ZvcrError::Malformed(format!("{what} length {v} is absurd")))
}

#[derive(Debug, Clone, Copy)]
enum Snapshot {
    Single(u16),
    Packed {
        off: usize,
        longs: usize,
        palette: u32,
    },
}

/// `r.<x>.<z>.zvcr3d` -> region coordinates.
pub fn parse_region_name(name: &str) -> Option<(i32, i32)> {
    let rest = name.strip_prefix("r.")?.strip_suffix(".zvcr3d")?;
    let (x, z) = rest.split_once('.')?;
    Some((x.parse().ok()?, z.parse().ok()?))
}

/// Sector directory a region lives in: `floorDiv(coord, 32)`.
pub fn sector_of(region_coord: i32) -> i32 {
    region_coord.div_euclid(32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_names_round_trip() {
        assert_eq!(parse_region_name("r.0.0.zvcr3d"), Some((0, 0)));
        assert_eq!(parse_region_name("r.-1.-12.zvcr3d"), Some((-1, -12)));
        assert_eq!(parse_region_name("r.9.28.zvcr3d"), Some((9, 28)));
        assert_eq!(parse_region_name("r.1.2.mca"), None);
        assert_eq!(parse_region_name("region.zvcr3d"), None);
    }

    /// A palette count that promises more entries than the container has
    /// bytes must fail as truncated, not reserve gigabytes for them.
    #[test]
    fn absurd_palette_count_is_an_error() {
        let container = [0xFF, 0xFF, 0xFF, 0xFF];
        assert!(matches!(
            Reader::new(&container, Dim::End),
            Err(ZvcrError::Truncated(_))
        ));
    }

    /// A packed array whose long count overflows when scaled to bytes must be
    /// reported as malformed instead of wrapping and slicing past the buffer.
    #[test]
    fn overflowing_packed_length_is_an_error() {
        let mut container = Vec::new();
        container.extend_from_slice(&0u32.to_le_bytes()); // block palettes
        container.extend_from_slice(&0u32.to_le_bytes()); // biome palettes
        container.push(1); // segment present
        container.extend_from_slice(&1u64.to_le_bytes()); // one snapshot
        container.extend_from_slice(&7u64.to_le_bytes()); // timestamp
        container.push(1); // packed
        container.extend_from_slice(&0x2000_0000_0000_0001u64.to_le_bytes());
        let mut r = Reader::new(&container, Dim::End).expect("palette tables parse");
        let err = r.for_each_segment(|_, _| {}).unwrap_err();
        assert!(matches!(err, ZvcrError::Malformed(_)), "{err}");
    }

    #[test]
    fn sectors_floor_toward_negative_infinity() {
        assert_eq!(sector_of(0), 0);
        assert_eq!(sector_of(31), 0);
        assert_eq!(sector_of(32), 1);
        assert_eq!(sector_of(-1), -1);
        assert_eq!(sector_of(-32), -1);
        assert_eq!(sector_of(-33), -2);
    }
}
