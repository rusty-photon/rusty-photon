//! Renders the most recent captured FITS frame as an 8-bit grayscale
//! PNG for `GET /preview.png` — presentation only, so the P5 UI can
//! draw the star/target overlay over the actual sky. Nothing here
//! feeds the alignment math; star *analysis* stays in rp's
//! `detect_stars`.
//!
//! The pipeline is deliberately simple: stride-subsample to the
//! requested width, linear percentile stretch computed on the preview
//! pixels, encode grayscale PNG. Downscaling never affects overlay
//! accuracy — `/status` speaks native pixel coordinates and the UI
//! scales the bitmap under its viewBox.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Default preview width when `?width=` is absent.
pub const DEFAULT_PREVIEW_WIDTH: u32 = 1024;

/// Requests below this width render at it (a smaller preview has no
/// UI value and invites accidental `width=1` requests). Typed as the
/// frame geometry it is compared against, not as the request.
const MIN_PREVIEW_WIDTH: usize = 64;

/// The linear stretch spans these percentiles of the preview pixels:
/// the low cut swallows outlier-dark pixels, the high cut keeps star
/// cores from crushing the sky background to black.
const STRETCH_LOW_PERCENTILE: f64 = 0.005;
const STRETCH_HIGH_PERCENTILE: f64 = 0.999;

/// Why a preview could not be rendered. `NoFrameOnDisk` maps to 404
/// (rp owns the capture directory and may prune it); `Unreadable` is
/// a real failure and maps to 500.
#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    #[error("the captured frame no longer exists on disk: {0}")]
    NoFrameOnDisk(String),
    #[error("failed to render the preview: {0}")]
    Unreadable(String),
}

/// Render the FITS primary HDU at `path` as a grayscale PNG of at
/// most `width` pixels across (clamped to [64, native width]).
pub fn render_png(path: &Path, width: u32) -> Result<Vec<u8>, PreviewError> {
    let file = File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            PreviewError::NoFrameOnDisk(path.display().to_string())
        } else {
            PreviewError::Unreadable(format!("open {}: {e}", path.display()))
        }
    })?;
    let (pixels, native_w, native_h) =
        rp_fits::reader::read_primary_as_i32(BufReader::new(file))
            .map_err(|e| PreviewError::Unreadable(format!("read {}: {e}", path.display())))?;
    render_pixels_png(&pixels, native_w, native_h, width)
}

/// The pure half of [`render_png`], unit-testable without a file.
///
/// The native geometry indexes `pixels`, so it is `usize`; the request
/// and the rendered size are PNG header fields, so they are `u32`. The
/// subsampling in between is buffer arithmetic and stays in `usize`.
fn render_pixels_png(
    pixels: &[i32],
    native_w: usize,
    native_h: usize,
    width: u32,
) -> Result<Vec<u8>, PreviewError> {
    if native_w == 0 || native_h == 0 || native_w.checked_mul(native_h) != Some(pixels.len()) {
        return Err(PreviewError::Unreadable(format!(
            "frame geometry {native_w}×{native_h} does not match {} pixels",
            pixels.len()
        )));
    }

    // A request wider than `usize` can hold clamps to the frame width
    // on the next line anyway, so saturating here is the exact answer
    // rather than a fallback.
    let requested = usize::try_from(width).unwrap_or(usize::MAX);
    let width = requested.min(native_w).max(MIN_PREVIEW_WIDTH.min(native_w));
    let stride = native_w.div_ceil(width);
    let out_w = native_w.div_ceil(stride);
    let out_h = native_h.div_ceil(stride);

    // The product cannot saturate: out_w·out_h ≤ native_w·native_h, which
    // the guard above proved equals `pixels.len()`.
    let mut sampled = Vec::with_capacity(out_w.saturating_mul(out_h));
    for row in pixels.chunks(native_w).step_by(stride) {
        sampled.extend(row.iter().copied().step_by(stride));
    }

    let mut sorted = sampled.clone();
    sorted.sort_unstable();
    // `sampled` holds at least one pixel (the guard rejects an empty
    // frame), so a missing percentile is a broken invariant, reported
    // through the error this function already returns.
    let percentile = |p: f64| -> Result<i32, PreviewError> {
        sorted
            .get(stretch::percentile_index(sorted.len(), p))
            .copied()
            .ok_or_else(|| PreviewError::Unreadable("empty preview sample".to_string()))
    };
    let low = percentile(STRETCH_LOW_PERCENTILE)?;
    let high = percentile(STRETCH_HIGH_PERCENTILE)?;

    let bytes: Vec<u8> = if high > low {
        // Widen before subtracting: pixels can span the full i32
        // range (the reader saturates to it), where `high - low`
        // overflows i32.
        let low = i64::from(low);
        let range = stretch::range(low, i64::from(high));
        sampled
            .iter()
            .map(|&v| stretch::to_u8(v, low, range))
            .collect()
    } else {
        // A constant frame (cover closed, test pattern) renders
        // mid-gray instead of dividing by zero.
        vec![128; sampled.len()]
    };

    let mut out = Vec::new();
    {
        // The PNG header carries fixed-width dimensions, so this is
        // where the buffer geometry stops being a length. Subsampling
        // only ever shrinks the frame, so a frame that fit in memory
        // encodes — but the size is the format's to bound, not ours.
        let (Ok(png_w), Ok(png_h)) = (u32::try_from(out_w), u32::try_from(out_h)) else {
            return Err(PreviewError::Unreadable(format!(
                "rendered size {out_w}×{out_h} exceeds what a PNG header can hold"
            )));
        };
        let mut encoder = png::Encoder::new(&mut out, png_w, png_h);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| PreviewError::Unreadable(format!("png header: {e}")))?;
        writer
            .write_image_data(&bytes)
            .map_err(|e| PreviewError::Unreadable(format!("png data: {e}")))?;
    }
    Ok(out)
}

