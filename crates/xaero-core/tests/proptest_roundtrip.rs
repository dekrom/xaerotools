use proptest::prelude::*;
use xaero_core::{decode_region, encode_region};

proptest! {
    #[test]
    fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let _ = decode_region(&bytes);
    }

    #[test]
    fn encode_is_a_fixed_point(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        if let Ok(region) = decode_region(&bytes) {
            let once = encode_region(&region);
            let twice = encode_region(&decode_region(&once).expect("re-decode canonical"));
            prop_assert_eq!(once, twice);
        }
    }
}
