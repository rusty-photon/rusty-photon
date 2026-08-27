//! FITS file utilities for saving guide-star image data.
//!
//! Pixel encoding is delegated to `crates/rp-fits` per ADR-001
//! Amendment A. Native unsigned 16-bit values are written via the
//! `BITPIX=16 + BZERO=32768` convention, restoring on-disk size
//! parity with other PHD2 tooling. The previous `fitrs`-based path
//! widened to BITPIX=32 because `fitrs` did not support u16.

use std::path::Path;

use base64::Engine;
use rp_fits::atomic::write_atomic_with;
use rp_fits::writer::{write_u16_image, Keyword, KeywordValue};
use rp_fits::FitsError;
use tracing::debug;

use crate::error::{Phd2Error, Result};

/// Decode base64-encoded image data to u16 pixel values.
///
/// PHD2 returns image data as base64-encoded bytes where each pixel
/// is a 16-bit unsigned integer in little-endian format.
///
/// # Errors
///
/// Returns [`Phd2Error::InvalidState`] when the data is not valid base64
/// or decodes to an odd number of bytes.
pub fn decode_base64_u16(base64_data: &str) -> Result<Vec<u16>> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .map_err(|e| Phd2Error::InvalidState(format!("Invalid base64 data: {e}")))?;

    if bytes.len() % 2 != 0 {
        return Err(Phd2Error::InvalidState(format!(
            "Invalid pixel data: byte count {} is not a multiple of 2",
            bytes.len()
        )));
    }

    let pixels: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&chunk| u16::from_le_bytes(chunk))
        .collect();

    debug!("Decoded {} pixels from base64 data", pixels.len());
    Ok(pixels)
}

/// Write a 16-bit grayscale image to a FITS file (BITPIX=16 +
/// BZERO=32768). Atomic: stages, fsyncs, renames, fsyncs the parent
/// dir.
///
/// # Arguments
/// * `path` - Path where the FITS file will be written
/// * `pixels` - The pixel data as u16 values in row-major order
/// * `width` - Image width in pixels
/// * `height` - Image height in pixels
/// * `headers` - Optional additional FITS headers (string-valued)
///
/// # Errors
///
/// Fails when a header keyword is invalid, when staging, writing, or
/// renaming the FITS file fails, or when the blocking write task cannot
/// be joined.
pub async fn write_grayscale_u16_fits<P: AsRef<Path>>(
    path: P,
    pixels: &[u16],
    width: usize,
    height: usize,
    headers: Option<&[(&str, &str)]>,
) -> Result<()> {
    let path = path.as_ref().to_path_buf();
    let pixels = pixels.to_vec();
    let owned_headers: Option<Vec<(String, String)>> = headers.map(|h| {
        h.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    });

    debug!(
        "Writing {}x{} FITS image to {}",
        width,
        height,
        path.display()
    );

    tokio::task::spawn_blocking(move || -> Result<()> {
        let keywords: Vec<Keyword> = match owned_headers.as_deref() {
            Some(hs) => hs
                .iter()
                .map(|(k, v)| {
                    Keyword::new(k, KeywordValue::Str(v.clone()))
                        .map_err(|e| Phd2Error::InvalidState(format!("Invalid FITS header: {e}")))
                })
                .collect::<Result<_>>()?,
            None => Vec::new(),
        };
        write_atomic_with(&path, |w| {
            write_u16_image(w, &pixels, width, height, &keywords)
        })
        .map_err(translate_fits_error)?;
        debug!("FITS file written successfully to {}", path.display());
        Ok(())
    })
    .await
    .map_err(|e| Phd2Error::InvalidState(format!("Task join error: {e}")))?
}

