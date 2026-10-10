//! Just enough Java NBT to walk a saved chunk.
//!
//! A chunk is mostly things the map has no use for — light arrays, block
//! entities, tick lists, heightmaps — so nothing is materialized into a tree:
//! the cursor walks the compound, hands out borrowed slices for the fields the
//! importer wants, and skips the rest by length. Keys come back as raw bytes so
//! the hot path never pays for a UTF-8 check on a name it is about to discard.

pub const TAG_END: u8 = 0;
pub const TAG_BYTE: u8 = 1;
pub const TAG_SHORT: u8 = 2;
pub const TAG_INT: u8 = 3;
pub const TAG_LONG: u8 = 4;
pub const TAG_FLOAT: u8 = 5;
pub const TAG_DOUBLE: u8 = 6;
pub const TAG_BYTE_ARRAY: u8 = 7;
pub const TAG_STRING: u8 = 8;
pub const TAG_LIST: u8 = 9;
pub const TAG_COMPOUND: u8 = 10;
pub const TAG_INT_ARRAY: u8 = 11;
pub const TAG_LONG_ARRAY: u8 = 12;

/// Compound/list nesting the skimmer accepts. Chunk NBT nests a handful of
/// levels; anything near this bound is corrupt, and the bound is what keeps a
/// corrupt file from recursing into a stack overflow.
const MAX_DEPTH: u32 = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NbtError {
    Eof,
    BadTag(u8),
    BadLength(i32),
    TooDeep,
    NotUtf8,
    NotACompound(u8),
}

impl std::fmt::Display for NbtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NbtError::Eof => write!(f, "NBT ended mid-tag"),
            NbtError::BadTag(t) => write!(f, "unknown NBT tag {t}"),
            NbtError::BadLength(n) => write!(f, "negative NBT length {n}"),
            NbtError::TooDeep => write!(f, "NBT nested deeper than {MAX_DEPTH}"),
            NbtError::NotUtf8 => write!(f, "NBT string is not UTF-8"),
            NbtError::NotACompound(t) => write!(f, "root tag is {t}, not a compound"),
        }
    }
}

impl std::error::Error for NbtError {}

pub struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(b: &'a [u8]) -> Cursor<'a> {
        Cursor { b, i: 0 }
    }

    /// Consumes the named root tag's header, leaving the cursor on the first
    /// entry of the root compound.
    pub fn open_root(&mut self) -> Result<(), NbtError> {
        let tag = self.u8()?;
        if tag != TAG_COMPOUND {
            return Err(NbtError::NotACompound(tag));
        }
        let n = self.u16()? as usize;
        self.take(n)?;
        Ok(())
    }

    #[inline]
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], NbtError> {
        let end = self.i.checked_add(n).ok_or(NbtError::Eof)?;
        if end > self.b.len() {
            return Err(NbtError::Eof);
        }
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }

    #[inline]
    pub fn u8(&mut self) -> Result<u8, NbtError> {
        Ok(self.take(1)?[0])
    }
    #[inline]
    pub fn i8(&mut self) -> Result<i8, NbtError> {
        Ok(self.take(1)?[0] as i8)
    }
    #[inline]
    pub fn u16(&mut self) -> Result<u16, NbtError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    #[inline]
    pub fn i32(&mut self) -> Result<i32, NbtError> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    /// A non-negative array/list length, as every NBT length is required to be.
    #[inline]
    pub fn array_len(&mut self) -> Result<usize, NbtError> {
        let n = self.i32()?;
        if n < 0 {
            return Err(NbtError::BadLength(n));
        }
        Ok(n as usize)
    }

    /// Raw bytes of a TAG_String payload. Block, property, biome and key names
    /// are ASCII, where Java's modified UTF-8 and UTF-8 coincide.
    #[inline]
    pub fn raw_str(&mut self) -> Result<&'a [u8], NbtError> {
        let n = self.u16()? as usize;
        self.take(n)
    }

    #[inline]
    pub fn str(&mut self) -> Result<&'a str, NbtError> {
        std::str::from_utf8(self.raw_str()?).map_err(|_| NbtError::NotUtf8)
    }

    /// Payload of a TAG_Long_Array, still as big-endian bytes: the packed
    /// block/biome storage is read out of it directly, never copied.
    #[inline]
    pub fn long_array(&mut self) -> Result<&'a [u8], NbtError> {
        let n = self.array_len()?;
        let bytes = n.checked_mul(8).ok_or(NbtError::Eof)?;
        self.take(bytes)
    }

    /// Skips one payload of the given tag type.
    pub fn skip(&mut self, tag: u8) -> Result<(), NbtError> {
        self.skip_at(tag, 0)
    }

    fn skip_at(&mut self, tag: u8, depth: u32) -> Result<(), NbtError> {
        if depth > MAX_DEPTH {
            return Err(NbtError::TooDeep);
        }
        match tag {
            TAG_END => {}
            TAG_BYTE => {
                self.take(1)?;
            }
            TAG_SHORT => {
                self.take(2)?;
            }
            TAG_INT | TAG_FLOAT => {
                self.take(4)?;
            }
            TAG_LONG | TAG_DOUBLE => {
                self.take(8)?;
            }
            TAG_BYTE_ARRAY => {
                let n = self.array_len()?;
                self.take(n)?;
            }
            TAG_STRING => {
                self.raw_str()?;
            }
            TAG_LIST => {
                let elem = self.u8()?;
                let n = self.array_len()?;
                if n > 0 && elem == TAG_END {
                    return Err(NbtError::BadTag(elem));
                }
                for _ in 0..n {
                    self.skip_at(elem, depth + 1)?;
                }
            }
            TAG_COMPOUND => loop {
                let t = self.u8()?;
                if t == TAG_END {
                    break;
                }
                let n = self.u16()? as usize;
                self.take(n)?;
                self.skip_at(t, depth + 1)?;
            },
            TAG_INT_ARRAY => {
                let n = self.array_len()?;
                let bytes = n.checked_mul(4).ok_or(NbtError::Eof)?;
                self.take(bytes)?;
            }
            TAG_LONG_ARRAY => {
                let n = self.array_len()?;
                let bytes = n.checked_mul(8).ok_or(NbtError::Eof)?;
                self.take(bytes)?;
            }
            other => return Err(NbtError::BadTag(other)),
        }
        Ok(())
    }
}

