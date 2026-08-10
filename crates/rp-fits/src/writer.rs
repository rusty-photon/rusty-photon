//! Hand-rolled FITS primary-HDU writer.
//!
//! Emits BITPIX 8, 16 (signed) or 32 (signed) integer image HDUs in
//! the format defined by FITS Standard v4.0. The native unsigned 16-bit
//! path uses the standard `BITPIX=16 + BZERO=32768` convention so each
//! `u16` value `p` is serialized as the `i16` with value `p - 32768`.
//!
//! Block layout is the canonical 2880-byte FITS block: the header is
//! composed of 80-byte ASCII cards, terminated by `END`, then padded
//! with ASCII spaces to the next 2880-byte boundary; the data array
//! is big-endian, then zero-padded to the next 2880-byte boundary.
//!
//! Floating-point and i64 BITPIX writes are out of scope per
//! ADR-001 Amendment A.

use std::io::Write;

use crate::error::FitsError;

/// Size of a FITS block.
pub(crate) const BLOCK_SIZE: usize = 2880;
/// Size of a FITS header card.
pub(crate) const CARD_SIZE: usize = 80;
/// Reserved keywords managed by the writer; users may not supply them.
const RESERVED: &[&str] = &[
    "SIMPLE", "BITPIX", "NAXIS", "NAXIS1", "NAXIS2", "BSCALE", "BZERO", "END",
];

/// Typed value of a FITS header card. The variants map 1:1 to the
/// `FITSv4` §4.2 value types we serialize.
#[derive(Debug, Clone, PartialEq)]
pub enum KeywordValue {
    /// FITS logical: emitted as `T` or `F` right-justified at column 30.
    Bool(bool),
    /// FITS integer: emitted right-justified at column 30.
    Int(i64),
    /// FITS real: emitted in `%20.10E` form right-justified at column 30.
    Float(f64),
    /// FITS character string: emitted as `'value   '` starting at column 11,
    /// internally padded to at least 8 characters and any embedded `'`
    /// doubled per `FITSv4` §4.2.1.1.
    Str(String),
}

/// A user-supplied header card. Construct via [`Keyword::new`] which
/// validates the key.
#[derive(Debug, Clone)]
pub struct Keyword {
    key: [u8; 8],
    value: KeywordValue,
    comment: Option<String>,
}

impl Keyword {
    /// Build a keyword card from a key and a typed value.
    ///
    /// Validates per `FITSv4` §4.1.2.1: name is ≤ 8 chars, drawn from the
    /// restricted set `[A-Z0-9_-]` (case-insensitive — the input is
    /// uppercased). Reserved keywords (the writer emits SIMPLE, BITPIX,
    /// NAXIS{,1,2}, BSCALE, BZERO, END itself) are rejected. Float
    /// values must be finite (no NaN/±Inf — those are not valid FITS
    /// numeric forms). String values must be printable ASCII and short
    /// enough to fit in a single 80-byte card.
    pub fn new(key: &str, value: KeywordValue) -> Result<Self, FitsError> {
        if key.is_empty() || key.len() > 8 {
            return Err(FitsError::InvalidKeyword(format!(
                "keyword length must be 1..=8 (got {})",
                key.len()
            )));
        }
        let upper = key.to_ascii_uppercase();
        // FITSv4 §4.1.2.1: keyword chars are uppercase A-Z, digits,
        // hyphen, underscore. We uppercase first so callers can write
        // `Keyword::new("doc_id", …)` ergonomically.
        if !upper
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            return Err(FitsError::InvalidKeyword(format!(
                "keyword name must be [A-Z0-9_-]: {key:?}"
            )));
        }
        if RESERVED.contains(&upper.as_str()) {
            return Err(FitsError::InvalidKeyword(format!(
                "keyword {key:?} is reserved (writer emits it)"
            )));
        }
        match &value {
            KeywordValue::Str(s) => {
                if s.bytes().any(|b| !(0x20..=0x7E).contains(&b)) {
                    return Err(FitsError::InvalidKeyword(
                        "string value must be printable ASCII".into(),
                    ));
                }
                // FITSv4 fixed-format strings start at column 11 with a
                // single quote, value padded to 8+ chars, ending quote
                // before column 80. Double the embedded quote count to
                // bound the encoded length; the sum is at most twice a
                // live string's length, so saturating is its total
                // spelling.
                let escaped_len = s
                    .len()
                    .saturating_add(s.bytes().filter(|b| *b == b'\'').count());
                let padded_len = escaped_len.max(8);
                // 10 cols prefix ("KEYWORD  = ") + 2 quotes + body, plus
                // space for an optional " / comment". Reject anything
                // that can't possibly fit on a single card.
                if padded_len.saturating_add(12) > CARD_SIZE {
                    return Err(FitsError::InvalidKeyword(format!(
                        "string value too long for a single FITS card: {} bytes",
                        s.len()
                    )));
                }
            }
            KeywordValue::Float(f) if !f.is_finite() => {
                return Err(FitsError::InvalidKeyword(format!(
                    "float value must be finite (got {f})"
                )));
            }
            _ => {}
        }
        let mut key_buf = [b' '; 8];
        for (dst, b) in key_buf.iter_mut().zip(upper.bytes()) {
            *dst = b;
        }
        Ok(Self {
            key: key_buf,
            value,
            comment: None,
        })
    }

    /// Attach a `/ comment` suffix (non-recoverable on read; informational).
    pub fn with_comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }
}

