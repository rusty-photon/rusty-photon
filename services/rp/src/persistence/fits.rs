//! FITS file I/O for rp's persistence layer.
//!
//! Per ADR-001 Amendment A:
//!
//! - **Writes use the workspace `rp-fits` crate** (no `fitrs`, no
//!   CFITSIO). The on-disk format defaults to BITPIX=16+BZERO=32768
//!   for the common 16-bit sensor case; cameras whose `max_adu`
//!   exceeds `u16::MAX` go through [`write_fits_i32`] instead.
//! - **Reads always return `Vec<i32>`**. `rp_fits::reader` applies
//!   BSCALE/BZERO and saturates floats so imaging code (which feeds
//!   `Array2<i32>` into `measure_basic` etc.) does not need to care
//!   about on-disk pixel type.
//! - **Atomic-write durability** (stage→fsync→rename→fsync-parent)
//!   lives in `rp_fits::atomic`. This file is just the rp-specific
//!   layer that stamps `DOC_ID` and translates errors to `RpError`.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use rp_fits::atomic::write_atomic_with;
use rp_fits::reader::{read_primary_as_i32, read_primary_keyword};
use rp_fits::writer::{write_i32_image, write_u16_image, Keyword, KeywordValue};
use rp_fits::FitsError;
use tracing::debug;

use crate::error::{Result, RpError};

const DOC_ID_KEY: &str = "DOC_ID";

fn doc_id_keyword(doc_id: &str) -> Result<Keyword> {
    Keyword::new(DOC_ID_KEY, KeywordValue::Str(doc_id.to_string()))
        .map_err(|e| RpError::Imaging(format!("invalid DOC_ID keyword: {e}")))
}

/// Write u16 pixel data as a FITS file (BITPIX=16 + BZERO=32768).
///
/// Atomic and durable: stages to a sibling temp file, fsyncs, renames
/// onto `path`, fsyncs the parent dir. `doc_id` is stamped into the
/// primary HDU header as `DOC_ID = '<full-uuid>'`.
///
/// Used by the production capture path for the common 16-bit sensor
/// case (QHY600 and similar). Cameras whose `max_adu` exceeds 65535
/// must use [`write_fits_i32`] instead — see `mcp::capture` for the
/// dispatch.
pub async fn write_fits_u16<P: AsRef<Path>>(
    path: P,
    pixels: &[u16],
    width: usize,
    height: usize,
    doc_id: &str,
) -> Result<()> {
    let path = path.as_ref().to_path_buf();
    let pixels = pixels.to_vec();
    let doc_id = doc_id.to_string();

    debug!(
        width = width,
        height = height,
        path = %path.display(),
        doc_id = %doc_id,
        "writing u16 FITS image"
    );

    tokio::task::spawn_blocking(move || {
        let kw = [doc_id_keyword(&doc_id)?];
        write_atomic_with(&path, |w| write_u16_image(w, &pixels, width, height, &kw))
            .map_err(|e| translate_write_err(&e))
    })
    .await
    .map_err(|e| RpError::Imaging(format!("task join error: {e}")))?
}

/// Write i32 pixel data as a FITS file (BITPIX=32). Used for
/// scientific cameras whose `max_adu` exceeds `u16::MAX`.
pub async fn write_fits_i32<P: AsRef<Path>>(
    path: P,
    pixels: &[i32],
    width: usize,
    height: usize,
    doc_id: &str,
) -> Result<()> {
    let path = path.as_ref().to_path_buf();
    let pixels = pixels.to_vec();
    let doc_id = doc_id.to_string();

    debug!(
        width = width,
        height = height,
        path = %path.display(),
        doc_id = %doc_id,
        "writing i32 FITS image"
    );

    tokio::task::spawn_blocking(move || {
        let kw = [doc_id_keyword(&doc_id)?];
        write_atomic_with(&path, |w| write_i32_image(w, &pixels, width, height, &kw))
            .map_err(|e| translate_write_err(&e))
    })
    .await
    .map_err(|e| RpError::Imaging(format!("task join error: {e}")))?
}

/// Read pixel data from a FITS file, normalised to `i32`.
///
/// Returns `(pixels, width, height)`. The pixel vector is flat
/// row-major; `width` is `NAXIS1` (the fastest-varying axis), `height`
/// is `NAXIS2`. Both are `usize`: every caller uses them to index or
/// shape `pixels`, and the fixed-width `NAXIS` values they came from
/// belong to the header, not to the buffer. The `write_fits_*` pair
/// above takes `usize` for the same reason — a caller holds the
/// geometry of a buffer it already has, and `rp_fits` narrows it to a
/// `NAXIS` card at the point it becomes one.
/// `rp_fits::reader` applies BSCALE/BZERO so the caller
/// sees physical ADU values regardless of on-disk encoding. `BLANK`
/// sentinel pixels are *not* remapped at this layer — they pass
/// through unscaled and may surface as ordinary numeric ADU values.
/// Per ADR-001 Amendment A the wrapper exposes BLANK on the typed
/// `read_primary` path; consumers that need explicit handling should
/// use that entry point instead of `read_fits_pixels`.
pub fn read_fits_pixels<P: AsRef<Path>>(path: P) -> Result<(Vec<i32>, usize, usize)> {
    let path = path.as_ref();
    debug!(path = %path.display(), "reading FITS pixels");
    let file = File::open(path).map_err(|e| {
        RpError::Imaging(format!(
            "failed to open FITS file '{}': {}",
            path.display(),
            e
        ))
    })?;
    read_primary_as_i32(BufReader::new(file)).map_err(|e| {
        RpError::Imaging(format!(
            "failed to parse FITS file '{}': {}",
            path.display(),
            e
        ))
    })
}

