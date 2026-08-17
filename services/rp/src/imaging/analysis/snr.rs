//! `compute_snr`: signal-to-noise summary based on per-star photometry.
//!
//! Composes background estimation → star detection → per-star SNR via the
//! CCD-equation approximation, then aggregates with the median. Pure
//! logic — no I/O, no MCP types.
//!
//! Per-star noise model:
//!
//! ```text
//! noise = sqrt(signal + n_pixels · σ_bg²)
//! snr   = signal / noise
//! ```
//!
//! where `signal` is the background-subtracted total flux of the
//! component, `n_pixels` is the area of the component, and `σ_bg` is the
//! sigma-clipped background standard deviation. This is the standard CCD
//! equation with the dark-current and read-noise terms collapsed into the
//! background variance, and the gain implicitly set to 1 ADU/electron —
//! good enough for relative quality screening across frames from the
//! *same* camera, **not** an absolute photometric SNR.
//!
//! When no stars are detected, the aggregate SNR/signal/noise are all
//! `None` (the JSON tool layer maps this to `null`). The caller decides
//! whether that's a failure for their workflow.

use ndarray::ArrayView2;
use serde::Serialize;

use super::background::estimate_background;
use super::pixel::Pixel;
use super::stars::{detect_stars, DetectionParams, Star};
use crate::error::{Result, RpError};

/// Aggregate result of the `compute_snr` pipeline.
#[derive(Debug, Clone, Serialize)]
pub struct SnrResult {
    /// Median per-star SNR. `None` when no stars are detected.
    pub snr: Option<f64>,
    /// Median per-star signal (background-subtracted total flux, ADU).
    /// `None` when no stars are detected.
    pub signal: Option<f64>,
    /// Median per-star noise (ADU). `None` when no stars are detected.
    pub noise: Option<f64>,
    pub star_count: u32,
    pub background_mean: f64,
    pub background_stddev: f64,
}

/// Per-star SNR triple: `(signal, noise, snr)`. Returns `None` when the
/// noise term is zero (no signal and a perfectly flat background — only
/// happens on synthetic test inputs).
#[must_use]
pub fn per_star_snr(star: &Star, background_stddev: f64) -> Option<(f64, f64, f64)> {
    let signal = star.total_flux;
    // Per-star pixel counts fit u32 for any in-memory image; `?` folds
    // the impossible overflow into the existing no-SNR result.
    let n = f64::from(u32::try_from(star.pixels.len()).ok()?);
    let variance = signal.max(0.0) + n * background_stddev * background_stddev;
    if variance <= 0.0 {
        return None;
    }
    let noise = variance.sqrt();
    Some((signal, noise, signal / noise))
}

/// Run the SNR pipeline. `max_adu` is for saturation flagging inside
/// `detect_stars`; this tool does not gate on it.
pub fn compute_snr<T: Pixel>(
    view: &ArrayView2<T>,
    threshold_sigma: f64,
    min_area: usize,
    max_area: usize,
    max_adu: Option<u32>,
) -> Result<SnrResult> {
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
    let star_count = u32::try_from(stars.len()).unwrap_or(u32::MAX);

    let mut signals: Vec<f64> = Vec::with_capacity(stars.len());
    let mut noises: Vec<f64> = Vec::with_capacity(stars.len());
    let mut snrs: Vec<f64> = Vec::with_capacity(stars.len());
    for star in &stars {
        if let Some((s, n, r)) = per_star_snr(star, background.stddev) {
            signals.push(s);
            noises.push(n);
            snrs.push(r);
        }
    }

    Ok(SnrResult {
        snr: median_of(snrs),
        signal: median_of(signals),
        noise: median_of(noises),
        star_count,
        background_mean: background.mean,
        background_stddev: background.stddev,
    })
}

