//! Reader for Minecraft's region container (`r.<x>.<z>.mca`).
//!
//! 8 KiB of header — 1024 chunk locations then 1024 chunk timestamps — followed
//! by the chunks themselves, each a length, a compression byte and one
//! compressed NBT document. Nothing here decodes NBT; it hands out the
//! decompressed bytes and the timestamp the save recorded, which is the only
//! honest observation time for the region the importer writes.

use std::io::Read;

use flate2::{Decompress, FlushDecompress, Status};

pub const CHUNKS_PER_REGION: usize = 1024;
const SECTOR: usize = 4096;
const HEADER: usize = 2 * SECTOR;
/// A chunk that inflates past this is corrupt, not large: the biggest real
/// 1.21 chunk is a few hundred KiB.
const MAX_CHUNK_NBT: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub enum McaError {
    TooShort(usize),
    BadSector { index: usize, offset: usize },
    BadLength { index: usize, len: u32 },
    External(usize),
    UnknownCompression(u8),
    Inflate(String),
    Oversized(usize),
}

impl std::fmt::Display for McaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McaError::TooShort(n) => write!(f, "region file is {n} bytes, shorter than its header"),
            McaError::BadSector { index, offset } => {
                write!(f, "chunk {index} points past the file (sector {offset})")
            }
            McaError::BadLength { index, len } => write!(f, "chunk {index} declares {len} bytes"),
            McaError::External(index) => write!(
                f,
                "chunk {index} is stored in an external .mcc file, which this download has none of"
            ),
            McaError::UnknownCompression(c) => write!(f, "unknown chunk compression {c}"),
            McaError::Inflate(e) => write!(f, "chunk decompression failed: {e}"),
            McaError::Oversized(n) => write!(f, "chunk inflates past {n} bytes"),
        }
    }
}

impl std::error::Error for McaError {}

pub struct RegionFile<'a> {
    b: &'a [u8],
}

impl<'a> RegionFile<'a> {
    pub fn new(b: &'a [u8]) -> Result<RegionFile<'a>, McaError> {
        if b.len() < HEADER {
            return Err(McaError::TooShort(b.len()));
        }
        Ok(RegionFile { b })
    }

    /// Chunk slot for chunk coordinates local to the region.
    #[inline]
    pub fn slot(cx: usize, cz: usize) -> usize {
        (cz & 31) * 32 + (cx & 31)
    }

    /// When the save last wrote this chunk, in Unix seconds; 0 when absent.
    #[inline]
    pub fn timestamp(&self, index: usize) -> u32 {
        let o = SECTOR + index * 4;
        u32::from_be_bytes([self.b[o], self.b[o + 1], self.b[o + 2], self.b[o + 3]])
    }

    /// Raw payload of one chunk: its compression byte and compressed bytes, or
    /// `None` when the slot is empty.
    pub fn raw(&self, index: usize) -> Result<Option<(u8, &'a [u8])>, McaError> {
        let o = index * 4;
        let loc = u32::from_be_bytes([self.b[o], self.b[o + 1], self.b[o + 2], self.b[o + 3]]);
        if loc == 0 {
            return Ok(None);
        }
        let offset = (loc >> 8) as usize;
        let sectors = (loc & 0xFF) as usize;
        let start = offset * SECTOR;
        // The declared length is trusted only as far as the sectors reserved
        // for the chunk and the file itself: a bad header must not read
        // neighbouring chunks' bytes as this one's.
        let limit = start
            .checked_add(sectors * SECTOR)
            .filter(|&e| e <= self.b.len() && start >= HEADER)
            .ok_or(McaError::BadSector { index, offset })?;
        if start + 5 > limit {
            return Err(McaError::BadSector { index, offset });
        }
        let len = u32::from_be_bytes([
            self.b[start],
            self.b[start + 1],
            self.b[start + 2],
            self.b[start + 3],
        ]);
        let len = len as usize;
        if len == 0 || start + 4 + len > limit {
            return Err(McaError::BadLength {
                index,
                len: len as u32,
            });
        }
        let scheme = self.b[start + 4];
        if scheme & 0x80 != 0 {
            return Err(McaError::External(index));
        }
        Ok(Some((scheme, &self.b[start + 5..start + 4 + len])))
    }
}

/// Reusable inflate state. One per worker: rebuilding the 32 KiB window for
/// each of tens of millions of chunks would cost more than the inflating.
pub struct Inflater {
    zlib: Decompress,
}

impl Default for Inflater {
    fn default() -> Self {
        Self::new()
    }
}

impl Inflater {
    pub fn new() -> Inflater {
        Inflater {
            zlib: Decompress::new(true),
        }
    }

