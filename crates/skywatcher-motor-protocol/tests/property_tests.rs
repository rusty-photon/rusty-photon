//! Property-based round-trip invariants for the codec.
//!
//! Each test below picks a property that must hold for every input the wire
//! protocol can carry, then exercises it against random inputs from
//! [`proptest`]. Property tests catch the kind of off-by-one and boundary
//! mistakes the inline unit tests in `src/codec.rs` won't — for example, a
//! decoder that's silently lossy at the high end of the encoder range only
//! shows up here.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    clippy::used_underscore_binding,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::float_cmp,
    clippy::similar_names,
    clippy::struct_excessive_bools
)]

use proptest::prelude::*;
use skywatcher_motor_protocol::codec::{
    decode_position, decode_u24, encode_position, encode_u24, POSITION_MAX, POSITION_MIN,
};

proptest! {
    /// Any 24-bit unsigned value round-trips losslessly through encode→decode.
    #[test]
    fn u24_roundtrip(value in 0u32..=0x00FF_FFFF) {
        let bytes = encode_u24(value);
        prop_assert_eq!(decode_u24(&bytes).unwrap(), value);
    }

    /// `encode_u24` always produces six bytes that are valid uppercase ASCII
    /// hex digits.
    #[test]
    fn u24_encode_is_uppercase_hex(value in 0u32..=0x00FF_FFFF) {
        let bytes = encode_u24(value);
        for b in &bytes {
            prop_assert!(
                matches!(b, b'0'..=b'9' | b'A'..=b'F'),
                "non-hex byte 0x{b:02X} in encoding of 0x{value:06X}"
            );
        }
    }

    /// Any signed encoder-tick value in the representable range round-trips
    /// through encode_position → decode_position.
    #[test]
    fn position_roundtrip(ticks in POSITION_MIN..=POSITION_MAX) {
        let bytes = encode_position(ticks).unwrap();
        prop_assert_eq!(decode_position(&bytes).unwrap(), ticks);
    }

    /// `encode_position` rejects out-of-range inputs symmetrically.
    #[test]
    fn position_rejects_out_of_range(
        ticks in prop_oneof![
            (POSITION_MAX + 1)..=i32::MAX,
            i32::MIN..=(POSITION_MIN - 1),
        ],
    ) {
        prop_assert!(encode_position(ticks).is_err());
    }
}