/// Write a `u8` (BITPIX=8) image HDU.
pub fn write_u8_image<W: Write + ?Sized>(
    w: &mut W,
    pixels: &[u8],
    width: usize,
    height: usize,
    extra: &[Keyword],
) -> Result<(), FitsError> {
    let body = serialize_image(8, &[], pixels, width, height, extra, |out, &p| {
        out.push(p);
    })?;
    w.write_all(&body)?;
    Ok(())
}

/// Write a `u16` (BITPIX=16 + BZERO=32768) image HDU. Restores native
/// unsigned 16-bit semantics — the on-disk i16 representation is just
/// the FITS-mandated encoding for unsigned values.
pub fn write_u16_image<W: Write + ?Sized>(
    w: &mut W,
    pixels: &[u16],
    width: usize,
    height: usize,
    extra: &[Keyword],
) -> Result<(), FitsError> {
    // BSCALE = 1.0, BZERO = 32768.0 — the standard u16 dance. These are
    // reserved keywords the writer manages itself; bypass the public
    // `Keyword::new` reservation guard with a private constructor.
    let managed = [
        reserved_card("BSCALE", KeywordValue::Float(1.0)),
        reserved_card("BZERO", KeywordValue::Float(32768.0)),
    ];

    let body = serialize_image(16, &managed, pixels, width, height, extra, |out, &p| {
        // `p - 32768` in two's complement: wrap-then-reinterpret is the
        // BITPIX=16 + BZERO=32768 encoding of an unsigned sample.
        let raw = p.wrapping_sub(32768).cast_signed();
        out.extend_from_slice(&raw.to_be_bytes());
    })?;
    w.write_all(&body)?;
    Ok(())
}

/// Write an `i32` (BITPIX=32) image HDU.
pub fn write_i32_image<W: Write + ?Sized>(
    w: &mut W,
    pixels: &[i32],
    width: usize,
    height: usize,
    extra: &[Keyword],
) -> Result<(), FitsError> {
    let body = serialize_image(32, &[], pixels, width, height, extra, |out, &p| {
        out.extend_from_slice(&p.to_be_bytes());
    })?;
    w.write_all(&body)?;
    Ok(())
}

