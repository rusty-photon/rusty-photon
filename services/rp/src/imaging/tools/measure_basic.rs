//! `measure_basic` MVP: compose background estimation, star detection, and
//! aggregate HFR into a single result the MCP tool returns and persists.
//!
//! Pure logic — no I/O, no MCP types. The MCP wrapper in `mcp.rs` resolves
//! pixels (cache hit or FITS read) and serializes the result; this module
//! only computes.

use ndarray::ArrayView2;
use serde::Serialize;

use crate::error::{Result, RpError};
use crate::imaging::analysis::background::estimate_background;
use crate::imaging::analysis::hfr::aggregate_hfr;
use crate::imaging::analysis::pixel::Pixel;
use crate::imaging::analysis::stars::{detect_stars, DetectionParams};

/// Result of the basic image measurement. Field names match the JSON
/// contract returned by the MCP tool — change only with intent.
#[derive(Debug, Clone, Serialize)]
pub struct MeasureBasicResult {
    /// Median of per-star HFRs in pixels. `None` ⇒ JSON `null` when no
    /// stars are detected.
    pub hfr: Option<f64>,
    pub star_count: u32,
    /// Number of detected stars containing at least one saturated pixel.
    /// Always `0` when `max_adu` is unknown (e.g. bare `image_path` mode).
    pub saturated_star_count: u32,
    pub background_mean: f64,
    pub background_stddev: f64,
    pub pixel_count: u64,
}