/// One packed entry out of a `BitStorage` long array, which since 1.16 never
/// lets an entry straddle two longs.
#[inline]
pub fn unpack(longs: &[u8], bits: u32, i: usize) -> u32 {
    let per_long = 64 / bits as usize;
    let long_idx = i / per_long;
    let shift = (i % per_long) as u32 * bits;
    let off = long_idx * 8;
    let word = u64::from_be_bytes([
        longs[off],
        longs[off + 1],
        longs[off + 2],
        longs[off + 3],
        longs[off + 4],
        longs[off + 5],
        longs[off + 6],
        longs[off + 7],
    ]);
    ((word >> shift) & ((1u64 << bits) - 1)) as u32
}

/// Longs a `BitStorage` of `entries` values at `bits` each occupies.
#[inline]
pub fn packed_longs(entries: usize, bits: u32) -> usize {
    let per_long = 64 / bits as usize;
    entries.div_ceil(per_long)
}

/// `Mth.ceillog2` — the bit width Minecraft picks for a palette of this size.
#[inline]
pub fn ceil_log2(n: usize) -> u32 {
    if n <= 1 {
        return 0;
    }
    usize::BITS - (n - 1).leading_zeros()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceil_log2_matches_minecraft() {
        assert_eq!(ceil_log2(0), 0);
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(4), 2);
        assert_eq!(ceil_log2(5), 3);
        assert_eq!(ceil_log2(256), 8);
        assert_eq!(ceil_log2(257), 9);
    }

    #[test]
    fn packed_long_counts_match_the_real_format() {
        // Verified against real 1.21.1 chunks: a 33-entry palette is 6 bits
        // and 410 longs for a 4096-block section.
        assert_eq!(packed_longs(4096, 6), 410);
        assert_eq!(packed_longs(4096, 4), 256);
        assert_eq!(packed_longs(4096, 5), 342);
        assert_eq!(packed_longs(4096, 8), 512);
        assert_eq!(packed_longs(64, 1), 1);
        assert_eq!(packed_longs(64, 3), 4);
    }

    #[test]
    fn entries_never_straddle_a_long() {
        // Two 5-bit entries, values 1 and 31, packed low bits first.
        let word: u64 = 1 | (31 << 5);
        let bytes = word.to_be_bytes();
        assert_eq!(unpack(&bytes, 5, 0), 1);
        assert_eq!(unpack(&bytes, 5, 1), 31);
    }

    #[test]
    fn skip_walks_a_nested_compound() {
        // { a: 1b, b: [I; 2], c: { d: "x" } }
        let mut v = vec![TAG_COMPOUND, 0, 0];
        v.extend([TAG_BYTE, 0, 1, b'a', 7]);
        v.extend([TAG_INT_ARRAY, 0, 1, b'b']);
        v.extend(2i32.to_be_bytes());
        v.extend(0i32.to_be_bytes());
        v.extend(0i32.to_be_bytes());
        v.extend([
            TAG_COMPOUND,
            0,
            1,
            b'c',
            TAG_STRING,
            0,
            1,
            b'd',
            0,
            1,
            b'x',
            TAG_END,
        ]);
        v.push(TAG_END);
        let mut c = Cursor::new(&v);
        c.open_root().unwrap();
        let mut keys = Vec::new();
        loop {
            let t = c.u8().unwrap();
            if t == TAG_END {
                break;
            }
            let n = c.u16().unwrap() as usize;
            keys.push(c.take(n).unwrap().to_vec());
            c.skip(t).unwrap();
        }
        assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        assert_eq!(c.i, v.len());
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let v = vec![TAG_COMPOUND, 0, 0, TAG_INT_ARRAY, 0, 1, b'b', 0, 0, 0, 9];
        let mut c = Cursor::new(&v);
        c.open_root().unwrap();
        assert_eq!(c.u8().unwrap(), TAG_INT_ARRAY);
        let n = c.u16().unwrap() as usize;
        c.take(n).unwrap();
        assert_eq!(c.skip(TAG_INT_ARRAY), Err(NbtError::Eof));
    }
}