/// Internal: shared header+data serializer parameterized by per-pixel
/// big-endian emit. `managed_pre_user` are reserved keyword cards we
/// emit ourselves between the mandatory block and user-supplied cards
/// (currently only BSCALE/BZERO for the u16 path).
fn serialize_image<T, F>(
    bitpix: i8,
    managed_pre_user: &[Card],
    pixels: &[T],
    width: usize,
    height: usize,
    extra: &[Keyword],
    mut emit: F,
) -> Result<Vec<u8>, FitsError>
where
    F: FnMut(&mut Vec<u8>, &T),
{
    let expected = width
        .checked_mul(height)
        .ok_or_else(|| FitsError::Unsupported(format!("dimensions overflow: {width}x{height}")))?;
    if pixels.len() != expected {
        return Err(FitsError::DimensionMismatch {
            got: pixels.len(),
            width,
            height,
            expected,
        });
    }

    // `bitpix` is paired with `T` by the three public entry points, so
    // this is `size_of::<T>()`, and the check above established
    // `expected == pixels.len()`. The product is therefore the byte
    // length of a live slice, which Rust caps at `isize::MAX` — the
    // capacity arithmetic cannot overflow a `usize`, and saturating is
    // its total spelling.
    let bytes_per_pixel = usize::from(bitpix.unsigned_abs()) / 8;
    let mut out = Vec::with_capacity(
        expected
            .saturating_mul(bytes_per_pixel)
            .saturating_add(BLOCK_SIZE)
            .saturating_add(BLOCK_SIZE),
    );

    // Header.
    write_card(&mut out, &Card::logical("SIMPLE", true))?;
    write_card(&mut out, &Card::integer("BITPIX", i64::from(bitpix)))?;
    write_card(&mut out, &Card::integer("NAXIS", 2))?;
    let (Ok(naxis1), Ok(naxis2)) = (i64::try_from(width), i64::try_from(height)) else {
        return Err(FitsError::Unsupported(format!(
            "dimensions exceed a NAXIS card: {width}x{height}"
        )));
    };
    write_card(&mut out, &Card::integer("NAXIS1", naxis1))?;
    write_card(&mut out, &Card::integer("NAXIS2", naxis2))?;
    for c in managed_pre_user {
        write_card(&mut out, c)?;
    }
    for kw in extra {
        write_card(&mut out, &Card::from_keyword(kw))?;
    }
    write_end_card(&mut out);
    pad_to_block(&mut out, b' ');

    // Data.
    let data_start = out.len();
    for p in pixels {
        emit(&mut out, p);
    }
    debug_assert_eq!(
        out.len().saturating_sub(data_start),
        expected.saturating_mul(bytes_per_pixel)
    );
    pad_to_block(&mut out, 0);

    Ok(out)
}

/// Internal card representation. Public surface goes through [`Keyword`].
#[derive(Debug, Clone)]
pub(crate) struct Card {
    key: [u8; 8],
    value: KeywordValue,
    comment: Option<String>,
}

impl Card {
    fn integer(key: &str, value: i64) -> Self {
        Self {
            key: pad_key(key),
            value: KeywordValue::Int(value),
            comment: None,
        }
    }

    fn logical(key: &str, value: bool) -> Self {
        Self {
            key: pad_key(key),
            value: KeywordValue::Bool(value),
            comment: None,
        }
    }

    fn from_keyword(kw: &Keyword) -> Self {
        Self {
            key: kw.key,
            value: kw.value.clone(),
            comment: kw.comment.clone(),
        }
    }
}

fn reserved_card(key: &str, value: KeywordValue) -> Card {
    Card {
        key: pad_key(key),
        value,
        comment: None,
    }
}

fn pad_key(key: &str) -> [u8; 8] {
    let mut buf = [b' '; 8];
    for (dst, b) in buf.iter_mut().zip(key.bytes()) {
        *dst = b.to_ascii_uppercase();
    }
    buf
}

