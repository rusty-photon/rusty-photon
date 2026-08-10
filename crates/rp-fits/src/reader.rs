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

    let bscale = read_float_keyword(image_hdu.get_header(), "BSCALE").unwrap_or(1.0);
    let bzero = read_float_keyword(image_hdu.get_header(), "BZERO").unwrap_or(0.0);
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
    #[expect(
        clippy::as_conversions,
        reason = "the guards establish the value is in i32 range; `as` then only truncates the fraction, the intended scaling behaviour"
    )]
    let scale = |v: f64| -> i32 {
        let scaled = v * img.bscale + img.bzero;
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
    let pixels: Vec<i32> = match img.data {
        Pixels::U8(v) => v.into_iter().map(|p| scale(f64::from(p))).collect(),
        Pixels::I16(v) => v.into_iter().map(|p| scale(f64::from(p))).collect(),
        Pixels::I32(v) => v.into_iter().map(|p| scale(f64::from(p))).collect(),
        Pixels::I64(v) => v.into_iter().map(|p| scale(int_to_f64(p))).collect(),
        Pixels::F32(v) => v.into_iter().map(|p| scale(f64::from(p))).collect(),
        Pixels::F64(v) => v.into_iter().map(scale).collect(),
    };
    Ok((pixels, img.width, img.height))
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
    let value = image_hdu.get_header().get(upper.as_str());
    match value {
        None => Ok(None),
        Some(FitsValue::Integer { value, .. }) => Ok(Some(KeywordValue::Int(*value))),
        Some(FitsValue::Float { value, .. }) => Ok(Some(KeywordValue::Float(*value))),
        Some(FitsValue::Logical { value, .. }) => Ok(Some(KeywordValue::Bool(*value))),
        Some(FitsValue::String { value, .. }) => {
            Ok(Some(KeywordValue::Str(value.trim_end().to_owned())))
        }
        Some(FitsValue::Undefined) => Ok(None),
        Some(FitsValue::Invalid(s)) => Err(FitsError::Parse(format!(
            "keyword {key} has invalid value: {s}"
        ))),
    }
}

/// Widen an integer pixel or header value to `f64`. Magnitudes beyond
/// 2^53 lose precision — far past any real FITS pixel depth or
/// BSCALE/BZERO — and the `i32` scaling path saturates afterwards
/// anyway.
#[expect(
    clippy::as_conversions,
    reason = "no lossless i64-to-f64 conversion exists; the precision loss beyond 2^53 is accepted and documented"
)]
fn int_to_f64(v: i64) -> f64 {
    v as f64
}

fn read_float_keyword<X>(header: &fitsrs::hdu::header::Header<X>, key: &str) -> Option<f64> {
    match header.get(key)? {
        FitsValue::Float { value, .. } => Some(*value),
        FitsValue::Integer { value, .. } => Some(int_to_f64(*value)),
        _ => None,
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

    #[test]
    fn read_primary_rejects_non_2d_image() {
        // Build a 3-axis BITPIX=32 HDU by hand. Reader rejects.
        let mut header = String::new();
        let push = |h: &mut String, line: String| {
            let mut padded = format!("{line:<80}");
            padded.truncate(80);
            h.push_str(&padded);
        };
        push(&mut header, "SIMPLE  =                    T".into());
        push(&mut header, "BITPIX  =                   32".into());
        push(&mut header, "NAXIS   =                    3".into());
        push(&mut header, "NAXIS1  =                    1".into());
        push(&mut header, "NAXIS2  =                    1".into());
        push(&mut header, "NAXIS3  =                    1".into());
        push(&mut header, "END".into());
        while !header.len().is_multiple_of(2880) {
            header.push(' ');
        }
        let mut bytes = header.into_bytes();
        bytes.extend(vec![0u8; 2880]);
        let err = read_primary(Cursor::new(&bytes[..])).unwrap_err();
        assert!(matches!(err, FitsError::Unsupported(_)));
    }
}
