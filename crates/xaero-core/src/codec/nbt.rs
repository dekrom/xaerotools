//! Minimal NBT "skim" support for the state palette.
//!
//! Block states are stored via Java's `NbtIo.write(CompoundTag, DataOutput)`:
//! a named root tag — `0x0A`, u16 name length (always 0 here), then compound
//! payload until TAG_End. We never need to materialize a tree: we locate the
//! end of the tag (so the raw bytes can be captured verbatim for re-emission)
//! and extract the root `Name` string for rendering.
//!
//! Save major 8 (Minecraft 26.3) spells the two root keys `id`/`properties`
//! instead; [`read_state_nbt`] renames them back on read.

use super::{Eof, Rd};

/// Max value of the recursion-depth counter before the skimmer treats the
/// input as corrupt. The counter increments on every recursive call, and
/// compound nesting recurses twice per level (`skim_compound` ->
/// `skim_payload` -> `skim_compound`), so the real bound on compound nesting
/// is ~256 levels (~512 for lists, which recurse once per level). Real
/// Minecraft block-state NBT nests only a few levels; either bound is far
/// above legitimate depth and far below a stack-overflow (~10k).
const MAX_NBT_DEPTH: u32 = 512;

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

pub struct RawNbt {
    /// Bytes of the named root tag: exactly as they appeared in the stream,
    /// except that a major-8 state has its keys respelled by [`read_state_nbt`].
    pub raw: Vec<u8>,
    /// Value of the root compound's "Name" string entry, if present.
    pub name: Option<String>,
}

/// Reads one block-state palette entry written under save `major`, returned in
/// the `Name`/`Properties` spelling of majors 1..=7.
///
/// Minecraft 26.3 renamed the root keys of a block state's NBT to
/// `id`/`properties`, and Xaero's World Map marks regions written that way as
/// major 8. On load it renames older entries forward
/// (`OldFormatSupport.fixBlockState262_263`); this applies the exact inverse,
/// so one palette spelling serves every major, the 7.8 writer's output stays
/// loadable by every build that reads 7.8, and the 26.3 build's own fix turns it
/// back into what it wrote.
pub(crate) fn read_state_nbt(rd: &mut Rd<'_>, major: u16) -> Result<RawNbt, Eof> {
    if major < 8 {
        return read_named_nbt(rd);
    }
    let start = rd.pos;
    if rd.u8()? != TAG_COMPOUND {
        // Not a block state at all: keep it verbatim, as the older majors do.
        rd.pos = start;
        return read_named_nbt(rd);
    }
    let name_len = rd.u16()? as usize;
    rd.take(name_len)?;
    let mut raw = rd.buf[start..rd.pos].to_vec();
    let mut name: Option<String> = None;
    loop {
        let entry = rd.pos;
        let tag = rd.u8()?;
        if tag == TAG_END {
            raw.push(TAG_END);
            break;
        }
        let key_len = rd.u16()? as usize;
        let key = rd.take(key_len)?;
        let payload = rd.pos;
        skim_payload(rd, tag, 1)?;
        let respelled: &[u8] = match (tag, key) {
            (TAG_STRING, b"id") => {
                if name.is_none() {
                    name = Some(decode_java_utf(&rd.buf[payload + 2..rd.pos]));
                }
                b"Name"
            }
            (TAG_COMPOUND, b"properties") => b"Properties",
            // The 26.3 reader never consults the old keys, so a stray one is
            // dropped rather than left to shadow the respelled entry.
            (TAG_STRING, b"Name") | (TAG_COMPOUND, b"Properties") => continue,
            _ => {
                raw.extend_from_slice(&rd.buf[entry..rd.pos]);
                continue;
            }
        };
        raw.push(tag);
        raw.extend_from_slice(&(respelled.len() as u16).to_be_bytes());
        raw.extend_from_slice(respelled);
        raw.extend_from_slice(&rd.buf[payload..rd.pos]);
    }
    Ok(RawNbt { raw, name })
}