/// Read the `DOC_ID` keyword from a FITS file's primary HDU.
///
/// Returns `Ok(Some(uuid))` when the header is present and is a
/// character string, `Ok(None)` when the header is absent (e.g. files
/// written before Phase 7). Surface I/O and FITS-parse failures as
/// `Err`.
pub fn read_fits_doc_id<P: AsRef<Path>>(path: P) -> Result<Option<String>> {
    let path = path.as_ref();
    let file = File::open(path).map_err(|e| {
        RpError::Imaging(format!(
            "failed to open FITS file '{}': {}",
            path.display(),
            e
        ))
    })?;
    match read_primary_keyword(BufReader::new(file), DOC_ID_KEY) {
        Ok(None) => Ok(None),
        Ok(Some(KeywordValue::Str(s))) => Ok(Some(s)),
        Ok(Some(_)) => Err(RpError::Imaging(
            "DOC_ID header has non-string value".to_string(),
        )),
        Err(e) => Err(RpError::Imaging(format!(
            "failed to read DOC_ID from '{}': {}",
            path.display(),
            e
        ))),
    }
}

fn translate_write_err(err: &FitsError) -> RpError {
    // `rp_fits::atomic` surfaces every step's failure as
    // `FitsError::Io`. We can't distinguish a `create_dir_all` failure
    // from an fsync failure from a `persist` (rename) collision via
    // the variant alone, so route everything through a single
    // "failed to write FITS file" prefix. The previous version painted
    // every Io with "failed to persist FITS file" for fitrs-era
    // backwards compat — that's gone now (no external callers depend
    // on the legacy prefix).
    RpError::Imaging(format!("failed to write FITS file: {err}"))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_and_read_u16_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.fits");

        let pixels = vec![100u16, 200, 300, 400];
        write_fits_u16(&path, &pixels, 2, 2, "test-doc")
            .await
            .unwrap();

        assert!(path.exists());

        let (read_back, w, h) = read_fits_pixels(&path).unwrap();
        // u16 values widen losslessly to i32 on read.
        assert_eq!(read_back, vec![100i32, 200, 300, 400]);
        assert_eq!((w, h), (2, 2));
    }

    #[tokio::test]
    async fn write_and_read_i32_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.fits");

        let pixels = vec![100i32, -200, 0, 1_000_000];
        write_fits_i32(&path, &pixels, 2, 2, "test-doc")
            .await
            .unwrap();

        let (read_back, w, h) = read_fits_pixels(&path).unwrap();
        assert_eq!(read_back, pixels);
        assert_eq!((w, h), (2, 2));
    }

    #[tokio::test]
    async fn write_fits_dimension_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.fits");

        let err = write_fits_u16(&path, &[1u16, 2, 3, 4], 2, 3, "test-doc")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("does not match"),
            "unexpected error: {err}"
        );
    }

    /// Dimensions whose product no buffer could hold are rejected
    /// before anything is staged, rather than wrapped into a
    /// plausible pixel count.
    #[tokio::test]
    async fn write_fits_rejects_overflowing_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.fits");

        let err = write_fits_u16(&path, &[], usize::MAX, usize::MAX, "test-doc")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("dimensions overflow"),
            "unexpected error: {err}"
        );

        let err = write_fits_i32(&path, &[], usize::MAX, usize::MAX, "test-doc")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("dimensions overflow"),
            "unexpected error: {err}"
        );
        assert!(!path.exists(), "nothing should have been staged");
    }

    #[test]
    fn read_fits_nonexistent() {
        let err = read_fits_pixels("/nonexistent/path.fits").unwrap_err();
        assert!(
            err.to_string().contains("failed to open FITS"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn doc_id_round_trips_through_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img.fits");
        let doc_id = "550e8400-e29b-41d4-a716-446655440000";

        write_fits_u16(&path, &[1u16, 2, 3, 4], 2, 2, doc_id)
            .await
            .unwrap();

        let read_back = read_fits_doc_id(&path).unwrap();
        assert_eq!(read_back.as_deref(), Some(doc_id));
    }

    #[test]
    fn read_fits_doc_id_nonexistent() {
        let err = read_fits_doc_id("/nonexistent/path.fits").unwrap_err();
        assert!(
            err.to_string().contains("failed to open FITS"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_fits_doc_id_returns_none_when_header_absent() {
        // Files written before Phase 7 lacked DOC_ID. Bypass the
        // public `write_fits_*` API and emit a raw HDU without the
        // keyword to simulate them.
        use rp_fits::writer::write_i32_image as raw_write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.fits");
        let mut file = std::fs::File::create(&path).unwrap();
        raw_write(&mut file, &[1i32, 2, 3, 4], 2, 2, &[]).unwrap();
        drop(file);

        let read_back = read_fits_doc_id(&path).unwrap();
        assert!(
            read_back.is_none(),
            "expected None for FITS without DOC_ID, got {read_back:?}"
        );
    }

    #[tokio::test]
    async fn write_fits_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("dir").join("image.fits");

        write_fits_u16(&path, &[42u16], 1, 1, "test-doc")
            .await
            .unwrap();

        assert!(path.exists());
    }

    fn entry_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn write_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img.fits");

        write_fits_u16(&path, &[1u16, 2, 3, 4], 2, 2, "test-doc")
            .await
            .unwrap();
        write_fits_u16(&path, &[10u16, 20, 30, 40], 2, 2, "test-doc")
            .await
            .unwrap();

        let (pixels, w, h) = read_fits_pixels(&path).unwrap();
        assert_eq!(pixels, vec![10i32, 20, 30, 40]);
        assert_eq!((w, h), (2, 2));
    }

    #[tokio::test]
    async fn successful_write_leaves_no_staging_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img.fits");

        write_fits_u16(&path, &[1u16, 2, 3, 4], 2, 2, "test-doc")
            .await
            .unwrap();

        assert_eq!(
            entry_names(dir.path()),
            vec!["img.fits"],
            "directory should contain only the FITS file after a successful write"
        );
    }

    #[tokio::test]
    async fn failed_write_cleans_up_staging_file() {
        // Force the rename to fail by replacing the destination with a
        // directory — `rename(file, dir)` is rejected on Linux and Windows.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img.fits");
        std::fs::create_dir(&path).unwrap();

        let err = write_fits_u16(&path, &[1u16, 2, 3, 4], 2, 2, "test-doc")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("failed to write FITS file"),
            "unexpected error: {err}"
        );

        assert_eq!(
            entry_names(dir.path()),
            vec!["img.fits"],
            "failed write must not leave a staging file behind (only the directory we put in the way remains)"
        );
    }

    #[tokio::test]
    async fn write_fits_i32_dimension_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.fits");

        let err = write_fits_i32(&path, &[1i32, 2, 3, 4], 2, 3, "test-doc")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("does not match"),
            "unexpected error: {err}"
        );
    }

    /// `read_fits_doc_id` translates a non-string `DOC_ID` header (a file
    /// where `DOC_ID` was somehow written as an integer) into a clean
    /// "non-string value" error. Force the case by writing a raw HDU
    /// with `DOC_ID` as an Int.
    #[test]
    fn read_fits_doc_id_rejects_non_string_value() {
        use rp_fits::writer::{write_i32_image as raw_write, Keyword, KeywordValue};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad_docid.fits");
        let mut file = std::fs::File::create(&path).unwrap();
        let kw = [Keyword::new("DOC_ID", KeywordValue::Int(42)).unwrap()];
        raw_write(&mut file, &[1i32; 4], 2, 2, &kw).unwrap();
        drop(file);

        let err = read_fits_doc_id(&path).unwrap_err();
        assert!(
            err.to_string().contains("non-string value"),
            "unexpected error: {err}"
        );
    }

    /// `read_fits_doc_id` propagates a parse failure from rp-fits.
    /// Force the case with a deliberately-truncated FITS file.
    #[test]
    fn read_fits_doc_id_propagates_parse_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.fits");
        std::fs::write(&path, b"not a fits payload").unwrap();
        let err = read_fits_doc_id(&path).unwrap_err();
        assert!(
            err.to_string().contains("failed to read DOC_ID"),
            "unexpected error: {err}"
        );
    }

    /// Atomic rename means the destination is either the old file or
    /// the new file, never torn or missing. Force a second write to
    /// fail by removing write permission on the parent and confirm
    /// the prior file's contents are intact.
    #[cfg(unix)]
    #[tokio::test]
    async fn failed_write_preserves_prior_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img.fits");

        write_fits_u16(&path, &[1u16, 2, 3, 4], 2, 2, "test-doc")
            .await
            .unwrap();

        let original_perms = std::fs::metadata(dir.path()).unwrap().permissions();
        let mut readonly = original_perms.clone();
        readonly.set_mode(0o555);
        std::fs::set_permissions(dir.path(), readonly).unwrap();

        let err = write_fits_u16(&path, &[9u16, 9, 9, 9], 2, 2, "test-doc")
            .await
            .unwrap_err();

        // Restore so tempdir can clean up regardless of assertion outcomes.
        std::fs::set_permissions(dir.path(), original_perms).unwrap();

        assert!(
            err.to_string().contains("failed to write FITS file"),
            "unexpected error: {err}"
        );

        let (pixels, _, _) = read_fits_pixels(&path).unwrap();
        assert_eq!(
            pixels,
            vec![1i32, 2, 3, 4],
            "the original file must remain intact when a write to the same path fails"
        );
    }
}