    /// Decompresses one chunk payload into `out`, which is cleared first.
    pub fn chunk(&mut self, scheme: u8, src: &[u8], out: &mut Vec<u8>) -> Result<(), McaError> {
        out.clear();
        match scheme {
            1 => {
                // gzip: never written by the modern game, still accepted by it.
                let mut d = flate2::read::GzDecoder::new(src).take(MAX_CHUNK_NBT as u64 + 1);
                d.read_to_end(out)
                    .map_err(|e| McaError::Inflate(e.to_string()))?;
                if out.len() > MAX_CHUNK_NBT {
                    return Err(McaError::Oversized(MAX_CHUNK_NBT));
                }
                Ok(())
            }
            2 => self.zlib(src, out),
            3 => {
                if src.len() > MAX_CHUNK_NBT {
                    return Err(McaError::Oversized(MAX_CHUNK_NBT));
                }
                out.extend_from_slice(src);
                Ok(())
            }
            other => Err(McaError::UnknownCompression(other)),
        }
    }

    fn zlib(&mut self, src: &[u8], out: &mut Vec<u8>) -> Result<(), McaError> {
        self.zlib.reset(true);
        if out.capacity() < 256 * 1024 {
            out.reserve(256 * 1024);
        }
        let mut input = src;
        loop {
            if out.len() == out.capacity() {
                if out.capacity() >= MAX_CHUNK_NBT {
                    return Err(McaError::Oversized(MAX_CHUNK_NBT));
                }
                out.reserve(out.capacity());
            }
            let before_in = self.zlib.total_in();
            let before_out = self.zlib.total_out();
            let status = self
                .zlib
                .decompress_vec(input, out, FlushDecompress::None)
                .map_err(|e| McaError::Inflate(e.to_string()))?;
            let used = (self.zlib.total_in() - before_in) as usize;
            input = &input[used..];
            if status == Status::StreamEnd {
                return Ok(());
            }
            // No input consumed and nothing produced means the stream cannot
            // advance: either it is truncated or the output is full, and the
            // top of the loop is what grows the output.
            if used == 0 && self.zlib.total_out() == before_out && out.len() < out.capacity() {
                return Err(McaError::Inflate("stream ended mid-chunk".into()));
            }
        }
    }
}

/// Region coordinates out of an `r.<x>.<z>.mca` file name.
pub fn parse_region_name(name: &str) -> Option<(i32, i32)> {
    let rest = name.strip_prefix("r.")?.strip_suffix(".mca")?;
    let (x, z) = rest.split_once('.')?;
    Some((x.parse().ok()?, z.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_names_round_trip() {
        assert_eq!(parse_region_name("r.0.0.mca"), Some((0, 0)));
        assert_eq!(parse_region_name("r.-100.99.mca"), Some((-100, 99)));
        assert_eq!(parse_region_name("r.1.2.zvcr3d"), None);
        assert_eq!(parse_region_name("r.1.mca"), None);
        assert_eq!(parse_region_name("r.a.b.mca"), None);
    }

    #[test]
    fn an_empty_header_reads_as_no_chunks() {
        let b = vec![0u8; HEADER];
        let r = RegionFile::new(&b).unwrap();
        assert!(r.raw(0).unwrap().is_none());
        assert_eq!(r.timestamp(0), 0);
    }

    #[test]
    fn a_short_file_is_an_error_not_a_panic() {
        assert!(matches!(
            RegionFile::new(&[0u8; 10]),
            Err(McaError::TooShort(10))
        ));
    }

    #[test]
    fn a_location_past_the_file_is_rejected() {
        let mut b = vec![0u8; HEADER];
        b[0..4].copy_from_slice(&(((9999u32) << 8) | 1).to_be_bytes());
        let r = RegionFile::new(&b).unwrap();
        assert!(matches!(r.raw(0), Err(McaError::BadSector { .. })));
    }

    #[test]
    fn zlib_round_trips_through_the_reused_inflater() {
        use flate2::{Compression, write::ZlibEncoder};
        use std::io::Write;
        let mut inf = Inflater::new();
        let mut out = Vec::new();
        for n in [1usize, 1000, 300_000] {
            let data: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
            let mut e = ZlibEncoder::new(Vec::new(), Compression::fast());
            e.write_all(&data).unwrap();
            let packed = e.finish().unwrap();
            inf.chunk(2, &packed, &mut out).unwrap();
            assert_eq!(out, data, "n={n}");
        }
    }

    #[test]
    fn a_truncated_stream_is_an_error() {
        use flate2::{Compression, write::ZlibEncoder};
        use std::io::Write;
        let mut e = ZlibEncoder::new(Vec::new(), Compression::fast());
        e.write_all(&[7u8; 100_000]).unwrap();
        let packed = e.finish().unwrap();
        let mut inf = Inflater::new();
        let mut out = Vec::new();
        assert!(inf.chunk(2, &packed[..packed.len() / 2], &mut out).is_err());
    }
}