/// Reads one named NBT root tag from the cursor. Returns Err(Eof) on
/// truncation; malformed-but-complete NBT surfaces as Eof too (the caller
/// treats it as a truncated region — same recovery path).
pub(crate) fn read_named_nbt(rd: &mut Rd<'_>) -> Result<RawNbt, Eof> {
    let start = rd.pos;
    let tag = rd.u8()?;
    let mut name: Option<String> = None;
    if tag != TAG_END {
        let name_len = rd.u16()? as usize;
        rd.take(name_len)?;
        if tag == TAG_COMPOUND {
            name = skim_compound(rd, true, 0)?;
        } else {
            skim_payload(rd, tag, 0)?;
        }
    }
    Ok(RawNbt {
        raw: rd.buf[start..rd.pos].to_vec(),
        name,
    })
}

/// Skims a compound payload; when `want_name`, returns the value of the
/// first top-level "Name" string entry found.
fn skim_compound(rd: &mut Rd<'_>, want_name: bool, depth: u32) -> Result<Option<String>, Eof> {
    if depth > MAX_NBT_DEPTH {
        return Err(Eof);
    }
    let mut found: Option<String> = None;
    loop {
        let tag = rd.u8()?;
        if tag == TAG_END {
            return Ok(found);
        }
        let name_len = rd.u16()? as usize;
        let name_bytes = rd.take(name_len)?;
        if want_name && found.is_none() && tag == TAG_STRING && name_bytes == b"Name" {
            let len = rd.u16()? as usize;
            let bytes = rd.take(len)?;
            found = Some(decode_java_utf(bytes));
        } else {
            skim_payload(rd, tag, depth + 1)?;
        }
    }
}

fn skim_payload(rd: &mut Rd<'_>, tag: u8, depth: u32) -> Result<(), Eof> {
    if depth > MAX_NBT_DEPTH {
        return Err(Eof);
    }
    match tag {
        TAG_BYTE => {
            rd.take(1)?;
        }
        TAG_SHORT => {
            rd.take(2)?;
        }
        TAG_INT | TAG_FLOAT => {
            rd.take(4)?;
        }
        TAG_LONG | TAG_DOUBLE => {
            rd.take(8)?;
        }
        TAG_BYTE_ARRAY => {
            let n = rd.i32()?;
            rd.take(usize::try_from(n).map_err(|_| Eof)?)?;
        }
        TAG_STRING => {
            let n = rd.u16()? as usize;
            rd.take(n)?;
        }
        TAG_LIST => {
            let elem = rd.u8()?;
            let n = rd.i32()?;
            let n = usize::try_from(n).unwrap_or(0);
            for _ in 0..n {
                skim_payload(rd, elem, depth + 1)?;
            }
        }
        TAG_COMPOUND => {
            skim_compound(rd, false, depth + 1)?;
        }
        TAG_INT_ARRAY => {
            let n = rd.i32()?;
            rd.take(
                usize::try_from(n)
                    .map_err(|_| Eof)?
                    .checked_mul(4)
                    .ok_or(Eof)?,
            )?;
        }
        TAG_LONG_ARRAY => {
            let n = rd.i32()?;
            rd.take(
                usize::try_from(n)
                    .map_err(|_| Eof)?
                    .checked_mul(8)
                    .ok_or(Eof)?,
            )?;
        }
        _ => return Err(Eof), // unknown tag type: treat as corruption
    }
    Ok(())
}