fn median_of(mut values: Vec<f64>) -> Option<f64> {
    values.retain(|v| v.is_finite());
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
    fn per_star_snr_matches_ccd_equation() {
        let star = Star {
            centroid_x: 0.0,
            centroid_y: 0.0,
            total_flux: 10_000.0,
            peak: 5_000.0,
            pixels: (0..25).map(|i| (i / 5, i % 5)).collect(),
            bounding_box: (0, 0, 4, 4),
            saturated_pixel_count: 0,
        };
        let bg_stddev = 10.0;
        let (signal, noise, snr) = per_star_snr(&star, bg_stddev).unwrap();
        // signal = 10_000; noise = sqrt(10_000 + 25 · 100) = sqrt(12_500) ≈ 111.8
        assert_eq!(signal, 10_000.0);
        assert!(
            (noise - 111.803_398_874_989_5).abs() < 1e-6,
            "noise = {noise}"
        );
        assert!((snr - 89.4427).abs() < 0.01, "snr = {snr}");
    }

    #[test]
    fn one_star_yields_finite_snr() {
        let arr = make_gaussian_with_dither(64, 64, 32.0, 32.0, 2.0, 20_000.0, 1000.0);
        let r = compute_snr(&arr.view(), 5.0, 5, 4096, Some(65535)).unwrap();
        assert_eq!(r.star_count, 1);
        let snr = r.snr.expect("snr should be Some");
        let signal = r.signal.expect("signal should be Some");
        let noise = r.noise.expect("noise should be Some");
        assert!(snr > 0.0 && snr.is_finite(), "snr = {snr}");
        assert!(signal > 0.0, "signal = {signal}");
        assert!(noise > 0.0, "noise = {noise}");
        // Sanity: signal / noise should reproduce snr (medians line up since
        // there's only one star).
        assert!((snr - signal / noise).abs() < 1e-6);
    }

    #[test]
    fn aggregates_via_median_across_two_stars() {
        let mut arr = make_gaussian_with_dither(96, 96, 24.0, 24.0, 2.0, 20_000.0, 1000.0);
        let arr2 = make_gaussian_with_dither(96, 96, 72.0, 72.0, 2.0, 30_000.0, 0.0);
        for r in 0..96 {
            for c in 0..96 {
                arr[[r, c]] = arr[[r, c]].saturating_add(arr2[[r, c]]);
            }
        }
        let r = compute_snr(&arr.view(), 5.0, 5, 4096, Some(65535)).unwrap();
        assert_eq!(r.star_count, 2);
        assert!(r.snr.is_some());
        assert!(r.signal.is_some());
        assert!(r.noise.is_some());
    }

    #[test]
    fn no_stars_yields_null_aggregates() {
        let arr: Array2<u16> = Array2::from_elem((32, 32), 1000);
        let r = compute_snr(&arr.view(), 5.0, 5, 200, None).unwrap();
        assert_eq!(r.star_count, 0);
        assert!(r.snr.is_none());
        assert!(r.signal.is_none());
        assert!(r.noise.is_none());
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
    fn one_star_in_hdr_i32_yields_finite_snr() {
        let arr = make_gaussian_i32_with_dither(64, 64, 32.0, 32.0, 2.0, 200_000.0, 1000.0);
        let r = compute_snr(&arr.view(), 5.0, 5, 4096, Some(1 << 20)).unwrap();
        assert_eq!(r.star_count, 1);
        let snr = r.snr.expect("snr should be Some");
        let signal = r.signal.expect("signal should be Some");
        let noise = r.noise.expect("noise should be Some");
        assert!(snr > 0.0 && snr.is_finite(), "snr = {snr}");
        // HDR signal must exceed u16::MAX — a stray `as u16` cast would clamp.
        assert!(
            signal > 65_535.0,
            "signal = {signal} (expected HDR > u16::MAX)"
        );
        assert!(noise > 0.0);
    }

    #[test]
    fn json_field_names_match_contract() {
        let arr: Array2<u16> = Array2::from_elem((10, 10), 1000);
        let r = compute_snr(&arr.view(), 5.0, 5, 64, None).unwrap();
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("snr").is_some());
        assert!(v.get("signal").is_some());
        assert!(v.get("noise").is_some());
        assert!(v.get("star_count").is_some());
        assert!(v.get("background_mean").is_some());
        assert!(v.get("background_stddev").is_some());
        assert!(v["snr"].is_null());
    }
}
