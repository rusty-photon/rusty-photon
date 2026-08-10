//! Thin reader facade over [`fitsrs`].
//!
//! Three entry points cover what the workspace's three FITS consumers
//! need:
//!
//! - [`read_primary`] returns the on-disk pixel type plus header
//!   metadata (`bscale`, `bzero`, `blank`). Callers that need exact
//!   unsigned-16 or other domain-specific scaling apply it themselves.
//! - [`read_primary_as_i32`] applies BSCALE/BZERO and saturates to
//!   `i32`, matching the `Vec<i32>` shape sky-survey-camera and rp's
//!   imaging pipeline expect.
//! - [`read_primary_keyword`] reads only the primary header — much
//!   cheaper than [`read_primary`] when the caller only needs one
//!   keyword (e.g. rp's `DOC_ID` lookup).
//!
//! BLANK handling: the raw integer sentinel value is surfaced via
//! [`FitsImage::blank`] and **not** filtered or replaced. Per
//! ADR-001 Amendment A, silently dropping pixels (the previous
//! `fitrs`-based path's behaviour) is a bug we're fixing here.

use std::fmt::Debug;
use std::io::{Read, Seek};

use fitsrs::card::Value as FitsValue;
use fitsrs::{Fits, Pixels as FitsPixels, HDU};

use crate::error::FitsError;
use crate::writer::KeywordValue;

/// Decoded primary HDU. `data` is the on-disk numeric type; consumers
/// that want a single typed `Vec<T>` should use [`read_primary_as_i32`]
/// or apply their own scaling.
///
/// `width` and `height` are `usize` because everything a caller does
/// with them — `data.len()`, a row offset, an `ndarray` shape — indexes
/// the buffer alongside. The fixed-width `NAXIS` values they were
/// parsed from stay on the writer's side of the boundary, where they
/// are header cards rather than lengths.
#[derive(Debug, Clone)]
pub struct FitsImage {
    pub width: usize,
    pub height: usize,
    pub data: Pixels,
    /// FITS `BSCALE` (default 1.0). Multiplied into the raw pixel value.
    pub bscale: f64,
    /// FITS `BZERO` (default 0.0). Added to the BSCALE-multiplied pixel.
    pub bzero: f64,
    /// FITS `BLANK` sentinel for integer images, raw and unscaled.
    /// Surfaced as-is so callers can decide how to handle it.
    pub blank: Option<i64>,
}

#[derive(Debug, Clone)]
pub enum Pixels {
    U8(Vec<u8>),
    I16(Vec<i16>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
}

impl Pixels {
    /// Decoded pixel count, whatever the on-disk type.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::U8(v) => v.len(),
            Self::I16(v) => v.len(),
            Self::I32(v) => v.len(),
            Self::I64(v) => v.len(),
            Self::F32(v) => v.len(),
            Self::F64(v) => v.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Collapse to `i32` samples by the FITS physical-value equation
    /// `value = bzero + bscale × raw`, saturating to the `i32` range;
    /// NaN maps to 0. Consumes the buffer and keeps its row-major
    /// order.
    #[must_use]
    #[expect(
        clippy::as_conversions,
        reason = "the guards make `scaled as i32` an in-range truncation, and no lossless i64-to-f64 exists — that widening's precision loss beyond 2^53 cannot outlive the i32 saturation"
    )]
    pub fn scaled_to_i32(self, bscale: f64, bzero: f64) -> Vec<i32> {
        let scale = |v: f64| -> i32 {
            let scaled = v * bscale + bzero;
            if scaled.is_nan() {
                0
            } else if scaled >= f64::from(i32::MAX) {
                i32::MAX
            } else if scaled <= f64::from(i32::MIN) {
                i32::MIN
            } else {
                scaled as i32
            }
        };
        match self {
            Self::U8(v) => v.into_iter().map(|p| scale(f64::from(p))).collect(),
            Self::I16(v) => v.into_iter().map(|p| scale(f64::from(p))).collect(),
            Self::I32(v) => v.into_iter().map(|p| scale(f64::from(p))).collect(),
            Self::I64(v) => v.into_iter().map(|p| scale(p as f64)).collect(),
            Self::F32(v) => v.into_iter().map(|p| scale(f64::from(p))).collect(),
            Self::F64(v) => v.into_iter().map(scale).collect(),
        }
    }
}

