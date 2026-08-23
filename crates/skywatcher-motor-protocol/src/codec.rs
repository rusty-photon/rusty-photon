//! Wire-format codec helpers.
//!
//! The protocol's one quirky feature is its 24-bit hex encoding: low byte
//! first, with each byte's nibbles in normal high-then-low order. So the
//! 24-bit value `0x123456` serialises as the ASCII string `"563412"`.
//!
//! Axis positions ride on top of that with a `+0x800000` bias so the unsigned
//! hex on the wire can carry both signed-positive and signed-negative encoder
//! counts. [`encode_position`] / [`decode_position`] handle the bias so service
//! code sees signed [`i32`] ticks only.

use crate::error::{ProtocolError, Result};

/// Position bias added on encode, subtracted on decode.
///
/// The mount carries axis positions on the wire as unsigned 24-bit values
/// offset by this constant so that encoder count `0` is wire value
/// `0x800000`, count `+1` is `0x800001`, count `-1` is `0x7FFFFF`, and so on.
pub const POSITION_BIAS: u32 = 0x0080_0000;

/// Inclusive lower bound of the signed-24-bit encoder-tick range.
pub const POSITION_MIN: i32 = -(1 << 23);
/// Inclusive upper bound of the signed-24-bit encoder-tick range.
pub const POSITION_MAX: i32 = (1 << 23) - 1;

/// Upper-case ASCII hex digit for the low nibble of `n`. The high nibble
/// is masked off, so the mapping is total. The saturating arithmetic is
/// the total spelling of offsets that cannot overflow under the mask.
pub(crate) const fn nibble_to_hex(n: u8) -> u8 {
    let lo = n & 0x0F;
    if lo < 10 {
        b'0'.saturating_add(lo)
    } else {
        b'A'.saturating_add(lo.saturating_sub(10))
    }
}

/// Encode a `u8` as two ASCII hex bytes (high nibble first, upper-case).
#[must_use]
pub const fn encode_u8(value: u8) -> [u8; 2] {
    [nibble_to_hex(value >> 4), nibble_to_hex(value & 0x0F)]
}

/// Decode two ASCII hex bytes into a `u8`. Case-insensitive on input.
///
/// # Errors
///
/// Returns [`ProtocolError::HexError`] if either byte is not an ASCII hex
/// digit.
pub fn decode_u8(bytes: [u8; 2]) -> Result<u8> {
    let hi = decode_nibble(bytes[0])?;
    let lo = decode_nibble(bytes[1])?;
    Ok((hi << 4) | lo)
}

/// Decode one ASCII hex digit. Each arm's range makes its subtraction
/// total; the saturating spelling states that without a panic path.
pub(crate) fn decode_nibble(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b.saturating_sub(b'0')),
        b'a'..=b'f' => Ok(b.saturating_sub(b'a').saturating_add(10)),
        b'A'..=b'F' => Ok(b.saturating_sub(b'A').saturating_add(10)),
        other => Err(ProtocolError::HexError(format!(
            "expected ASCII hex digit, got {other:#04x}"
        ))),
    }
}

/// Encode a 24-bit unsigned value into six ASCII hex bytes, low byte first.
///
/// Only the low 24 bits of `value` are used. For example, `0x12_3456` encodes
/// as `[b'5', b'6', b'3', b'4', b'1', b'2']`.
#[must_use]
pub const fn encode_u24(value: u32) -> [u8; 6] {
    let [lo, mid, hi, _] = value.to_le_bytes();
    let [l0, l1] = encode_u8(lo);
    let [m0, m1] = encode_u8(mid);
    let [h0, h1] = encode_u8(hi);
    [l0, l1, m0, m1, h0, h1]
}