/// Decodes Java "modified UTF-8" (as produced by DataOutput.writeUTF /
/// NBT strings): 1-3 byte sequences, embedded nulls as C0 80, supplementary
/// characters as CESU-8 surrogate pairs. Lossy on malformed input.
pub fn decode_java_utf(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    let mut pending_high: Option<u16> = None;
    while i < bytes.len() {
        let b = bytes[i];
        let unit: u16 = if b & 0x80 == 0 {
            i += 1;
            b as u16
        } else if b & 0xE0 == 0xC0 && i + 1 < bytes.len() {
            let u = (((b & 0x1F) as u16) << 6) | (bytes[i + 1] & 0x3F) as u16;
            i += 2;
            u
        } else if b & 0xF0 == 0xE0 && i + 2 < bytes.len() {
            let u = (((b & 0x0F) as u16) << 12)
                | (((bytes[i + 1] & 0x3F) as u16) << 6)
                | (bytes[i + 2] & 0x3F) as u16;
            i += 3;
            u
        } else {
            i += 1;
            out.push(char::REPLACEMENT_CHARACTER);
            pending_high = None;
            continue;
        };
        // Reassemble CESU-8 surrogate pairs into real chars.
        if let Some(h) = pending_high.take() {
            if (0xDC00..=0xDFFF).contains(&unit) {
                let c = 0x10000 + (((h - 0xD800) as u32) << 10) + (unit - 0xDC00) as u32;
                out.push(char::from_u32(c).unwrap_or(char::REPLACEMENT_CHARACTER));
                continue;
            }
            out.push(char::REPLACEMENT_CHARACTER);
        }
        if (0xD800..=0xDBFF).contains(&unit) {
            pending_high = Some(unit);
        } else if (0xDC00..=0xDFFF).contains(&unit) {
            out.push(char::REPLACEMENT_CHARACTER);
        } else {
            out.push(char::from_u32(unit as u32).unwrap_or(char::REPLACEMENT_CHARACTER));
        }
    }
    if pending_high.is_some() {
        out.push(char::REPLACEMENT_CHARACTER);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skims_blockstate_compound() {
        // {Name:"minecraft:netherrack"} as NbtIo writes it
        let mut raw = vec![0x0A, 0x00, 0x00]; // compound, empty root name
        raw.extend_from_slice(&[0x08, 0x00, 0x04]);
        raw.extend_from_slice(b"Name");
        raw.extend_from_slice(&[0x00, 0x14]);
        raw.extend_from_slice(b"minecraft:netherrack");
        raw.push(0x00); // TAG_End
        raw.extend_from_slice(&[0xAA, 0xBB]); // trailing junk must not be consumed

        let mut rd = Rd::new(&raw);
        let nbt = read_named_nbt(&mut rd).ok().unwrap();
        assert_eq!(nbt.name.as_deref(), Some("minecraft:netherrack"));
        assert_eq!(nbt.raw.len(), raw.len() - 2);
        assert_eq!(rd.remaining(), 2);
    }

    #[test]
    fn skims_properties() {
        // {Name:"a:b", Properties:{axis:"y"}}
        let mut raw = vec![0x0A, 0x00, 0x00];
        raw.extend_from_slice(&[0x08, 0x00, 0x04]);
        raw.extend_from_slice(b"Name");
        raw.extend_from_slice(&[0x00, 0x03]);
        raw.extend_from_slice(b"a:b");
        raw.extend_from_slice(&[0x0A, 0x00, 0x0A]);
        raw.extend_from_slice(b"Properties");
        raw.extend_from_slice(&[0x08, 0x00, 0x04]);
        raw.extend_from_slice(b"axis");
        raw.extend_from_slice(&[0x00, 0x01]);
        raw.push(b'y');
        raw.push(0x00);
        raw.push(0x00);
        let mut rd = Rd::new(&raw);
        let nbt = read_named_nbt(&mut rd).ok().unwrap();
        assert_eq!(nbt.name.as_deref(), Some("a:b"));
        assert_eq!(rd.remaining(), 0);
    }

    /// Named root compound (empty root name) of `(tag, key, payload)` entries.
    fn compound(entries: &[(u8, &str, Vec<u8>)]) -> Vec<u8> {
        let mut raw = vec![TAG_COMPOUND, 0x00, 0x00];
        raw.extend_from_slice(&compound_payload(entries));
        raw
    }

    fn compound_payload(entries: &[(u8, &str, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (tag, key, payload) in entries {
            out.push(*tag);
            out.extend_from_slice(&(key.len() as u16).to_be_bytes());
            out.extend_from_slice(key.as_bytes());
            out.extend_from_slice(payload);
        }
        out.push(TAG_END);
        out
    }

    fn string_payload(s: &str) -> Vec<u8> {
        let mut out = (s.len() as u16).to_be_bytes().to_vec();
        out.extend_from_slice(s.as_bytes());
        out
    }

    #[test]
    fn major8_state_keys_are_respelled_in_place() {
        let props = compound_payload(&[(TAG_STRING, "axis", string_payload("y"))]);
        let mut raw = compound(&[
            (TAG_COMPOUND, "properties", props.clone()),
            (TAG_STRING, "id", string_payload("minecraft:oak_log")),
        ]);
        raw.extend_from_slice(&[0xAA, 0xBB]); // trailing junk must not be consumed
        let mut rd = Rd::new(&raw);
        let nbt = read_state_nbt(&mut rd, 8).ok().unwrap();
        assert_eq!(nbt.name.as_deref(), Some("minecraft:oak_log"));
        assert_eq!(
            nbt.raw,
            compound(&[
                (TAG_COMPOUND, "Properties", props),
                (TAG_STRING, "Name", string_payload("minecraft:oak_log")),
            ]),
            "keys respelled, entry order and payloads untouched"
        );
        assert_eq!(rd.remaining(), 2);
    }

    #[test]
    fn major8_drops_old_keys_the_game_would_ignore() {
        let raw = compound(&[
            (TAG_STRING, "Name", string_payload("minecraft:dirt")),
            (TAG_STRING, "id", string_payload("minecraft:stone")),
            (TAG_BYTE, "extra", vec![1]),
        ]);
        let nbt = read_state_nbt(&mut Rd::new(&raw), 8).ok().unwrap();
        assert_eq!(nbt.name.as_deref(), Some("minecraft:stone"));
        assert_eq!(
            nbt.raw,
            compound(&[
                (TAG_STRING, "Name", string_payload("minecraft:stone")),
                (TAG_BYTE, "extra", vec![1]),
            ])
        );
    }

    #[test]
    fn older_majors_keep_state_bytes_verbatim() {
        let raw = compound(&[(TAG_STRING, "id", string_payload("minecraft:stone"))]);
        let nbt = read_state_nbt(&mut Rd::new(&raw), 7).ok().unwrap();
        assert_eq!(nbt.name, None);
        assert_eq!(nbt.raw, raw);
    }

    #[test]
    fn truncated_major8_state_is_eof() {
        let raw = compound(&[(TAG_STRING, "id", string_payload("minecraft:stone"))]);
        for keep in 0..raw.len() {
            assert!(read_state_nbt(&mut Rd::new(&raw[..keep]), 8).is_err());
        }
    }

    #[test]
    fn java_utf_null_and_pair() {
        assert_eq!(decode_java_utf(&[0xC0, 0x80]), "\0");
        // U+1F6A7 (🚧) in CESU-8: surrogate pair D83D DEA7
        assert_eq!(
            decode_java_utf(&[0xED, 0xA0, 0xBD, 0xED, 0xBA, 0xA7]),
            "\u{1F6A7}"
        );
        assert_eq!(decode_java_utf(b"minecraft:plains"), "minecraft:plains");
    }

    #[test]
    fn truncated_nbt_is_eof() {
        let raw = [0x0A, 0x00, 0x00, 0x08, 0x00, 0x04, b'N'];
        let mut rd = Rd::new(&raw);
        assert!(read_named_nbt(&mut rd).is_err());
    }

    // Nested empty compounds; read_named_nbt takes the first as the root tag.
    fn nested_compounds(depth: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        for _ in 0..depth {
            bytes.extend_from_slice(&[0x0A, 0x00, 0x00]); // TAG_Compound, name_len=0
        }
        bytes.resize(bytes.len() + depth, 0x00); // depth trailing TAG_End bytes
        bytes
    }

    #[test]
    fn shallow_nbt_still_parses() {
        let raw = nested_compounds(10);
        let mut rd = Rd::new(&raw);
        assert!(read_named_nbt(&mut rd).is_ok());
    }

    #[test]
    fn deep_nbt_is_capped_not_overflowed() {
        // 600 > MAX_NBT_DEPTH(512): well-formed, would parse Ok without a cap;
        // with the cap it must return Eof (bounded, never reaches an overflow).
        let raw = nested_compounds(600);
        let mut rd = Rd::new(&raw);
        assert!(read_named_nbt(&mut rd).is_err());
    }

    /// Lists are the cheap way in: one nested level costs five bytes, so a
    /// hostile region can bury a hundred thousand of them in half a megabyte.
    /// They recurse through a different arm than compounds and must be capped
    /// there too.
    #[test]
    fn deep_lists_are_capped_not_overflowed() {
        let mut raw = vec![0x0A, 0x00, 0x00, 0x09, 0x00, 0x01, b'l'];
        for _ in 0..100_000 {
            raw.extend_from_slice(&[0x09, 0x00, 0x00, 0x00, 0x01]); // TAG_List of one
        }
        raw.extend_from_slice(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x00]);
        let mut rd = Rd::new(&raw);
        assert!(read_named_nbt(&mut rd).is_err());
    }
}
