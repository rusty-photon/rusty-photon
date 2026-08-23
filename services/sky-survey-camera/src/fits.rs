//! FITS primary-HDU reader for sky-survey-camera.
//!
//! Thin shim over [`rp_fits::reader::read_primary_as_i32`]. The
//! workspace's FITS surface is consolidated in `crates/rp-fits` per
//! ADR-001 Amendment A — this file used to host a hand-rolled ~200-line
//! parser duplicating that logic.
//!
//! Output is normalised to `i32` ADU values (BSCALE/BZERO applied,
//! saturating into `i32::MIN..=i32::MAX`), matching what ASCOM
//! `ImageArray` expects.

use std::io::Cursor;

use rp_fits::reader::read_primary_as_i32;

pub use rp_fits::FitsError;

/// `width` and `height` are `usize`: they describe `data`, and every
/// consumer uses them to slice or shape it.
#[derive(Debug, Clone)]
pub struct FitsImage {
    pub width: usize,
    pub height: usize,
    pub data: Vec<i32>,
}

/// Parse the primary HDU of a FITS payload into `(width, height,
/// Vec<i32>)`.
///
/// Reads BITPIX to size the data buffer, applies optional BSCALE /
/// BZERO, and saturates floats to `i32::MIN..=i32::MAX`.
///
/// # Errors
///
/// Returns the [`FitsError`] that `rp_fits`'s primary-HDU reader
/// reports for a malformed or truncated payload.
pub fn parse_primary_hdu(bytes: &[u8]) -> Result<FitsImage, FitsError> {
    let (data, width, height) = read_primary_as_i32(Cursor::new(bytes))?;
    Ok(FitsImage {
        width,
        height,
        data,
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use rp_fits::writer::{write_i32_image, write_u16_image, write_u8_image};

    #[test]
    fn parses_bitpix32_image() {
        let pixels = vec![100i32, -200, 0, 1_000_000];
        let mut buf = Vec::new();
        write_i32_image(&mut buf, &pixels, 2, 2, &[]).unwrap();
        let img = parse_primary_hdu(&buf).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(img.data, pixels);
    }

    #[test]
    fn parses_bitpix8_image() {
        let mut buf = Vec::new();
        write_u8_image(&mut buf, &[1u8, 2, 3, 4], 2, 2, &[]).unwrap();
        let img = parse_primary_hdu(&buf).unwrap();
        assert_eq!(img.data, vec![1i32, 2, 3, 4]);
    }

    #[test]
    fn parses_bitpix16_with_bzero_unsigned() {
        // SkyView-shaped fixture: BITPIX=16 + BZERO=32768. The wrapper
        // must apply BSCALE/BZERO so unsigned 16-bit values round-trip
        // back to their physical values, not the on-disk i16 storage.
        let pixels = vec![0u16, 32768, 65535, 12345];
        let mut buf = Vec::new();
        write_u16_image(&mut buf, &pixels, 2, 2, &[]).unwrap();
        let img = parse_primary_hdu(&buf).unwrap();
        assert_eq!(img.data, vec![0i32, 32768, 65535, 12345]);
    }

    #[test]
    fn rejects_truncated_payload() {
        parse_primary_hdu(&[0u8; 100]).unwrap_err();
    }

    #[test]
    fn rejects_non_fits_bytes() {
        let bytes = b"not a fits file at all -- random bytes here".repeat(100);
        parse_primary_hdu(&bytes).unwrap_err();
    }

    #[test]
    fn rejects_naxis_other_than_2() {
        // Build a 3-axis BITPIX=32 HDU by hand so we don't have to
        // teach the writer about higher-rank images. fitsrs reads the
        // 3-axis NAXIS and our wrapper rejects it as Unsupported.
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

        // Match on the variant only — the human-readable message is
        // an implementation detail of `rp-fits` and would otherwise
        // make this test brittle to harmless wording changes there.
        let err = parse_primary_hdu(&bytes).unwrap_err();
        assert!(
            matches!(err, FitsError::Unsupported(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn rejects_unknown_bitpix() {
        let mut header = String::new();
        let push = |h: &mut String, line: String| {
            let mut padded = format!("{line:<80}");
            padded.truncate(80);
            h.push_str(&padded);
        };
        push(&mut header, "SIMPLE  =                    T".into());
        push(&mut header, "BITPIX  =                   24".into());
        push(&mut header, "NAXIS   =                    2".into());
        push(&mut header, "NAXIS1  =                    1".into());
        push(&mut header, "NAXIS2  =                    1".into());
        push(&mut header, "END".into());
        while !header.len().is_multiple_of(2880) {
            header.push(' ');
        }
        let mut bytes = header.into_bytes();
        bytes.extend(vec![0u8; 2880]);

        // fitsrs surfaces BITPIX=24 as a parse error before we even
        // see the value; either parse failure or unsupported is fine.
        let err = parse_primary_hdu(&bytes).unwrap_err();
        assert!(
            matches!(err, FitsError::Parse(_) | FitsError::Unsupported(_)),
            "unexpected error: {err:?}"
        );
    }
}