/// Decode six ASCII hex bytes (low byte first) into a 24-bit unsigned value.
///
/// The returned `u32` always has its high byte zeroed.
///
/// # Errors
///
/// Returns [`ProtocolError::HexError`] if any of the six bytes is not an
/// ASCII hex digit.
pub fn decode_u24(bytes: &[u8; 6]) -> Result<u32> {
    let lo = decode_u8([bytes[0], bytes[1]])?;
    let mid = decode_u8([bytes[2], bytes[3]])?;
    let hi = decode_u8([bytes[4], bytes[5]])?;
    Ok(u32::from(lo) | (u32::from(mid) << 8) | (u32::from(hi) << 16))
}

/// Encode a signed encoder-tick value as a 24-bit wire value with the
/// `+0x800000` bias applied.
///
/// # Errors
///
/// Returns [`ProtocolError::HexError`] when `ticks` is outside the
/// representable signed-24-bit range `-2^23 ..= 2^23 - 1`.
pub fn encode_position(ticks: i32) -> Result<[u8; 6]> {
    if !(POSITION_MIN..=POSITION_MAX).contains(&ticks) {
        return Err(ProtocolError::HexError(format!(
            "encoder ticks {ticks} out of signed-24-bit range [{POSITION_MIN}, {POSITION_MAX}]"
        )));
    }
    let biased = (ticks.wrapping_add(POSITION_BIAS.cast_signed())).cast_unsigned() & 0x00FF_FFFF;
    Ok(encode_u24(biased))
}

/// Decode six ASCII hex bytes into signed encoder ticks.
///
/// The wire value is interpreted as a 24-bit unsigned integer, the
/// `0x800000` bias is subtracted, and the result is returned as a signed
/// [`i32`] in the range `-2^23 ..= 2^23 - 1`.
///
/// # Errors
///
/// Returns [`ProtocolError::HexError`] if any of the six bytes is not an
/// ASCII hex digit ([`decode_u24`]'s check); the debias itself cannot fail.
pub fn decode_position(bytes: &[u8; 6]) -> Result<i32> {
    let biased = decode_u24(bytes)?;
    // `decode_u24` zeroes the high byte, so the debias cannot overflow;
    // wrapping is bit-exact and mirrors the encode side's `wrapping_add`.
    Ok(biased
        .cast_signed()
        .wrapping_sub(POSITION_BIAS.cast_signed()))
}

/// Verify that `frame` matches the strict outbound (command) UDP framing
/// rules: starts with `:`, has at least one byte after, ends with a single
/// `\r`, and contains no junk before/after.
///
/// Used by send-side UDP code paths to sanity-check encoded frames before
/// they go on the wire. Serial transports skip this — they stream
/// continuously and re-sync on the next `:`.
///
/// # Errors
///
/// Returns [`ProtocolError::FrameError`] if `frame` is shorter than three
/// bytes, does not start with `:`, does not end with `\r`, or carries an
/// embedded `\r`.
pub fn validate_command_frame(frame: &[u8]) -> Result<()> {
    // The short arms are the length check: the main arm only sees frames
    // of at least prefix + one byte + terminator.
    let (first, body, last) = match frame {
        [] | [_] | [_, _] => {
            return Err(ProtocolError::FrameError(format!(
                "command frame too short ({} bytes)",
                frame.len()
            )))
        }
        [first, body @ .., last] => (*first, body, *last),
    };
    if first != b':' {
        return Err(ProtocolError::FrameError(format!(
            "command frame must start with `:`, got {first:#04x}"
        )));
    }
    if last != b'\r' {
        return Err(ProtocolError::FrameError(
            "command frame must end with `\\r`".to_string(),
        ));
    }
    // Reject embedded `\r` — the controller treats a second `\r` as the end
    // of frame, so any payload containing one would split into two frames.
    // The prefix is already known not to be one, so checking the body
    // covers everything before the terminator.
    if body.contains(&b'\r') {
        return Err(ProtocolError::FrameError(
            "command frame contains embedded `\\r`".to_string(),
        ));
    }
    Ok(())
}

