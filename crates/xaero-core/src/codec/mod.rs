//! Region stream codec: decode (every version the game reads: majors 0..=7,
//! minors ..=8, plus pre-versioned saves) and encode (7.8).

pub mod legacy;
pub mod nbt;
mod reader;
mod writer;
mod zipio;

pub use reader::decode_region;
pub use writer::encode_region;
pub use zipio::{read_region_container, write_region_container};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatVersion {
    pub major: u16,
    pub minor: u16,
}

impl FormatVersion {
    pub fn packed(self) -> i32 {
        ((self.major as i32) << 16) | self.minor as i32
    }
}

impl std::fmt::Display for FormatVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("empty stream")]
    Empty,
    #[error("stream truncated inside the header")]
    TruncatedHeader,
    #[error("unsupported region save version {version} (XaeroTools reads up to 7.8{hint})")]
    UnsupportedVersion {
        version: FormatVersion,
        /// True when the file is *newer* than we support (update us, not the file).
        newer: bool,
        hint: &'static str,
    },
    #[error("zip container error: {0}")]
    Zip(String),
    #[error("invalid NBT in state palette: {0}")]
    Nbt(String),
}

impl CodecError {
    pub(crate) fn unsupported(version: FormatVersion) -> Self {
        let newer = version.major > 7 || (version.major == 7 && version.minor > 8);
        CodecError::UnsupportedVersion {
            version,
            newer,
            hint: if newer {
                "; update XaeroTools to read this file"
            } else {
                ""
            },
        }
    }
}

/// Cursor over the raw stream. All multi-byte reads are big-endian
/// (Java DataOutputStream convention).
pub(crate) struct Rd<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

/// Unexpected end of stream marker (not an error type: truncation is a
/// tolerated condition surfaced via `DecodedRegion::truncated`).
pub(crate) struct Eof;

impl<'a> Rd<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Rd { buf, pos: 0 }
    }
    #[inline]
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    #[inline]
    pub fn u8(&mut self) -> Result<u8, Eof> {
        let b = *self.buf.get(self.pos).ok_or(Eof)?;
        self.pos += 1;
        Ok(b)
    }
    #[inline]
    pub fn u16(&mut self) -> Result<u16, Eof> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    #[inline]
    pub fn i32(&mut self) -> Result<i32, Eof> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    #[inline]
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Eof> {
        if self.remaining() < n {
            return Err(Eof);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
}

/// Big-endian write helpers for the encoder.
pub(crate) trait WrExt {
    fn w_u8(&mut self, v: u8);
    fn w_u16(&mut self, v: u16);
    fn w_i32(&mut self, v: i32);
}

impl WrExt for Vec<u8> {
    #[inline]
    fn w_u8(&mut self, v: u8) {
        self.push(v);
    }
    #[inline]
    fn w_u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    #[inline]
    fn w_i32(&mut self, v: i32) {
        self.extend_from_slice(&v.to_be_bytes());
    }
}