/// The stretch's float↔integer seam, named and bounded in one place.
/// Pixels arrive as `i32`, so every widening below spans at most 2³² —
/// exact in `f64` (< 2⁵³) — and both narrowing casts land after a
/// round or clamp that bounds them; float→int `as` has saturated since
/// Rust 1.45, and no fallible `f64` conversion API exists to spell any
/// of this otherwise.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "widenings are exact below 2^53; narrowings are round/clamp-bounded and `as` saturates"
)]
mod stretch {
    /// Index of the `p`-th percentile in a sorted slice of `len`
    /// elements: `round((len − 1) · p)`, in bounds for `p` in [0, 1].
    pub(super) fn percentile_index(len: usize, p: f64) -> usize {
        (len.saturating_sub(1) as f64 * p).round() as usize
    }

    /// The stretch span `high − low` as the divisor the mapping needs.
    pub(super) const fn range(low: i64, high: i64) -> f64 {
        high.saturating_sub(low) as f64
    }

    /// Linear map of `v` from `[low, low + range]` onto 0..=255.
    pub(super) fn to_u8(v: i32, low: i64, range: f64) -> u8 {
        ((i64::from(v).saturating_sub(low) as f64 / range) * 255.0).clamp(0.0, 255.0) as u8
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
mod tests {
    use super::*;

    fn decode(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().expect("frame fits in memory")];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!(info.color_type, png::ColorType::Grayscale);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        buf.truncate(info.buffer_size());
        (info.width, info.height, buf)
    }

    /// A gradient frame keeps its ordering through the stretch and
    /// spans the full output range.
    #[test]
    fn test_gradient_renders_full_range_at_native_size() {
        let pixels: Vec<i32> = (0..64 * 64).map(|i| i % 4096).collect();
        let png_bytes = render_pixels_png(&pixels, 64, 64, 64).unwrap();
        let (w, h, gray) = decode(&png_bytes);
        assert_eq!((w, h), (64, 64));
        assert_eq!(*gray.iter().min().unwrap(), 0);
        assert_eq!(*gray.iter().max().unwrap(), 255);
    }

    #[test]
    fn test_width_request_downsamples_by_integer_stride() {
        let pixels: Vec<i32> = (0..200 * 100).map(|i| i % 1000).collect();
        // Requesting 90 of 200 gives stride 3 → 67×34.
        let png_bytes = render_pixels_png(&pixels, 200, 100, 90).unwrap();
        let (w, h, _) = decode(&png_bytes);
        assert_eq!((w, h), (67, 34));
    }

    #[test]
    fn test_width_is_clamped_to_the_native_and_minimum_bounds() {
        let pixels: Vec<i32> = (0..128 * 96).map(|i| i % 500).collect();
        // Larger than native → native.
        let (w, _, _) = decode(&render_pixels_png(&pixels, 128, 96, 4096).unwrap());
        assert_eq!(w, 128);
        // Absurdly small → the 64-px floor.
        let (w, _, _) = decode(&render_pixels_png(&pixels, 128, 96, 1).unwrap());
        assert_eq!(w, 64);
    }

    /// The reader saturates pixels to the full i32 range; the
    /// stretch must widen before subtracting or `high - low`
    /// overflows.
    #[test]
    fn test_extreme_pixel_range_does_not_overflow_the_stretch() {
        let mut pixels = vec![i32::MIN; 32 * 64];
        pixels.extend(vec![i32::MAX; 32 * 64]);
        let (_, _, gray) = decode(&render_pixels_png(&pixels, 64, 64, 64).unwrap());
        assert_eq!(*gray.iter().min().unwrap(), 0);
        assert_eq!(*gray.iter().max().unwrap(), 255);
    }

    #[test]
    fn test_constant_frame_renders_mid_gray() {
        let pixels = vec![7000; 64 * 64];
        let (_, _, gray) = decode(&render_pixels_png(&pixels, 64, 64, 64).unwrap());
        assert!(gray.iter().all(|&g| g == 128), "constant frame → mid-gray");
    }

    #[test]
    fn test_geometry_mismatch_is_an_error() {
        let err = render_pixels_png(&[0; 10], 64, 64, 64).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    #[test]
    fn test_missing_file_maps_to_no_frame_on_disk() {
        let err = render_png(Path::new("/nonexistent/pa-preview.fits"), 1024).unwrap_err();
        assert!(matches!(err, PreviewError::NoFrameOnDisk(_)), "{err}");
    }

    /// End-to-end through a real FITS file written with `rp-fits`.
    #[test]
    fn test_fits_round_trip_renders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame.fits");
        let pixels: Vec<u16> = (0..96u32 * 64).map(|i| (i % 9000) as u16).collect();
        let mut file = File::create(&path).unwrap();
        rp_fits::writer::write_u16_image(&mut file, &pixels, 96, 64, &[]).unwrap();
        drop(file);

        let png_bytes = render_png(&path, 96).unwrap();
        let (w, h, gray) = decode(&png_bytes);
        assert_eq!((w, h), (96, 64));
        assert!(gray.iter().any(|&g| g > 200), "stretch reaches the top");
        assert!(gray.iter().any(|&g| g < 50), "stretch reaches the bottom");
    }
}