/// Verify that `frame` matches the strict inbound (response) UDP framing
/// rules: starts with `=` (success) or `!` (error), ends with a single
/// `\r`, no junk before/after.
///
/// Used by the UDP transport on receive. Serial transports buffer up to
/// the first `\r` and pass exactly the resulting slice in here.
///
/// # Errors
///
/// Returns [`ProtocolError::FrameError`] if `frame` is shorter than two
/// bytes, does not start with `=` or `!`, does not end with `\r`, carries
/// an embedded `\r`, or is a `!` reply that is neither 3 nor 4 bytes long.
pub fn validate_response_frame(frame: &[u8]) -> Result<()> {
    split_response_frame(frame).map(|_| ())
}

/// Validate `frame` against the response framing rules and hand back the
/// prefix byte and the payload between it and the trailing `\r`, so
/// decoders never re-derive what validation already proved.
pub(crate) fn split_response_frame(frame: &[u8]) -> Result<(u8, &[u8])> {
    // The short arms are the length check: the main arm only sees frames
    // of at least prefix + terminator.
    let (prefix, payload, last) = match frame {
        [] | [_] => {
            return Err(ProtocolError::FrameError(format!(
                "response frame too short ({} bytes)",
                frame.len()
            )))
        }
        [prefix, payload @ .., last] => (*prefix, payload, *last),
    };
    if prefix != b'=' && prefix != b'!' {
        return Err(ProtocolError::FrameError(format!(
            "response frame must start with `=` or `!`, got {prefix:#04x}"
        )));
    }
    if last != b'\r' {
        return Err(ProtocolError::FrameError(
            "response frame must end with `\\r`".to_string(),
        ));
    }
    // The prefix is already known not to be a `\r`, so checking the
    // payload covers everything before the terminator.
    if payload.contains(&b'\r') {
        return Err(ProtocolError::FrameError(
            "response frame contains embedded `\\r`".to_string(),
        ));
    }
    // `!` reply: per Sky-Watcher spec §4 the payload is two hex
    // digits (`!XX\r`, 4 bytes). Empirically the Star Adventurer GTi
    // returns a single hex digit (`!X\r`, 3 bytes) for the
    // single-digit error codes defined in §5 (0..8). Accept either.
    if prefix == b'!' && frame.len() != 3 && frame.len() != 4 {
        return Err(ProtocolError::FrameError(format!(
            "error response must be `!X\\r` (3 bytes) or `!XX\\r` (4 bytes), got {} bytes",
            frame.len()
        )));
    }
    Ok((prefix, payload))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn encode_u8_uses_uppercase_hex() {
        assert_eq!(encode_u8(0x00), *b"00");
        assert_eq!(encode_u8(0x0A), *b"0A");
        assert_eq!(encode_u8(0xAB), *b"AB");
        assert_eq!(encode_u8(0xFF), *b"FF");
    }

    #[test]
    fn decode_u8_is_case_insensitive() {
        assert_eq!(decode_u8(*b"00").unwrap(), 0x00);
        assert_eq!(decode_u8(*b"ab").unwrap(), 0xAB);
        assert_eq!(decode_u8(*b"AB").unwrap(), 0xAB);
        assert_eq!(decode_u8(*b"aB").unwrap(), 0xAB);
        assert_eq!(decode_u8(*b"FF").unwrap(), 0xFF);
    }

    #[test]
    fn decode_u8_rejects_non_hex() {
        assert!(matches!(decode_u8(*b"0G"), Err(ProtocolError::HexError(_))));
        assert!(matches!(decode_u8(*b" 0"), Err(ProtocolError::HexError(_))));
    }

    #[test]
    fn encode_u24_uses_low_byte_first() {
        // Worked example from the design doc / Sky-Watcher spec.
        assert_eq!(encode_u24(0x12_3456), *b"563412");
        assert_eq!(encode_u24(0), *b"000000");
        assert_eq!(encode_u24(0xFF_FFFF), *b"FFFFFF");
        // Values above 24 bits are truncated.
        assert_eq!(encode_u24(0xAB12_3456), *b"563412");
    }

    #[test]
    fn decode_u24_round_trips_against_encode() {
        for value in [
            0x00_0000_u32,
            0x00_0001,
            0x12_3456,
            0xAB_CDEF,
            0xFF_FFFF,
            0x80_0000,
        ] {
            let bytes = encode_u24(value);
            assert_eq!(decode_u24(&bytes).unwrap(), value, "value=0x{value:06X}");
        }
    }

    #[test]
    fn encode_position_applies_the_bias() {
        // Per design doc: encoder count 0 → wire 0x800000 → "000080".
        assert_eq!(encode_position(0).unwrap(), *b"000080");
        // count +1 → 0x800001 → "010080"
        assert_eq!(encode_position(1).unwrap(), *b"010080");
        // count -1 → 0x7FFFFF → "FFFF7F"
        assert_eq!(encode_position(-1).unwrap(), *b"FFFF7F");
        // count +0x1234 → 0x801234 → "341280"
        assert_eq!(encode_position(0x1234).unwrap(), *b"341280");
        // count -0x1234 → 0x7FEDCC → "CCED7F"
        assert_eq!(encode_position(-0x1234).unwrap(), *b"CCED7F");
    }

    #[test]
    fn decode_position_round_trips_across_signed_range() {
        for ticks in [
            0,
            1,
            -1,
            POSITION_MAX,
            POSITION_MIN,
            0x1234,
            -0x1234,
            POSITION_MAX - 1,
            POSITION_MIN + 1,
        ] {
            let bytes = encode_position(ticks).unwrap();
            assert_eq!(decode_position(&bytes).unwrap(), ticks, "ticks={ticks}");
        }
    }

    #[test]
    fn encode_position_rejects_out_of_range() {
        assert!(matches!(
            encode_position(POSITION_MAX + 1),
            Err(ProtocolError::HexError(_))
        ));
        assert!(matches!(
            encode_position(POSITION_MIN - 1),
            Err(ProtocolError::HexError(_))
        ));
        assert!(matches!(
            encode_position(i32::MAX),
            Err(ProtocolError::HexError(_))
        ));
        assert!(matches!(
            encode_position(i32::MIN),
            Err(ProtocolError::HexError(_))
        ));
    }

    #[test]
    fn validate_command_frame_accepts_well_formed() {
        assert!(validate_command_frame(b":F1\r").is_ok());
        assert!(validate_command_frame(b":j1\r").is_ok());
        assert!(validate_command_frame(b":S1563412\r").is_ok());
    }

    #[test]
    fn validate_command_frame_rejects_malformed() {
        assert!(validate_command_frame(b"").is_err());
        assert!(validate_command_frame(b":\r").is_err()); // too short
        assert!(validate_command_frame(b"F1\r").is_err()); // no leading `:`
        assert!(validate_command_frame(b":F1").is_err()); // no trailing CR
        assert!(validate_command_frame(b":F1\r\0").is_err()); // junk after CR
        assert!(validate_command_frame(b":F\r1\r").is_err()); // embedded CR
    }

    #[test]
    fn validate_response_frame_accepts_success_and_error_replies() {
        assert!(validate_response_frame(b"=\r").is_ok());
        assert!(validate_response_frame(b"=000080\r").is_ok());
        // Spec §4: two hex digits.
        assert!(validate_response_frame(b"!04\r").is_ok());
        // Empirical Star Adventurer GTi: single hex digit for the
        // single-digit error codes (0..8) defined in §5.
        assert!(validate_response_frame(b"!4\r").is_ok());
    }

    #[test]
    fn validate_response_frame_rejects_malformed() {
        assert!(validate_response_frame(b":000080\r").is_err()); // command prefix
        assert!(validate_response_frame(b"=000080").is_err()); // no CR
        assert!(validate_response_frame(b"!\r").is_err()); // missing error code
        assert!(validate_response_frame(b"!0040\r").is_err()); // error too long (>2 chars)
        assert!(validate_response_frame(b"=00\r80\r").is_err()); // embedded CR
    }
}
