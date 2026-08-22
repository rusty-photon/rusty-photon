//! `measure_stars`: per-star photometry and PSF metrics.
//!
//! Composes background estimation → star detection → empirical HFR → 2D
//! Gaussian fit (FWHM, eccentricity) into a single result. Pure logic — no
//! I/O, no MCP types. The MCP wrapper resolves pixels (cache or FITS) and
//! serializes the result; this module only computes.
//!
//! When the Gaussian fit fails (centroid too close to the edge, divergent
//! solver, degenerate sigma), the row is *kept* with `fwhm: None` and
//! `eccentricity: None`. Dropping failures would lose information the
//! caller needs to decide whether the frame is usable.

use ndarray::ArrayView2;
use serde::Serialize;

use crate::error::{Result, RpError};
use crate::imaging::analysis::background::estimate_background;
use crate::imaging::analysis::fwhm::fit_2d_gaussian;
use crate::imaging::analysis::hfr::star_hfr;
use crate::imaging::analysis::pixel::Pixel;
use crate::imaging::analysis::stars::{detect_stars, DetectionParams};

/// Default postage-stamp half-size (pixels). 8 ⇒ 17×17 stamp, comfortably
/// captures a PSF with σ ≤ 4 px (FWHM ≤ ~9 px).
pub const DEFAULT_STAMP_HALF_SIZE: usize = 8;

/// Per-star measurement. Field names match the JSON contract returned by
/// the MCP tool — change only with intent.
#[derive(Debug, Clone, Serialize)]
pub struct StarMeasurement {
    pub x: f64,
    pub y: f64,
    /// Empirical half-flux radius (pixels). `None` only when the star has
    /// no positive flux above background, which `detect_stars` already
    /// filters out — included for completeness.
    pub hfr: Option<f64>,
    /// Geometric-mean FWHM (pixels) from the 2D Gaussian fit. `None` when
    /// the fit failed.
    pub fwhm: Option<f64>,
    /// PSF eccentricity from the 2D Gaussian fit. `None` when the fit
    /// failed.
    pub eccentricity: Option<f64>,
    /// Sum of background-subtracted, non-negative flux (ADU).
    pub flux: f64,
}

/// Aggregate result of the `measure_stars` pipeline.
#[derive(Debug, Clone, Serialize)]
pub struct MeasureStarsResult {
    pub stars: Vec<StarMeasurement>,
    pub star_count: u32,
    /// Median FWHM across stars whose fit succeeded. `None` when no fits
    /// converged (or no stars were detected).
    pub median_fwhm: Option<f64>,
    /// Median empirical HFR across stars with computable HFR. `None` when
    /// no stars were detected.
    pub median_hfr: Option<f64>,
    pub background_mean: f64,
    pub background_stddev: f64,
}

/// Run the measurement pipeline.
///
/// `max_adu` is for saturation flagging inside `detect_stars`; the
/// per-star saturation count is not surfaced in the result (callers
/// wanting it call `detect_stars` directly).
///
/// # Errors
///
/// Returns [`RpError::Imaging`] if background estimation fails; a frame
/// with no detectable stars is not an error.
pub fn measure_stars<T: Pixel>(
    view: &ArrayView2<T>,
    threshold_sigma: f64,
    min_area: usize,
    max_area: usize,
    max_adu: Option<u32>,
    stamp_half_size: usize,
) -> Result<MeasureStarsResult> {
    let background = estimate_background(view)
        .ok_or_else(|| RpError::Imaging("background estimation failed".to_string()))?;

    let detection = DetectionParams {
        threshold_sigma,
        smoothing_sigma: 1.0,
        min_area,
        max_area,
        max_adu,
    };
    let stars = detect_stars(view, &background, &detection);

    let mut measurements: Vec<StarMeasurement> = Vec::with_capacity(stars.len());
    for star in &stars {
        let hfr = star_hfr(view, star, background.mean);
        let fit = fit_2d_gaussian(
            view,
            star.centroid_x,
            star.centroid_y,
            star.peak,
            background.mean,
            1.5,
            stamp_half_size,
        );
        measurements.push(StarMeasurement {
            x: star.centroid_x,
            y: star.centroid_y,
            hfr,
            fwhm: fit.map(|f| f.fwhm),
            eccentricity: fit.map(|f| f.eccentricity),
            flux: star.total_flux,
        });
    }

    let median_fwhm = median_of(measurements.iter().filter_map(|m| m.fwhm));
    let median_hfr = median_of(measurements.iter().filter_map(|m| m.hfr));
    let star_count = u32::try_from(measurements.len()).unwrap_or(u32::MAX);

    Ok(MeasureStarsResult {
        stars: measurements,
        star_count,
        median_fwhm,
        median_hfr,
        background_mean: background.mean,
        background_stddev: background.stddev,
    })
}

