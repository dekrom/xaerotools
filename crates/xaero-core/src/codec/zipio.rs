//! Region container handling.
//!
//! Modern files are a ZIP with exactly one entry named `region.xaero`
//! (deflate). Legacy bare `.xaero` files are the same stream uncompressed.
//! We sniff by magic so callers can pass either.

use std::io::{Cursor, Read, Write};

use super::CodecError;

/// Ceiling on a region stream, inflated or bare. The largest real region
/// seen is under 10 MB; the zip header's declared size and the deflate
/// stream itself are both untrusted (regions arrive over the network), so
/// neither is allowed to size an allocation past this.
pub const MAX_STREAM: u64 = 64 << 20;

/// Extracts the raw region stream from container bytes.
/// ZIP (PK..) -> inflated `region.xaero` entry (falls back to the first
/// entry if the canonical name is missing); anything else -> passed through
/// as a bare stream. Anything over [`MAX_STREAM`] is refused.
pub fn read_region_container(bytes: &[u8]) -> Result<Vec<u8>, CodecError> {
    if bytes.len() >= 4 && &bytes[0..2] == b"PK" {
        let mut archive =
            zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| CodecError::Zip(e.to_string()))?;
        let index = (0..archive.len())
            .find(|&i| {
                archive
                    .by_index_raw(i)
                    .map(|f| f.name() == "region.xaero")
                    .unwrap_or(false)
            })
            .unwrap_or(0);
        if archive.is_empty() {
            return Err(CodecError::Zip("empty zip container".into()));
        }
        let entry = archive
            .by_index(index)
            .map_err(|e| CodecError::Zip(e.to_string()))?;
        if entry.size() > MAX_STREAM {
            return Err(CodecError::TooLarge {
                bytes: entry.size(),
                limit: MAX_STREAM,
            });
        }
        let mut out = Vec::with_capacity(entry.size() as usize);
        // The declared size is only a hint: read one byte past the limit so
        // a stream that lies about its length is caught rather than trusted.
        entry
            .take(MAX_STREAM + 1)
            .read_to_end(&mut out)
            .map_err(|e| CodecError::Zip(e.to_string()))?;
        if out.len() as u64 > MAX_STREAM {
            return Err(CodecError::TooLarge {
                bytes: out.len() as u64,
                limit: MAX_STREAM,
            });
        }
        Ok(out)
    } else if bytes.len() as u64 > MAX_STREAM {
        Err(CodecError::TooLarge {
            bytes: bytes.len() as u64,
            limit: MAX_STREAM,
        })
    } else {
        Ok(bytes.to_vec())
    }
}

/// Wraps a region stream into the standard container: a ZIP with one
/// deflated `region.xaero` entry, matching what the mod writes.
pub fn write_region_container(stream: &[u8]) -> Result<Vec<u8>, CodecError> {
    let mut cursor = Cursor::new(Vec::with_capacity(stream.len() / 4 + 256));
    {
        let mut zw = zip::ZipWriter::new(&mut cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("region.xaero", opts)
            .map_err(|e| CodecError::Zip(e.to_string()))?;
        zw.write_all(stream)
            .map_err(|e| CodecError::Zip(e.to_string()))?;
        zw.finish().map_err(|e| CodecError::Zip(e.to_string()))?;
    }
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_round_trip() {
        let stream = vec![0xFFu8, 0x00, 0x07, 0x00, 0x08, 0x23, 0xFF, 0xFF, 0xFF, 0xFF];
        let zipped = write_region_container(&stream).unwrap();
        assert_eq!(&zipped[0..2], b"PK");
        assert_eq!(read_region_container(&zipped).unwrap(), stream);
        // bare stream passthrough
        assert_eq!(read_region_container(&stream).unwrap(), stream);
    }

    /// A zip whose entry inflates past the ceiling is refused before the
    /// inflated bytes can be allocated, whatever the header claims.
    #[test]
    fn oversized_stream_is_refused() {
        let zipped = write_region_container(&vec![0u8; (MAX_STREAM + 1) as usize]).unwrap();
        assert!(matches!(
            read_region_container(&zipped),
            Err(CodecError::TooLarge { .. })
        ));
        let bare = vec![0x23u8; (MAX_STREAM + 1) as usize];
        assert!(matches!(
            read_region_container(&bare),
            Err(CodecError::TooLarge { .. })
        ));
    }
}