/// Emit one 80-byte card to `out`.
///
/// Returns [`FitsError::InvalidKeyword`] if the value (or value +
/// comment) cannot fit in a single 80-byte card. The previous version
/// silently truncated overlong cards via `Vec::resize`, which could
/// produce malformed FITS headers (e.g. a string value missing its
/// closing quote) without surfacing any error.
fn write_card(out: &mut Vec<u8>, card: &Card) -> Result<(), FitsError> {
    let start = out.len();
    out.extend_from_slice(&card.key);
    out.extend_from_slice(b"= ");
    match &card.value {
        KeywordValue::Bool(b) => {
            // Right-justified at column 30: 20 chars wide.
            let s = if *b { "T" } else { "F" };
            push_padded_left(out, s.as_bytes(), 20);
        }
        KeywordValue::Int(n) => {
            let s = format!("{n}");
            // i64 fits comfortably in 20 chars (i64::MIN is 20 chars).
            // Defensive check anyway in case a future numeric type lands.
            if s.len() > 20 {
                return Err(card_overflow(&card.key, "integer", s.len()));
            }
            push_padded_left(out, s.as_bytes(), 20);
        }
        KeywordValue::Float(f) => {
            // FITSv4 §4.2.4: any "F" or "E" form is acceptable. Our
            // %20.10E form fits in well under 20 chars.
            let s = format_float(*f);
            if s.len() > 20 {
                return Err(card_overflow(&card.key, "float", s.len()));
            }
            push_padded_left(out, s.as_bytes(), 20);
        }
        KeywordValue::Str(s) => {
            // FITSv4 §4.2.1.1: string starts at column 11 with a single
            // quote. Embedded single quotes are doubled. The string body
            // must be at least 8 characters (space-padded).
            let mut escaped = String::with_capacity(s.len());
            for c in s.chars() {
                if c == '\'' {
                    escaped.push_str("''");
                } else {
                    escaped.push(c);
                }
            }
            while escaped.len() < 8 {
                escaped.push(' ');
            }
            // Defensive: `Keyword::new` already rejects strings that
            // wouldn't fit. Re-check here for cards constructed via the
            // private `Card` API (BSCALE/BZERO etc.).
            let encoded_len = escaped.len().saturating_add(12);
            if encoded_len > CARD_SIZE {
                return Err(card_overflow(&card.key, "string", encoded_len));
            }
            out.push(b'\'');
            out.extend_from_slice(escaped.as_bytes());
            out.push(b'\'');
        }
    }
    if let Some(comment) = &card.comment {
        // " / <comment>" — only emit if it fits in the remaining bytes.
        // Long comments are *truncated* (informational, lossy is OK).
        // Long values fail above (lossy on a value would be silent
        // corruption).
        let remaining = CARD_SIZE.saturating_sub(out.len().saturating_sub(start));
        if remaining > 4 {
            out.extend_from_slice(b" / ");
            let max_comment = remaining.saturating_sub(3);
            out.extend(comment.bytes().take(max_comment));
        }
    }
    let written = out.len().saturating_sub(start);
    if written > CARD_SIZE {
        // Should be unreachable given the per-variant guards above;
        // defensive net so a future code path can't slip an overflow
        // past via the resize-truncate gap.
        return Err(card_overflow(&card.key, "card", written));
    }
    // Right-pad the card with spaces to 80 bytes.
    out.resize(start.saturating_add(CARD_SIZE), b' ');
    Ok(())
}

fn card_overflow(key: &[u8; 8], kind: &str, len: usize) -> FitsError {
    let key_str = std::str::from_utf8(key).unwrap_or("?").trim_end();
    FitsError::InvalidKeyword(format!(
        "{kind} value for keyword {key_str:?} is too long for a single FITS card ({len} bytes, max {CARD_SIZE})"
    ))
}

fn write_end_card(out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(b"END");
    out.resize(start.saturating_add(CARD_SIZE), b' ');
}

fn push_padded_left(out: &mut Vec<u8>, value: &[u8], width: usize) {
    let pad = width.saturating_sub(value.len());
    out.extend(std::iter::repeat_n(b' ', pad));
    out.extend_from_slice(value);
}

fn format_float(f: f64) -> String {
    // %20.10E form. `{:.10E}` gives `1.0000000000E0`; expand to
    // `1.0000000000E+00` to match the FITS canonical form.
    let raw = format!("{f:.10E}");
    // Insert sign on exponent if missing.
    let Some((mantissa, exp)) = raw.split_once('E') else {
        return raw;
    };
    let (sign, exp_digits) = match exp.chars().next() {
        Some('+' | '-') => exp.split_at(1),
        _ => ("+", exp),
    };
    // Pad exponent digits to at least 2.
    let padded_digits = if exp_digits.len() < 2 {
        format!("{exp_digits:0>2}")
    } else {
        exp_digits.to_string()
    };
    format!("{mantissa}E{sign}{padded_digits}")
}