/// Read the primary HDU of a FITS stream. Returns the on-disk pixel
/// data plus BSCALE/BZERO/BLANK metadata. The reader must support
/// seeking — `Cursor<&[u8]>` and `BufReader<File>` both qualify.
pub fn read_primary<R: Read + Seek + Debug>(reader: R) -> Result<FitsImage, FitsError> {
    let mut hdu_list = Fits::from_reader(reader);
    let hdu = hdu_list
        .next()
        .ok_or_else(|| FitsError::Parse("FITS stream contains no HDUs".into()))?
        .map_err(|e| FitsError::Parse(format!("primary HDU parse failed: {e}")))?;

    let HDU::Primary(image_hdu) = hdu else {
        return Err(FitsError::Unsupported(
            "first HDU is not a primary image HDU".into(),
        ));
    };

    let xtension = image_hdu.get_header().get_xtension();
    let naxis = xtension.get_naxis();
    let &[naxis1, naxis2] = naxis else {
        return Err(FitsError::Unsupported(format!(
            "only 2-D images are supported (NAXIS = {})",
            naxis.len()
        )));
    };
    let width = usize::try_from(naxis1)
        .map_err(|_| FitsError::Parse(format!("NAXIS1 out of range: {naxis1}")))?;
    let height = usize::try_from(naxis2)
        .map_err(|_| FitsError::Parse(format!("NAXIS2 out of range: {naxis2}")))?;

    let bscale = read_real_keyword(image_hdu.get_header(), "BSCALE")?.unwrap_or(1.0);
    let bzero = read_real_keyword(image_hdu.get_header(), "BZERO")?.unwrap_or(0.0);
    let blank = read_int_keyword(image_hdu.get_header(), "BLANK");

    let image = hdu_list.get_data(&image_hdu);
    let data = match image.pixels() {
        FitsPixels::U8(it) => Pixels::U8(it.collect()),
        FitsPixels::I16(it) => Pixels::I16(it.collect()),
        FitsPixels::I32(it) => Pixels::I32(it.collect()),
        FitsPixels::I64(it) => Pixels::I64(it.collect()),
        FitsPixels::F32(it) => Pixels::F32(it.collect()),
        FitsPixels::F64(it) => Pixels::F64(it.collect()),
    };

    // The geometry describes `data`, so establish that here instead of
    // leaving each consumer to discover it. A truncated stream decodes
    // to fewer pixels than `NAXIS1 × NAXIS2` promises, and a consumer
    // that walks it by row — sky-survey-camera crops survey responses
    // fetched over HTTP — would slice past the end.
    let expected = width
        .checked_mul(height)
        .ok_or_else(|| FitsError::Parse(format!("NAXIS {width}×{height} overflows a buffer")))?;
    if data.len() != expected {
        return Err(FitsError::Parse(format!(
            "pixel count {} does not match NAXIS {width}×{height} (expected {expected})",
            data.len()
        )));
    }

    Ok(FitsImage {
        width,
        height,
        data,
        bscale,
        bzero,
        blank,
    })
}

/// Read the primary HDU and return scaled-to-`i32` pixels in row-major
/// order. Applies BSCALE/BZERO and saturates to `i32::MIN..=i32::MAX`,
/// matching the legacy sky-survey-camera and rp behaviour.
pub fn read_primary_as_i32<R: Read + Seek + Debug>(
    reader: R,
) -> Result<(Vec<i32>, usize, usize), FitsError> {
    let img = read_primary(reader)?;
    let (width, height) = (img.width, img.height);
    Ok((img.data.scaled_to_i32(img.bscale, img.bzero), width, height))
}