/// Run the `measure_basic` pipeline. `max_adu` is for saturation flagging
/// only; `None` skips the flag (see contract in `docs/services/rp.md`).
pub fn measure_basic<T: Pixel>(
    view: &ArrayView2<T>,
    threshold_sigma: f64,
    min_area: usize,
    max_area: usize,
    max_adu: Option<u32>,
) -> Result<MeasureBasicResult> {
    let (rows, cols) = view.dim();
    let pixel_count = u64::try_from(rows)
        .unwrap_or(u64::MAX)
        .saturating_mul(u64::try_from(cols).unwrap_or(u64::MAX));

    let background = estimate_background(view)
        .ok_or_else(|| RpError::Imaging("background estimation failed".to_string()))?;

    let params = DetectionParams {
        threshold_sigma,
        smoothing_sigma: 1.0,
        min_area,
        max_area,
        max_adu,
    };
    let stars = detect_stars(view, &background, &params);
    let star_count = u32::try_from(stars.len()).unwrap_or(u32::MAX);
    let saturated_star_count =
        u32::try_from(stars.iter().filter(|s| s.saturated_pixel_count > 0).count())
            .unwrap_or(u32::MAX);

    let hfr = aggregate_hfr(view, &stars, background.mean);

    Ok(MeasureBasicResult {
        hfr,
        star_count,
        saturated_star_count,
        background_mean: background.mean,
        background_stddev: background.stddev,
        pixel_count,
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use ndarray::Array2;
    use std::f64::consts::E;

    /// Synthetic frame: Gaussian PSF over a deterministically-dithered
    /// background. The dither gives `estimate_background` a non-zero stddev
    /// so the detection threshold is meaningful (real cameras always have
    /// shot + read noise; flat synthetic backgrounds are a test artifact).
    fn make_gaussian(
        rows: usize,
        cols: usize,
        cx: f64,
        cy: f64,
        sigma: f64,
        peak: f64,
        bg: f64,
    ) -> Array2<u16> {
        let mut arr = Array2::<u16>::zeros((rows, cols));
        for r in 0..rows {
            for c in 0..cols {
                let dx = r as f64 - cx;
                let dy = c as f64 - cy;
                let star_v = peak * E.powf(-(dx * dx + dy * dy) / (2.0 * sigma * sigma));
                // ±2 dither, alternating in a checkerboard. stddev = 2.
                let dither = if (r + c) % 2 == 0 { -2.0 } else { 2.0 };
                let v = bg + dither + star_v;
                arr[[r, c]] = v.round().clamp(0.0, 65535.0) as u16;
            }
        }
        arr
    }

    #[test]
    fn blank_field_yields_zero_stars_with_populated_background() {
        // Truly flat — no dither, no star. estimate_background returns
        // mean=1000, stddev=0; threshold = 1000 exactly. Verify that no
        // bogus star is reported.
        let arr: Array2<u16> = Array2::from_elem((64, 64), 1000);
        let r = measure_basic(&arr.view(), 5.0, 5, 200, Some(65535)).unwrap();
        assert_eq!(r.star_count, 0);
        assert!(r.hfr.is_none());
        assert_eq!(r.saturated_star_count, 0);
        assert!((r.background_mean - 1000.0).abs() < 1e-9);
        assert!(r.background_stddev < 1e-6);
        assert_eq!(r.pixel_count, 64 * 64);
    }

    #[test]
    fn one_star_yields_count_one_and_finite_hfr() {
        // Generous max_area: synthetic flat-background images produce
        // sigma-clipped stddev ≈ 0, which collapses the threshold to background
        // and broadens the above-threshold region. Real images have shot noise.
        let arr = make_gaussian(64, 64, 32.5, 32.5, 1.5, 20_000.0, 1000.0);
        let r = measure_basic(&arr.view(), 5.0, 5, 4096, Some(65535)).unwrap();
        assert_eq!(r.star_count, 1);
        let hfr = r.hfr.expect("hfr should be Some");
        assert!(hfr.is_finite() && hfr > 0.0, "hfr = {hfr}");
        assert_eq!(r.pixel_count, 64 * 64);
    }

    #[test]
    fn very_high_threshold_yields_zero_stars() {
        // Modest peak (~100 ADU above bg with 2-ADU dither stddev) is well
        // above 5σ but well below 1000σ. The latter rejects any detection,
        // mirroring the BDD scenario's empty-stars expected outcome.
        let arr = make_gaussian(64, 64, 32.5, 32.5, 1.5, 100.0, 1000.0);
        let r = measure_basic(&arr.view(), 1000.0, 5, 4096, Some(65535)).unwrap();
        assert_eq!(r.star_count, 0);
        assert!(r.hfr.is_none());
        assert!((r.background_mean - 1000.0).abs() < 100.0);
    }

    #[test]
    fn json_field_names_match_contract() {
        let arr: Array2<u16> = Array2::from_elem((10, 10), 1000);
        let r = measure_basic(&arr.view(), 5.0, 5, 200, None).unwrap();
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("hfr").is_some());
        assert!(v.get("star_count").is_some());
        assert!(v.get("saturated_star_count").is_some());
        assert!(v.get("background_mean").is_some());
        assert!(v.get("background_stddev").is_some());
        assert!(v.get("pixel_count").is_some());
        assert!(v["hfr"].is_null());
    }

    #[test]
    fn no_max_adu_means_zero_saturated_stars() {
        let mut arr = make_gaussian(64, 64, 32.5, 32.5, 1.5, 200_000.0, 1000.0);
        for r in 30..35 {
            for c in 30..35 {
                arr[[r, c]] = 65535;
            }
        }
        let r = measure_basic(&arr.view(), 5.0, 5, 4096, None).unwrap();
        assert_eq!(r.star_count, 1);
        assert_eq!(r.saturated_star_count, 0);
    }

    /// HDR-amplitude i32 frame: peak > `u16::MAX`. Exercises the I32
    /// monomorphization end-to-end and would catch a stray `as u16` cast in
    /// the generic body (peak would wrap and detection would shift).
    fn make_gaussian_i32(
        rows: usize,
        cols: usize,
        cx: f64,
        cy: f64,
        sigma: f64,
        peak: f64,
        bg: f64,
    ) -> Array2<i32> {
        let mut arr = Array2::<i32>::zeros((rows, cols));
        for r in 0..rows {
            for c in 0..cols {
                let dx = r as f64 - cx;
                let dy = c as f64 - cy;
                let star_v = peak * E.powf(-(dx * dx + dy * dy) / (2.0 * sigma * sigma));
                let dither = if (r + c) % 2 == 0 { -2.0 } else { 2.0 };
                arr[[r, c]] = (bg + dither + star_v).round() as i32;
            }
        }
        arr
    }

    #[test]
    fn one_star_in_hdr_i32_pixels() {
        let arr = make_gaussian_i32(64, 64, 32.5, 32.5, 1.5, 200_000.0, 1000.0);
        let r = measure_basic(&arr.view(), 5.0, 5, 4096, Some(1 << 20)).unwrap();
        assert_eq!(r.star_count, 1);
        let hfr = r.hfr.expect("hfr should be Some");
        assert!(hfr.is_finite() && hfr > 0.0, "hfr = {hfr}");
        assert_eq!(r.pixel_count, 64 * 64);
    }

    #[test]
    fn saturation_is_flagged_when_max_adu_provided() {
        let mut arr = make_gaussian(64, 64, 32.5, 32.5, 1.5, 200_000.0, 1000.0);
        for r in 30..35 {
            for c in 30..35 {
                arr[[r, c]] = 65535;
            }
        }
        let r = measure_basic(&arr.view(), 5.0, 5, 4096, Some(65535)).unwrap();
        assert_eq!(r.star_count, 1);
        assert_eq!(r.saturated_star_count, 1);
    }
}
