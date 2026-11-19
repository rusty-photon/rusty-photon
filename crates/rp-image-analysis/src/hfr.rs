//! A component's own half-flux radius via radial flux accumulation.
//!
//! For each star: collect `(distance_from_centroid, background_subtracted_flux)`
//! pairs from the component pixels, sort by distance ascending, walk the
//! cumulative-flux curve, and return the radius at which it first crosses
//! half the total flux. Sub-pixel precision via linear interpolation between
//! the two bracketing samples.
//!
//! The component is what the threshold kept, so this radius is capped by
//! it: detection's 0.25 px floor reads it, and the aperture measurement
//! starts from it, but a star's reported HFR is the aperture's
//! ([`super::aperture`]). See `docs/services/rp.md` (`measure_basic`
//! algorithm steps 8–9).

use ndarray::ArrayView2;

use super::pixel::Pixel;
use super::stars::Star;

/// Per-star half-flux radius in pixels. `None` if the star's total
/// background-subtracted flux is non-positive.
#[must_use]
#[expect(
    clippy::suboptimal_flops,
    reason = "prev + t·(dist − prev) is the canonical lerp shape for the half-flux crossing; analysis math feeding autofocus stays unfused"
)]
pub fn star_hfr<T: Pixel>(view: &ArrayView2<T>, star: &Star, background_mean: f64) -> Option<f64> {
    let mut samples: Vec<(f64, f64)> = Vec::with_capacity(star.pixels.len());
    let mut total_flux = 0.0_f64;
    for &(r, c) in &star.pixels {
        // Star pixels come from this view's own component mask; `?`
        // folds the impossible out-of-bounds into the no-HFR result,
        // and the coordinates fit u32 for any in-memory image.
        let f = (view.get((r, c))?.to_f64() - background_mean).max(0.0);
        if f <= 0.0 {
            continue;
        }
        // `(r, c)` is `(y, x)`: rows pair with `centroid_y`, columns
        // with `centroid_x`.
        let dx = f64::from(u32::try_from(c).ok()?) - star.centroid_x;
        let dy = f64::from(u32::try_from(r).ok()?) - star.centroid_y;
        let d = dx.hypot(dy);
        samples.push((d, f));
        total_flux += f;
    }
    if total_flux <= 0.0 || samples.is_empty() {
        return None;
    }

    samples.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let half = total_flux / 2.0;
    let mut cumulative = 0.0_f64;
    let mut prev_dist = 0.0_f64;
    let mut prev_cum = 0.0_f64;
    for (dist, flux) in &samples {
        let next_cum = cumulative + flux;
        if next_cum >= half {
            // Linear interpolation between (prev_dist, prev_cum) and (dist, next_cum).
            let span = next_cum - prev_cum;
            return Some(if span <= 0.0 {
                *dist
            } else {
                let t = (half - prev_cum) / span;
                prev_dist + t * (dist - prev_dist)
            });
        }
        prev_dist = *dist;
        prev_cum = next_cum;
        cumulative = next_cum;
    }

    // Should not be reachable since cumulative ends at total_flux >= half.
    samples.last().map(|(d, _)| *d)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use ndarray::Array2;
    use std::f64::consts::E;

    fn star_from_pixels(pixels: Vec<(usize, usize)>, cx: f64, cy: f64) -> Star {
        let bbox = pixels.iter().fold(
            (usize::MAX, usize::MAX, 0usize, 0usize),
            |(min_x, min_y, max_x, max_y), &(r, c)| {
                (min_x.min(c), min_y.min(r), max_x.max(c), max_y.max(r))
            },
        );
        Star {
            centroid_x: cx,
            centroid_y: cy,
            total_flux: 0.0,
            peak: 0.0,
            pixels,
            bounding_box: bbox,
            saturated_pixel_count: 0,
        }
    }

    #[test]
    fn hfr_zero_flux_returns_none() {
        let arr: Array2<u16> = Array2::from_elem((5, 5), 100);
        let pixels: Vec<(usize, usize)> =
            (0..5).flat_map(|r| (0..5).map(move |c| (r, c))).collect();
        let star = star_from_pixels(pixels, 2.0, 2.0);
        assert!(star_hfr(&arr.view(), &star, 100.0).is_none());
    }

    #[test]
    fn hfr_constant_disc_matches_geometric_expectation() {
        // Uniform disc of radius R: cumulative flux is proportional to area
        // (πr²), so half-flux is at r = R/√2.
        let r_disc = 5_i32;
        let mut arr: Array2<u16> = Array2::from_elem((20, 20), 0);
        let cx = 10_i32;
        let cy = 10_i32;
        let mut pixels = Vec::new();
        for r in 0..20_i32 {
            for c in 0..20_i32 {
                let d2 = (c - cx) * (c - cx) + (r - cy) * (r - cy);
                if d2 <= r_disc * r_disc {
                    arr[[r as usize, c as usize]] = 1000;
                    pixels.push((r as usize, c as usize));
                }
            }
        }
        let star = star_from_pixels(pixels, f64::from(cx), f64::from(cy));
        let hfr = star_hfr(&arr.view(), &star, 0.0).unwrap();
        let expected = f64::from(r_disc) / 2_f64.sqrt();
        assert!(
            (hfr - expected).abs() < 0.5,
            "hfr = {hfr}, expected ≈ {expected}"
        );
    }

    /// A disc centred at column 30, row 8 of a 20-row × 40-column frame:
    /// `centroid_x` pairs with the column index and `centroid_y` with the
    /// row index, so the radial distances — and the HFR — come out right
    /// for a star whose `x` exceeds the row count.
    #[test]
    fn hfr_pairs_centroid_x_with_columns_and_y_with_rows() {
        let r_disc = 4_i32;
        let (cx, cy) = (30_i32, 8_i32);
        let mut arr: Array2<u16> = Array2::from_elem((20, 40), 0);
        let mut pixels = Vec::new();
        for r in 0..20_i32 {
            for c in 0..40_i32 {
                let d2 = (c - cx) * (c - cx) + (r - cy) * (r - cy);
                if d2 <= r_disc * r_disc {
                    arr[[r as usize, c as usize]] = 1000;
                    pixels.push((r as usize, c as usize));
                }
            }
        }
        let star = star_from_pixels(pixels, f64::from(cx), f64::from(cy));
        let hfr = star_hfr(&arr.view(), &star, 0.0).unwrap();
        let expected = f64::from(r_disc) / 2_f64.sqrt();
        assert!(
            (hfr - expected).abs() < 0.5,
            "hfr = {hfr}, expected ≈ {expected}"
        );
    }

    #[test]
    fn hfr_gaussian_psf_is_in_expected_range() {
        // 2D Gaussian with sigma = 2.0. The continuous half-flux radius is
        // ≈ σ × √(2 ln 2) ≈ 2.355σ ≈ 2.355 for σ=2.0... wait that's FWHM/2.
        // Half-flux for a circular Gaussian: 1 - exp(-r²/2σ²) = 0.5 →
        // r = σ√(2 ln 2) ≈ 1.1774 σ ≈ 2.355 for σ=2.0. With pixel quantization
        // and finite sampling tolerance is generous.
        let sigma = 2.0_f64;
        let cx = 10.0_f64;
        let cy = 10.0_f64;
        let mut arr: Array2<u16> = Array2::zeros((20, 20));
        let mut pixels = Vec::new();
        for r in 0..20 {
            for c in 0..20 {
                let dx = c as f64 - cx;
                let dy = r as f64 - cy;
                let v = 10_000.0 * E.powf(-(dx * dx + dy * dy) / (2.0 * sigma * sigma));
                arr[[r, c]] = v.round() as u16;
                if v > 10.0 {
                    pixels.push((r, c));
                }
            }
        }
        let star = star_from_pixels(pixels, cx, cy);
        let hfr = star_hfr(&arr.view(), &star, 0.0).unwrap();
        let expected = sigma * (2.0_f64.ln() * 2.0).sqrt();
        assert!(
            (hfr - expected).abs() < 0.5,
            "hfr = {hfr}, expected ≈ {expected}"
        );
    }

    #[test]
    fn hfr_constant_disc_in_hdr_i32_matches_geometric_expectation() {
        // Same uniform-disc geometry as the u16 test, but with HDR pixel
        // values (>u16::MAX) on i32 to exercise the I32 monomorphization.
        let r_disc = 5_i32;
        let mut arr: Array2<i32> = Array2::from_elem((20, 20), 0);
        let cx = 10_i32;
        let cy = 10_i32;
        let mut pixels = Vec::new();
        for r in 0..20_i32 {
            for c in 0..20_i32 {
                let d2 = (c - cx) * (c - cx) + (r - cy) * (r - cy);
                if d2 <= r_disc * r_disc {
                    arr[[r as usize, c as usize]] = 200_000;
                    pixels.push((r as usize, c as usize));
                }
            }
        }
        let star = star_from_pixels(pixels, f64::from(cx), f64::from(cy));
        let hfr = star_hfr(&arr.view(), &star, 0.0).unwrap();
        let expected = f64::from(r_disc) / 2_f64.sqrt();
        assert!(
            (hfr - expected).abs() < 0.5,
            "hfr = {hfr}, expected ≈ {expected}"
        );
    }
}