/// Read a single keyword from the primary HDU's header. Cheaper than
/// [`read_primary`] when the caller only needs metadata (e.g. rp's
/// `DOC_ID` resolver). Returns `Ok(None)` when the keyword is absent.
pub fn read_primary_keyword<R: Read + Seek + Debug>(
    reader: R,
    key: &str,
) -> Result<Option<KeywordValue>, FitsError> {
    let mut hdu_list = Fits::from_reader(reader);
    let hdu = hdu_list
        .next()
        .ok_or_else(|| FitsError::Parse("FITS stream contains no HDUs".into()))?
        .map_err(|e| FitsError::Parse(format!("primary HDU parse failed: {e}")))?;
    let HDU::Primary(image_hdu) = hdu else {
        return Err(FitsError::Unsupported(
            "first HDU is not a primary image HDU".into(),
        ));
    };
    let upper = key.to_ascii_uppercase();
    match image_hdu.get_header().get(upper.as_str()) {
        None => Ok(None),
        Some(value) => keyword_value_from_fits(key, value),
    }
}

/// Map a fitsrs card value onto the crate's [`KeywordValue`].
/// `Ok(None)` for an undefined value; an unparseable value is a parse
/// error.
fn keyword_value_from_fits(
    key: &str,
    value: &FitsValue,
) -> Result<Option<KeywordValue>, FitsError> {
    match value {
        FitsValue::Integer { value, .. } => Ok(Some(KeywordValue::Int(*value))),
        FitsValue::Float { value, .. } => Ok(Some(KeywordValue::Float(*value))),
        FitsValue::Logical { value, .. } => Ok(Some(KeywordValue::Bool(*value))),
        FitsValue::String { value, .. } => Ok(Some(KeywordValue::Str(value.trim_end().to_owned()))),
        FitsValue::Undefined => Ok(None),
        FitsValue::Invalid(s) => Err(FitsError::Parse(format!(
            "keyword {key} has invalid value: {s}"
        ))),
    }
}

/// Read a real-valued keyword that FITS lets writers store as either a
/// Float or an Integer card. `Ok(None)` when the keyword is absent or
/// non-numeric; an unparseable card is a parse error, not a silent
/// fall-back to the caller's default.
fn read_real_keyword<X>(
    header: &fitsrs::hdu::header::Header<X>,
    key: &str,
) -> Result<Option<f64>, FitsError> {
    match header.get(key) {
        None => Ok(None),
        Some(value) => Ok(keyword_value_from_fits(key, value)?.and_then(|v| v.as_real())),
    }
}