fn pad_to_block(out: &mut Vec<u8>, fill: u8) {
    // `next_multiple_of` panics only if the result overflows `usize`,
    // and `out.len()` is capped at `isize::MAX` — unreachable here.
    out.resize(out.len().next_multiple_of(BLOCK_SIZE), fill);
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::io::Cursor;

    use fitsrs::Fits;
    use fitsrs::Pixels;
    use fitsrs::HDU;

    fn fitsrs_read_pixels_i32(bytes: &[u8]) -> Vec<i32> {
        let mut hdu_list = Fits::from_reader(Cursor::new(bytes));
        let HDU::Primary(hdu) = hdu_list.next().expect("hdu present").expect("hdu ok") else {
            panic!("expected primary HDU");
        };
        let _hdu = hdu;
        // Re-iterate to claim data after we've inspected the header.
        // fitsrs's iterator yields one HDU at a time; we re-open.
        let mut hdu_list = Fits::from_reader(Cursor::new(bytes));
        let hdu = match hdu_list.next().unwrap().unwrap() {
            HDU::Primary(h) => h,
            _ => panic!(),
        };
        let image = hdu_list.get_data(&hdu);
        match image.pixels() {
            Pixels::I32(it) => it.collect(),
            _ => panic!("expected I32 pixels"),
        }
    }

    fn fitsrs_read_pixels_i16(bytes: &[u8]) -> Vec<i16> {
        let mut hdu_list = Fits::from_reader(Cursor::new(bytes));
        let hdu = match hdu_list.next().unwrap().unwrap() {
            HDU::Primary(h) => h,
            _ => panic!(),
        };
        let image = hdu_list.get_data(&hdu);
        match image.pixels() {
            Pixels::I16(it) => it.collect(),
            _ => panic!("expected I16 pixels"),
        }
    }

    fn fitsrs_read_pixels_u8(bytes: &[u8]) -> Vec<u8> {
        let mut hdu_list = Fits::from_reader(Cursor::new(bytes));
        let hdu = match hdu_list.next().unwrap().unwrap() {
            HDU::Primary(h) => h,
            _ => panic!(),
        };
        let image = hdu_list.get_data(&hdu);
        match image.pixels() {
            Pixels::U8(it) => it.collect(),
            _ => panic!("expected U8 pixels"),
        }
    }

    #[test]
    fn writes_u8_image_with_block_aligned_size() {
        let mut buf = Vec::new();
        write_u8_image(&mut buf, &[1, 2, 3, 4], 2, 2, &[]).unwrap();
        assert!(
            buf.len() % BLOCK_SIZE == 0,
            "output not block-aligned: {}",
            buf.len()
        );
        assert_eq!(fitsrs_read_pixels_u8(&buf), vec![1, 2, 3, 4]);
    }

    #[test]
    fn writes_i32_image_round_trips_through_fitsrs() {
        let pixels = vec![100, -200, 0, 1_000_000];
        let mut buf = Vec::new();
        write_i32_image(&mut buf, &pixels, 2, 2, &[]).unwrap();
        assert!(buf.len().is_multiple_of(BLOCK_SIZE));
        assert_eq!(fitsrs_read_pixels_i32(&buf), pixels);
    }

    #[test]
    fn writes_u16_image_uses_bzero_convention() {
        // Boundary values: 0, 32768 (BZERO point), 65535 (max), 12345 (mid).
        let pixels = vec![0u16, 32768, 65535, 12345];
        let mut buf = Vec::new();
        write_u16_image(&mut buf, &pixels, 2, 2, &[]).unwrap();
        // Read raw i16 values via fitsrs — should be the BZERO-shifted form.
        let raw = fitsrs_read_pixels_i16(&buf);
        assert_eq!(raw, vec![-32768, 0, 32767, -20423]);
        // Apply BSCALE/BZERO ourselves and confirm we get the originals back.
        let recovered: Vec<u16> = raw.iter().map(|r| (i32::from(*r) + 32768) as u16).collect();
        assert_eq!(recovered, pixels);
    }

    #[test]
    fn writes_keyword_card_visible_to_fitsrs() {
        let mut buf = Vec::new();
        let kw = vec![
            Keyword::new("DOC_ID", KeywordValue::Str("abc-uuid".into())).unwrap(),
            Keyword::new("OBSERVER", KeywordValue::Str("Igor".into())).unwrap(),
            Keyword::new("EXPTIME", KeywordValue::Float(2.5)).unwrap(),
        ];
        write_i32_image(&mut buf, &[0i32; 4], 2, 2, &kw).unwrap();

        let mut hdu_list = Fits::from_reader(Cursor::new(&buf[..]));
        let hdu = match hdu_list.next().unwrap().unwrap() {
            HDU::Primary(h) => h,
            _ => panic!(),
        };
        let header = hdu.get_header();
        let doc = header.get("DOC_ID").expect("DOC_ID present");
        match doc {
            fitsrs::card::Value::String { value, .. } => assert_eq!(value, "abc-uuid"),
            other => panic!("expected DOC_ID to be string, got {other:?}"),
        }
        let obs = header.get("OBSERVER").expect("OBSERVER present");
        assert!(
            matches!(obs, fitsrs::card::Value::String { value, .. } if value == "Igor"),
            "unexpected OBSERVER: {obs:?}"
        );
        let exp = header.get("EXPTIME").expect("EXPTIME present");
        match exp {
            fitsrs::card::Value::Float { value, .. } => {
                assert!((value - 2.5).abs() < 1e-9, "got {value}");
            }
            other => panic!("expected EXPTIME to be float, got {other:?}"),
        }
    }

    #[test]
    fn rejects_dimension_mismatch() {
        let mut buf = Vec::new();
        let err = write_i32_image(&mut buf, &[1, 2, 3], 2, 2, &[]).unwrap_err();
        match err {
            FitsError::DimensionMismatch { got, expected, .. } => {
                assert_eq!((got, expected), (3, 4));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// Dimensions whose product no buffer could hold must be reported,
    /// not wrapped into a plausible-looking pixel count.
    #[test]
    fn rejects_dimensions_that_overflow_a_pixel_count() {
        let mut buf = Vec::new();
        let err = write_i32_image(&mut buf, &[], usize::MAX, usize::MAX, &[]).unwrap_err();
        assert!(
            err.to_string().contains("dimensions overflow"),
            "expected an overflow error, got: {err}"
        );
    }

    #[test]
    fn rejects_oversize_keyword() {
        let err = Keyword::new("THIS_IS_TOO_LONG", KeywordValue::Bool(true)).unwrap_err();
        assert!(matches!(err, FitsError::InvalidKeyword(_)));
    }

    #[test]
    fn rejects_reserved_keyword() {
        let err = Keyword::new("BITPIX", KeywordValue::Int(8)).unwrap_err();
        match err {
            FitsError::InvalidKeyword(msg) => assert!(msg.contains("reserved"), "{msg}"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn rejects_non_ascii_keyword() {
        let err = Keyword::new("FÖÖ", KeywordValue::Bool(true)).unwrap_err();
        assert!(matches!(err, FitsError::InvalidKeyword(_)));
    }

    #[test]
    fn rejects_keyword_with_disallowed_punctuation() {
        // FITSv4 §4.1.2.1 restricts keyword chars to [A-Z0-9_-]. Other
        // ASCII graphic chars (=, ', ., etc.) must be rejected even
        // though they're printable.
        for bad in ["KEY=Y", "FOO.BAR", "A'B", "X+Y"] {
            let err = Keyword::new(bad, KeywordValue::Bool(true)).unwrap_err();
            assert!(
                matches!(err, FitsError::InvalidKeyword(_)),
                "should reject {bad:?}: {err:?}"
            );
        }
    }

    #[test]
    fn rejects_non_finite_float() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = Keyword::new("EXPTIME", KeywordValue::Float(bad)).unwrap_err();
            assert!(
                matches!(err, FitsError::InvalidKeyword(_)),
                "should reject {bad}: {err:?}"
            );
        }
    }

    #[test]
    fn rejects_overlong_string_value() {
        // The fixed-format card has 80 bytes total; "KEYWORD  = " uses
        // 10 chars plus 2 quotes leaves 68 chars for the body. A string
        // longer than that cannot fit in a single card.
        let too_long = "x".repeat(70);
        let err = Keyword::new("OBJECT", KeywordValue::Str(too_long)).unwrap_err();
        assert!(
            matches!(err, FitsError::InvalidKeyword(_)),
            "expected InvalidKeyword, got {err:?}"
        );
    }

    #[test]
    fn write_card_errors_on_value_overflow() {
        // Build a Card directly via the private API to bypass
        // `Keyword::new`'s length check, then verify `write_card`
        // rejects it instead of silently truncating.
        let mut buf = Vec::new();
        let bad = Card {
            key: pad_key("HISTORY"),
            value: KeywordValue::Str("x".repeat(80)),
            comment: None,
        };
        let err = write_card(&mut buf, &bad).unwrap_err();
        assert!(matches!(err, FitsError::InvalidKeyword(_)), "{err:?}");
    }

    #[test]
    fn header_padding_is_spaces_data_padding_is_zero() {
        let mut buf = Vec::new();
        write_u8_image(&mut buf, &[7u8; 16], 4, 4, &[]).unwrap();
        // First block is the header; last bytes before the data block
        // start (offset 2880) should be spaces.
        assert_eq!(
            buf[2879], b' ',
            "header should be space-padded to block boundary"
        );
        // Data block begins at 2880; pixel bytes are 16, so bytes
        // 2880..2880+16 are pixels (0x07), and the rest of the block
        // through 5760 is zero-padded.
        for &b in &buf[2880..2880 + 16] {
            assert_eq!(b, 7);
        }
        assert_eq!(buf[2880 + 16], 0, "data should zero-pad after pixels");
        assert_eq!(buf[5759], 0, "data block should zero-pad to boundary");
    }

    #[test]
    fn naxis_ordering_is_width_then_height() {
        // 4 wide, 2 tall (8 pixels). Verify NAXIS1=4 in the header.
        let mut buf = Vec::new();
        write_i32_image(&mut buf, &[1, 2, 3, 4, 5, 6, 7, 8], 4, 2, &[]).unwrap();
        let mut hdu_list = Fits::from_reader(Cursor::new(&buf[..]));
        let hdu = match hdu_list.next().unwrap().unwrap() {
            HDU::Primary(h) => h,
            _ => panic!(),
        };
        let header = hdu.get_header();
        let naxis1 = header.get("NAXIS1").expect("NAXIS1");
        let naxis2 = header.get("NAXIS2").expect("NAXIS2");
        assert!(matches!(naxis1, fitsrs::card::Value::Integer { value, .. } if *value == 4));
        assert!(matches!(naxis2, fitsrs::card::Value::Integer { value, .. } if *value == 2));
    }

    #[test]
    fn empty_image_is_supported() {
        let mut buf = Vec::new();
        write_i32_image(&mut buf, &[], 0, 0, &[]).unwrap();
        // Header block + no data; should still be 2880 bytes.
        assert_eq!(buf.len(), BLOCK_SIZE);
    }

    #[test]
    fn string_keyword_escapes_embedded_quote() {
        let mut buf = Vec::new();
        let kw = vec![Keyword::new("OBJECT", KeywordValue::Str("o'brien".into())).unwrap()];
        write_i32_image(&mut buf, &[0i32; 4], 2, 2, &kw).unwrap();
        // fitsrs unescapes `''` back to `'` on read.
        let mut hdu_list = Fits::from_reader(Cursor::new(&buf[..]));
        let hdu = match hdu_list.next().unwrap().unwrap() {
            HDU::Primary(h) => h,
            _ => panic!(),
        };
        let v = hdu.get_header().get("OBJECT").expect("OBJECT");
        match v {
            fitsrs::card::Value::String { value, .. } => assert_eq!(value, "o'brien"),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn keyword_with_comment_round_trips() {
        let mut buf = Vec::new();
        let kw = vec![Keyword::new("GAIN", KeywordValue::Int(100))
            .unwrap()
            .with_comment("ADU per electron")];
        write_i32_image(&mut buf, &[0i32; 4], 2, 2, &kw).unwrap();
        let mut hdu_list = Fits::from_reader(Cursor::new(&buf[..]));
        let hdu = match hdu_list.next().unwrap().unwrap() {
            HDU::Primary(h) => h,
            _ => panic!(),
        };
        let v = hdu.get_header().get("GAIN").expect("GAIN");
        match v {
            fitsrs::card::Value::Integer { value, comment } => {
                assert_eq!(*value, 100);
                assert!(
                    comment
                        .as_deref()
                        .unwrap_or("")
                        .contains("ADU per electron"),
                    "comment lost on round-trip: {comment:?}"
                );
            }
            other => panic!("expected int, got {other:?}"),
        }
    }

    #[test]
    fn format_float_handles_negative_and_zero() {
        // %20.10E forms — verify we always emit `E±DD` (FITS canonical).
        let s_pos = format_float(1.5);
        assert!(s_pos.contains("E+"), "got {s_pos:?}");
        let s_neg = format_float(-2.5e-3);
        assert!(s_neg.starts_with('-'), "got {s_neg:?}");
        assert!(s_neg.contains("E-"), "got {s_neg:?}");
        let s_zero = format_float(0.0);
        assert!(s_zero.contains('E'), "got {s_zero:?}");
    }
}