fn median_of<I: IntoIterator<Item = f64>>(iter: I) -> Option<f64> {
    let mut values: Vec<f64> = iter.into_iter().filter(|v| v.is_finite()).collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values.get(mid).copied()
    } else {
        let hi = values.get(mid)?;
        let lo = values.get(mid.checked_sub(1)?)?;
        Some(0.5 * (lo + hi))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use ndarray::Array2;
    use std::f64::consts::E;

    fn make_gaussian_with_dither(
        rows: usize,
        cols: usize,
        cx: f64,
        cy: f64,
        sigma: f64,
        amplitude: f64,
        background: f64,
    ) -> Array2<u16> {
        let mut arr = Array2::<u16>::zeros((rows, cols));
        for r in 0..rows {
            for c in 0..cols {
                let dx = r as f64 - cx;
                let dy = c as f64 - cy;
                let star_v = amplitude * E.powf(-(dx * dx + dy * dy) / (2.0 * sigma * sigma));
                let dither = if (r + c) % 2 == 0 { -2.0 } else { 2.0 };
                let v = background + dither + star_v;
                arr[[r, c]] = v.round().clamp(0.0, 65535.0) as u16;
            }
        }
        arr
    }

    #[test]
    fn one_star_yields_finite_hfr_and_fwhm() {
        let arr = make_gaussian_with_dither(64, 64, 32.0, 32.0, 2.0, 20_000.0, 1000.0);
        let r = measure_stars(&arr.view(), 5.0, 5, 4096, Some(65535), 10).unwrap();
        assert_eq!(r.star_count, 1);
        let s = &r.stars[0];
        let hfr = s.hfr.expect("hfr should be Some");
        let fwhm = s.fwhm.expect("fwhm should be Some");
        let ecc = s.eccentricity.expect("eccentricity should be Some");
        assert!(hfr > 0.0 && hfr.is_finite(), "hfr = {hfr}");
        // Input σ = 2 ⇒ FWHM ≈ 4.71. Allow generous slop for the noisy fit.
        assert!((fwhm - 4.71).abs() < 0.6, "fwhm = {fwhm} (expected ≈ 4.71)");
        assert!(
            ecc < 0.2,
            "circular PSF eccentricity should be ~0, got {ecc}"
        );
    }

    #[test]
    fn aggregates_medians_across_two_stars() {
        let mut arr = make_gaussian_with_dither(96, 96, 24.0, 24.0, 2.0, 20_000.0, 1000.0);
        let arr2 = make_gaussian_with_dither(96, 96, 72.0, 72.0, 2.0, 20_000.0, 0.0);
        for r in 0..96 {
            for c in 0..96 {
                arr[[r, c]] = arr[[r, c]].saturating_add(arr2[[r, c]]);
            }
        }
        let r = measure_stars(&arr.view(), 5.0, 5, 4096, Some(65535), 10).unwrap();
        assert_eq!(r.star_count, 2);
        assert!(r.median_fwhm.is_some());
        assert!(r.median_hfr.is_some());
    }

    #[test]
    fn no_stars_yields_null_aggregates() {
        let arr: Array2<u16> = Array2::from_elem((32, 32), 1000);
        let r = measure_stars(&arr.view(), 5.0, 5, 200, None, 8).unwrap();
        assert_eq!(r.star_count, 0);
        assert!(r.median_fwhm.is_none());
        assert!(r.median_hfr.is_none());
        assert!(r.stars.is_empty());
    }

    #[test]
    fn fit_failure_keeps_row_with_null_fwhm() {
        // Detect a star comfortably inside the frame, then call
        // `measure_stars` with a `stamp_half_size` larger than the image so
        // the postage-stamp bounds check in `fit_2d_gaussian` rejects every
        // candidate. This exercises the documented "fit failure → row kept
        // with null fwhm/eccentricity" path deterministically; trying to
        // engineer a centroid close enough to the edge to fail the stamp
        // check while keeping the component's bbox off the border is
        // brittle (the threshold-cross radius depends on smoothing + noise
        // and easily makes detect_stars reject the component instead).
        let arr = make_gaussian_with_dither(32, 32, 16.0, 16.0, 2.0, 20_000.0, 1000.0);
        let r = measure_stars(&arr.view(), 5.0, 5, 1024, Some(65535), 30).unwrap();
        assert!(
            r.star_count >= 1,
            "test setup must detect at least one star so fit-failure behavior is exercised \
             (got star_count = 0)"
        );
        let s = &r.stars[0];
        assert!(
            s.fwhm.is_none(),
            "expected fit to fail for oversized stamp, got fwhm = {:?}",
            s.fwhm
        );
        assert!(s.eccentricity.is_none());
    }

    fn make_gaussian_i32_with_dither(
        rows: usize,
        cols: usize,
        cx: f64,
        cy: f64,
        sigma: f64,
        amplitude: f64,
        background: f64,
    ) -> Array2<i32> {
        let mut arr = Array2::<i32>::zeros((rows, cols));
        for r in 0..rows {
            for c in 0..cols {
                let dx = r as f64 - cx;
                let dy = c as f64 - cy;
                let star_v = amplitude * E.powf(-(dx * dx + dy * dy) / (2.0 * sigma * sigma));
                let dither = if (r + c) % 2 == 0 { -2.0 } else { 2.0 };
                arr[[r, c]] = (background + dither + star_v).round() as i32;
            }
        }
        arr
    }

    #[test]
    fn one_star_in_hdr_i32_yields_finite_fwhm_and_hfr() {
        // Amplitude 200_000 > u16::MAX exposes a stray `as u16` cast in the
        // i32 path; FWHM ≈ 2.355σ for σ=2 should still recover.
        let arr = make_gaussian_i32_with_dither(64, 64, 32.0, 32.0, 2.0, 200_000.0, 1000.0);
        let r = measure_stars(&arr.view(), 5.0, 5, 4096, Some(1 << 20), 10).unwrap();
        assert_eq!(r.star_count, 1);
        let s = &r.stars[0];
        assert!(s.hfr.expect("hfr").is_finite());
        let fwhm = s.fwhm.expect("fwhm");
        assert!((fwhm - 4.71).abs() < 0.6, "fwhm = {fwhm} (expected ≈ 4.71)");
    }

    #[test]
    fn json_field_names_match_contract() {
        let arr: Array2<u16> = Array2::from_elem((10, 10), 1000);
        let r = measure_stars(&arr.view(), 5.0, 5, 64, None, 8).unwrap();
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("stars").is_some());
        assert!(v.get("star_count").is_some());
        assert!(v.get("median_fwhm").is_some());
        assert!(v.get("median_hfr").is_some());
        assert!(v.get("background_mean").is_some());
        assert!(v.get("background_stddev").is_some());
        assert!(v["median_fwhm"].is_null());
        assert!(v["median_hfr"].is_null());
    }
}