fn read_int_keyword<X>(header: &fitsrs::hdu::header::Header<X>, key: &str) -> Option<i64> {
    match header.get(key)? {
        FitsValue::Integer { value, .. } => Some(*value),
        _ => None,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::io::Cursor;

    use crate::writer::{write_i32_image, write_u16_image, write_u8_image, Keyword};

    #[test]
    fn round_trip_i32() {
        let pixels = vec![100i32, -200, 0, 1_000_000];
        let mut buf = Vec::new();
        write_i32_image(&mut buf, &pixels, 2, 2, &[]).unwrap();
        let img = read_primary(Cursor::new(&buf[..])).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        match img.data {
            Pixels::I32(v) => assert_eq!(v, pixels),
            other => panic!("expected I32, got {other:?}"),
        }
        assert_eq!(img.bscale, 1.0);
        assert_eq!(img.bzero, 0.0);
        assert!(img.blank.is_none());
    }

    /// A stream whose data section ends early decodes to fewer pixels
    /// than `NAXIS` promises. `FitsImage`'s geometry describes its
    /// buffer, so that has to be a parse error here — a consumer that
    /// walks the buffer by row would otherwise slice past the end.
    #[test]
    fn truncated_data_section_is_a_parse_error() {
        let pixels: Vec<i32> = (0..64).collect();
        let mut buf = Vec::new();
        write_i32_image(&mut buf, &pixels, 8, 8, &[]).unwrap();
        // Keep the header block, drop most of the pixel data.
        buf.truncate(2880 + 32);

        let err = read_primary(Cursor::new(&buf[..])).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("does not match NAXIS"),
            "expected a pixel-count error, got: {msg}"
        );
    }

    #[test]
    fn round_trip_u8() {
        let mut buf = Vec::new();
        write_u8_image(&mut buf, &[1u8, 2, 3, 4, 5, 6], 3, 2, &[]).unwrap();
        let img = read_primary(Cursor::new(&buf[..])).unwrap();
        assert_eq!((img.width, img.height), (3, 2));
        match img.data {
            Pixels::U8(v) => assert_eq!(v, vec![1, 2, 3, 4, 5, 6]),
            other => panic!("expected U8, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_u16_via_bzero_metadata() {
        // `read_primary` returns the raw i16 + bscale/bzero; caller
        // applies the inversion. Confirms the metadata is surfaced.
        let pixels = vec![0u16, 32768, 65535, 12345];
        let mut buf = Vec::new();
        write_u16_image(&mut buf, &pixels, 2, 2, &[]).unwrap();
        let img = read_primary(Cursor::new(&buf[..])).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(img.bscale, 1.0);
        assert_eq!(img.bzero, 32768.0);
        let raw = match img.data {
            Pixels::I16(v) => v,
            other => panic!("expected I16, got {other:?}"),
        };
        let recovered: Vec<u16> = raw.iter().map(|r| (i32::from(*r) + 32768) as u16).collect();
        assert_eq!(recovered, pixels);
    }

    #[test]
    fn read_primary_as_i32_applies_scaling() {
        // u16 image written via the BZERO=32768 path. `read_primary_as_i32`
        // should hand back the *physical* values (0..=65535), not the raw
        // i16 storage values.
        let pixels = vec![0u16, 32768, 65535, 12345];
        let mut buf = Vec::new();
        write_u16_image(&mut buf, &pixels, 2, 2, &[]).unwrap();
        let (got, w, h) = read_primary_as_i32(Cursor::new(&buf[..])).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(got, vec![0i32, 32768, 65535, 12345]);
    }

    #[test]
    fn read_primary_as_i32_passes_through_i32() {
        let pixels = vec![1i32, -1, 1_000_000, i32::MIN, i32::MAX];
        let mut buf = Vec::new();
        write_i32_image(&mut buf, &pixels, 5, 1, &[]).unwrap();
        let (got, _, _) = read_primary_as_i32(Cursor::new(&buf[..])).unwrap();
        assert_eq!(got, pixels);
    }

    #[test]
    fn read_primary_keyword_returns_string() {
        let mut buf = Vec::new();
        let kw = vec![Keyword::new("DOC_ID", KeywordValue::Str("uuid-here".into())).unwrap()];
        write_i32_image(&mut buf, &[0i32; 4], 2, 2, &kw).unwrap();
        let v = read_primary_keyword(Cursor::new(&buf[..]), "DOC_ID").unwrap();
        match v {
            Some(KeywordValue::Str(s)) => assert_eq!(s, "uuid-here"),
            other => panic!("expected string DOC_ID, got {other:?}"),
        }
    }

    #[test]
    fn read_primary_keyword_returns_none_when_absent() {
        let mut buf = Vec::new();
        write_i32_image(&mut buf, &[0i32; 4], 2, 2, &[]).unwrap();
        let v = read_primary_keyword(Cursor::new(&buf[..]), "DOC_ID").unwrap();
        assert!(v.is_none());
    }

    #[test]
    fn read_primary_keyword_is_case_insensitive() {
        let mut buf = Vec::new();
        let kw = vec![Keyword::new("EXPTIME", KeywordValue::Float(3.5)).unwrap()];
        write_i32_image(&mut buf, &[0i32; 4], 2, 2, &kw).unwrap();
        let v = read_primary_keyword(Cursor::new(&buf[..]), "exptime")
            .unwrap()
            .unwrap();
        match v {
            KeywordValue::Float(f) => assert!((f - 3.5).abs() < 1e-9),
            other => panic!("expected float, got {other:?}"),
        }
    }

    #[test]
    fn read_primary_rejects_empty_stream() {
        let err = read_primary(Cursor::new(&b""[..])).unwrap_err();
        assert!(matches!(err, FitsError::Parse(_)));
    }

    #[test]
    fn read_primary_as_i32_handles_u8() {
        let mut buf = Vec::new();
        write_u8_image(&mut buf, &[0u8, 7, 200, 255], 2, 2, &[]).unwrap();
        let (got, w, h) = read_primary_as_i32(Cursor::new(&buf[..])).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(got, vec![0i32, 7, 200, 255]);
    }

    #[test]
    fn read_primary_keyword_returns_int() {
        let mut buf = Vec::new();
        let kw = vec![Keyword::new("GAIN", KeywordValue::Int(42)).unwrap()];
        write_i32_image(&mut buf, &[0i32; 4], 2, 2, &kw).unwrap();
        let v = read_primary_keyword(Cursor::new(&buf[..]), "GAIN")
            .unwrap()
            .unwrap();
        match v {
            KeywordValue::Int(n) => assert_eq!(n, 42),
            other => panic!("expected int, got {other:?}"),
        }
    }

    #[test]
    fn read_primary_keyword_returns_bool() {
        let mut buf = Vec::new();
        let kw = vec![Keyword::new("LIGHT", KeywordValue::Bool(true)).unwrap()];
        write_i32_image(&mut buf, &[0i32; 4], 2, 2, &kw).unwrap();
        let v = read_primary_keyword(Cursor::new(&buf[..]), "LIGHT")
            .unwrap()
            .unwrap();
        assert!(matches!(v, KeywordValue::Bool(true)));
    }

    fn handmade_hdu(cards: &[&str]) -> Vec<u8> {
        let mut header = String::new();
        for line in cards {
            let mut padded = format!("{line:<80}");
            padded.truncate(80);
            header.push_str(&padded);
        }
        while !header.len().is_multiple_of(2880) {
            header.push(' ');
        }
        let mut bytes = header.into_bytes();
        bytes.extend(vec![0u8; 2880]);
        bytes
    }

    #[test]
    fn read_primary_rejects_non_2d_image() {
        // Build a 3-axis BITPIX=32 HDU by hand. Reader rejects.
        let bytes = handmade_hdu(&[
            "SIMPLE  =                    T",
            "BITPIX  =                   32",
            "NAXIS   =                    3",
            "NAXIS1  =                    1",
            "NAXIS2  =                    1",
            "NAXIS3  =                    1",
            "END",
        ]);
        let err = read_primary(Cursor::new(&bytes[..])).unwrap_err();
        assert!(matches!(err, FitsError::Unsupported(_)));
    }

    /// An unparseable `BSCALE` card must be a parse error, not a silent
    /// fall-back to the 1.0 default that would rescale every pixel.
    #[test]
    fn corrupt_bscale_card_is_a_parse_error() {
        let bytes = handmade_hdu(&[
            "SIMPLE  =                    T",
            "BITPIX  =                   32",
            "NAXIS   =                    2",
            "NAXIS1  =                    1",
            "NAXIS2  =                    1",
            "BSCALE  = abc",
            "END",
        ]);
        let err = read_primary(Cursor::new(&bytes[..])).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("BSCALE") && msg.contains("invalid"),
            "expected an invalid-BSCALE parse error, got: {msg}"
        );
    }

    #[test]
    fn integer_bscale_and_bzero_cards_read_as_reals() {
        // Foreign writers routinely emit BSCALE/BZERO as integer cards.
        let bytes = handmade_hdu(&[
            "SIMPLE  =                    T",
            "BITPIX  =                   32",
            "NAXIS   =                    2",
            "NAXIS1  =                    1",
            "NAXIS2  =                    1",
            "BSCALE  =                    2",
            "BZERO   =                32768",
            "END",
        ]);
        let img = read_primary(Cursor::new(&bytes[..])).unwrap();
        assert_eq!(img.bscale, 2.0);
        assert_eq!(img.bzero, 32768.0);
    }

    #[test]
    fn scaled_to_i32_saturates_and_zeroes_nan() {
        let pixels = Pixels::F64(vec![f64::NAN, 1e300, -1e300, 1.9]);
        assert_eq!(
            pixels.scaled_to_i32(1.0, 0.0),
            vec![0, i32::MAX, i32::MIN, 1]
        );
    }

    #[test]
    fn scaled_to_i32_applies_the_equation_to_i64_pixels() {
        let pixels = Pixels::I64(vec![0, 100, -100]);
        assert_eq!(pixels.scaled_to_i32(2.0, 10.0), vec![10, 210, -190]);
    }
}
