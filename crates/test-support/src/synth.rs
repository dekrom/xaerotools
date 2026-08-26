use xaero_core::{encode_region, DecodedRegion};

/// Re-encode a decoded region to the canonical 7.8 stream.
pub fn reencode(region: &DecodedRegion) -> Vec<u8> {
    encode_region(region)
}

/// First `keep` bytes of `bytes`, an error-injection or seed input.
pub fn truncate_prefix(bytes: &[u8], keep: usize) -> Vec<u8> {
    bytes[..keep.min(bytes.len())].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The committed legacy fixture the core crate already ships.
    const FIXTURE: &[u8] =
        include_bytes!("../../xaero-core/tests/fixtures/legacy/v0.7_-10000_-1.zip");

    #[test]
    fn reencode_of_a_decoded_region_decodes_again() {
        let stream = xaero_core::read_region_container(FIXTURE).expect("unzip");
        let region = xaero_core::decode_region(&stream).expect("decode");
        let bytes = reencode(&region);
        assert!(
            xaero_core::decode_region(&bytes).is_ok(),
            "re-encoded region must decode"
        );
    }

    #[test]
    fn truncate_prefix_never_panics_on_decode() {
        let stream = xaero_core::read_region_container(FIXTURE).expect("unzip");
        for keep in [0usize, 1, 8, stream.len() / 2] {
            let t = truncate_prefix(&stream, keep);
            let _ = xaero_core::decode_region(&t); // must not panic (Ok or Err both fine)
        }
    }
}