/// Map an [`rp_fits::FitsError`] to the right [`Phd2Error`] variant.
///
/// `FitsError::Io` carries a `std::io::Error` and is reported as
/// `Phd2Error::Io` so callers can distinguish disk failures from
/// logical misuse. Everything else (header validation, dimension
/// mismatch, malformed header) maps to `InvalidState` since those
/// represent caller errors, not I/O failures.
fn translate_fits_error(err: FitsError) -> Phd2Error {
    match err {
        FitsError::Io(io_err) => Phd2Error::Io(io_err),
        other => Phd2Error::InvalidState(format!("Failed to write FITS file: {other}")),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    use base64::Engine;

    #[test]
    fn test_decode_base64_u16_valid() {
        let bytes = vec![0x01, 0x00, 0x02, 0x00, 0x03, 0x00, 0x04, 0x00];
        let base64_data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let pixels = decode_base64_u16(&base64_data).unwrap();
        assert_eq!(pixels, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_decode_base64_u16_empty() {
        let base64_data = base64::engine::general_purpose::STANDARD.encode([]);
        let pixels = decode_base64_u16(&base64_data).unwrap();
        assert_eq!(pixels, Vec::<u16>::new());
    }

    #[test]
    fn test_decode_base64_u16_invalid_base64() {
        let result = decode_base64_u16("not valid base64!!!");
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_base64_u16_odd_bytes() {
        let bytes = vec![0x01, 0x00, 0x02];
        let base64_data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        assert!(decode_base64_u16(&base64_data).is_err());
    }

    #[test]
    fn test_decode_base64_u16_max_values() {
        let bytes = vec![0xFF, 0xFF, 0x00, 0x00];
        let base64_data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let pixels = decode_base64_u16(&base64_data).unwrap();
        assert_eq!(pixels, vec![65535, 0]);
    }

    #[tokio::test]
    async fn test_write_grayscale_u16_fits_dimension_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let pixels = vec![1u16, 2, 3, 4];
        let result =
            write_grayscale_u16_fits(dir.path().join("test.fits"), &pixels, 2, 3, None).await;
        assert!(result.is_err());
    }

    /// Dimensions whose product no buffer could hold are rejected
    /// rather than wrapped into a plausible pixel count.
    #[tokio::test]
    async fn write_grayscale_u16_fits_rejects_overflowing_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.fits");
        let err = write_grayscale_u16_fits(&path, &[], usize::MAX, usize::MAX, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("dimensions overflow"),
            "expected an overflow error, got: {err}"
        );
        assert!(!path.exists(), "a rejected write must leave no file behind");
    }

    #[tokio::test]
    async fn test_write_grayscale_u16_fits_success() {
        let pixels = vec![1u16, 2, 3, 4];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_fits_write.fits");

        write_grayscale_u16_fits(&path, &pixels, 2, 2, None)
            .await
            .unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn test_write_grayscale_u16_fits_with_headers() {
        let pixels = vec![100u16, 200, 300, 400, 500, 600, 700, 800, 900];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_fits_headers.fits");

        let headers = vec![("TELESCOP", "Test Telescope"), ("OBSERVER", "Test User")];
        write_grayscale_u16_fits(&path, &pixels, 3, 3, Some(&headers))
            .await
            .unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn test_write_grayscale_u16_fits_single_pixel() {
        let pixels = vec![42u16];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_fits_single.fits");
        write_grayscale_u16_fits(&path, &pixels, 1, 1, None)
            .await
            .unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn test_write_grayscale_u16_fits_larger_image() {
        let pixels: Vec<u16> = (0..1024).map(|i| i as u16).collect();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_fits_large.fits");
        write_grayscale_u16_fits(&path, &pixels, 32, 32, None)
            .await
            .unwrap();
        assert!(path.exists());
    }

    /// Restoring native u16 (BITPIX=16+BZERO=32768) is the headline
    /// outcome of ADR-001 Amendment A for phd2-guider. Verify the
    /// on-disk file matches that encoding by reading it back.
    #[tokio::test]
    async fn writes_native_u16_with_bzero_convention() {
        use rp_fits::reader::read_primary;
        use rp_fits::reader::Pixels;

        let pixels = vec![0u16, 32768, 65535, 12345];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native_u16.fits");
        write_grayscale_u16_fits(&path, &pixels, 2, 2, None)
            .await
            .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let img = read_primary(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(img.bzero, 32768.0);
        assert_eq!(img.bscale, 1.0);
        let raw = match img.data {
            Pixels::I16(v) => v,
            other => panic!("expected on-disk i16 storage, got {other:?}"),
        };
        let recovered: Vec<u16> = raw.iter().map(|r| (i32::from(*r) + 32768) as u16).collect();
        assert_eq!(recovered, pixels);
    }

    #[test]
    fn test_decode_base64_u16_typical_star_image() {
        let pixels: Vec<u16> = vec![
            100, 100, 100, 100, 100, 200, 500, 200, 100, 500, 1000, 500, 100, 200, 500, 200, 100,
            100, 100, 100,
        ];
        let bytes: Vec<u8> = pixels.iter().flat_map(|&p| p.to_le_bytes()).collect();
        let base64_data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let decoded = decode_base64_u16(&base64_data).unwrap();
        assert_eq!(decoded, pixels);
    }

    #[test]
    fn test_decode_base64_u16_all_zeros() {
        let bytes = vec![0u8; 8];
        let base64_data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let pixels = decode_base64_u16(&base64_data).unwrap();
        assert_eq!(pixels, vec![0, 0, 0, 0]);
    }

    #[test]
    fn test_decode_base64_u16_alternating() {
        let bytes = vec![0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF];
        let base64_data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let pixels = decode_base64_u16(&base64_data).unwrap();
        assert_eq!(pixels, vec![0, 65535, 0, 65535]);
    }
}
