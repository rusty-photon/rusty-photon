//! Star detection: smoothing → thresholding → connected-components labelling
//! → component filtering → centroiding.
//!
//! Coordinate convention follows the rest of the imaging pipeline: ndarray's
//! first axis is "x" (matches `capture`'s `(width, height)` shape from the
//! ASCOM image array), second axis is "y". `centroid_x` is the
//! flux-weighted mean of the first-axis index; `centroid_y` of the second.
//!
//! Saturated components are *not* rejected — they're flagged via
//! [`Star::saturated_pixel_count`] so downstream consumers (`auto_focus`,
//! quality-screen tools) apply their own policy. See `docs/services/rp.md`
//! (MVP `measure_basic` Contract, algorithm step 6) for the rationale.
//!
//! Connected-components is a hand-rolled 4-connectivity BFS over `Array2<bool>`
//! because `ndarray-ndimage` 0.6's `label` is 3D-only with a hard `assert!`
//! on a 3×3×3 structuring element.

use std::collections::VecDeque;

use ndarray::{Array2, ArrayView2};
use ndarray_ndimage::{gaussian_filter, BorderMode};

use super::background::BackgroundStats;
use super::pixel::Pixel;

/// A detected star.
#[derive(Debug, Clone)]
pub struct Star {
    /// Flux-weighted centroid along the first array axis ("x", width).
    pub centroid_x: f64,
    /// Flux-weighted centroid along the second array axis ("y", height).
    pub centroid_y: f64,
    /// Sum of background-subtracted, non-negative flux over the component.
    pub total_flux: f64,
    /// Maximum *raw* pixel value over the component (not background-subtracted).
    /// Useful for saturation awareness and as an FWHM-fit initial guess.
    pub peak: f64,
    /// Pixel coordinates `(axis0, axis1)` belonging to this component.
    pub pixels: Vec<(usize, usize)>,
    /// Inclusive bounding box `(min_x, min_y, max_x, max_y)`.
    pub bounding_box: (usize, usize, usize, usize),
    /// How many of `pixels` are at or above the camera's `max_adu`.
    /// Always `0` when `DetectionParams::max_adu` is `None`.
    pub saturated_pixel_count: u32,
}

/// Configuration for [`detect_stars`].
///
/// `min_area` and `max_area` have no defaults at the tool boundary — they
/// encode pixel-scale assumptions the tool cannot make on the caller's
/// behalf. Construct directly; do not derive `Default`.
#[derive(Debug, Clone, Copy)]
pub struct DetectionParams {
    /// Detection threshold above sky in multiples of background stddev.
    pub threshold_sigma: f64,
    /// Gaussian smoothing kernel sigma (px). 1.0 is a reasonable default
    /// for noise suppression on most setups; the design doc pins it.
    pub smoothing_sigma: f64,
    /// Minimum component pixel area to admit as a star.
    pub min_area: usize,
    /// Maximum component pixel area to admit as a star.
    pub max_area: usize,
    /// Camera's saturation level. When `Some`, components with any pixel
    /// at or above this value have their `saturated_pixel_count` reported.
    /// Saturation does *not* gate detection.
    pub max_adu: Option<u32>,
}

/// Run the detection pipeline over `view` against `background`. Returns the
/// list of admitted stars in row-major scan order.
#[must_use]
pub fn detect_stars<T: Pixel>(
    view: &ArrayView2<T>,
    background: &BackgroundStats,
    params: &DetectionParams,
) -> Vec<Star> {
    let (rows, cols) = view.dim();
    if rows == 0 || cols == 0 {
        return Vec::new();
    }

    // Smooth a f64 copy of the input.
    let f64_data: Array2<f64> = view.mapv(super::pixel::Pixel::to_f64);
    let smoothed = if params.smoothing_sigma > 0.0 && rows > 4 && cols > 4 {
        gaussian_filter(&f64_data, params.smoothing_sigma, 0, BorderMode::Reflect, 4)
    } else {
        f64_data
    };

    let threshold = background.mean + params.threshold_sigma * background.stddev;
    let mask = smoothed.mapv(|v| v > threshold);

    let components = connected_components_4(&mask.view());

    components
        .into_iter()
        .filter_map(|pixels| build_star(view, pixels, background.mean, params, rows, cols))
        .collect()
}

fn build_star<T: Pixel>(
    view: &ArrayView2<T>,
    pixels: Vec<(usize, usize)>,
    background_mean: f64,
    params: &DetectionParams,
    rows: usize,
    cols: usize,
) -> Option<Star> {
    let area = pixels.len();
    if area < params.min_area || area > params.max_area {
        return None;
    }

    let (mut min_x, mut min_y) = (usize::MAX, usize::MAX);
    let (mut max_x, mut max_y) = (0usize, 0usize);
    for &(r, c) in &pixels {
        if r < min_x {
            min_x = r;
        }
        if r > max_x {
            max_x = r;
        }
        if c < min_y {
            min_y = c;
        }
        if c > max_y {
            max_y = c;
        }
    }

    if min_x == 0
        || min_y == 0
        || max_x == rows.saturating_sub(1)
        || max_y == cols.saturating_sub(1)
    {
        return None;
    }

    let mut total_flux = 0.0;
    let mut sum_wx = 0.0;
    let mut sum_wy = 0.0;
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    let mut saturated = 0u32;
    let mut peak = f64::NEG_INFINITY;
    for &(r, c) in &pixels {
        let raw = *view.get((r, c))?;
        // Component coordinates come from the same-dimensioned mask, so
        // they fit any in-memory image's u32 bound; `?` folds the
        // impossible overflow into the existing no-star result.
        let r_f = f64::from(u32::try_from(r).ok()?);
        let c_f = f64::from(u32::try_from(c).ok()?);
        let raw_f = raw.to_f64();
        let f = (raw_f - background_mean).max(0.0);
        total_flux += f;
        sum_wx += f * r_f;
        sum_wy += f * c_f;
        sum_x += r_f;
        sum_y += c_f;
        if raw_f > peak {
            peak = raw_f;
        }
        if let Some(max_adu) = params.max_adu {
            if raw.to_u32() >= max_adu {
                saturated = saturated.saturating_add(1);
            }
        }
    }

    let (cx, cy) = if total_flux > 0.0 {
        (sum_wx / total_flux, sum_wy / total_flux)
    } else {
        // Unweighted fallback for a fully-background-subtracted blob;
        // components are non-empty, so `n > 0`.
        let n = f64::from(u32::try_from(area).ok()?);
        (sum_x / n, sum_y / n)
    };

    Some(Star {
        centroid_x: cx,
        centroid_y: cy,
        total_flux,
        peak,
        pixels,
        bounding_box: (min_x, min_y, max_x, max_y),
        saturated_pixel_count: saturated,
    })
}

/// 4-connectivity connected-components labelling. Returns one
/// `Vec<(usize, usize)>` per component in scan order.
fn connected_components_4(mask: &ArrayView2<bool>) -> Vec<Vec<(usize, usize)>> {
    let (rows, cols) = mask.dim();
    let mut visited = Array2::<bool>::from_elem((rows, cols), false);
    let mut components: Vec<Vec<(usize, usize)>> = Vec::new();
    let mut queue: VecDeque<(usize, usize)> = VecDeque::new();

    for ((r0, c0), &seed) in mask.indexed_iter() {
        if !seed || visited.get((r0, c0)).is_none_or(|&v| v) {
            continue;
        }
        queue.clear();
        queue.push_back((r0, c0));
        if let Some(v) = visited.get_mut((r0, c0)) {
            *v = true;
        }
        let mut component: Vec<(usize, usize)> = Vec::new();
        while let Some((r, c)) = queue.pop_front() {
            component.push((r, c));
            // `checked_*` folds the borders into the same skip that
            // `mask.get` gives out-of-range neighbours.
            let neighbours = [
                (r.checked_sub(1), Some(c)),
                (r.checked_add(1), Some(c)),
                (Some(r), c.checked_sub(1)),
                (Some(r), c.checked_add(1)),
            ];
            for (nr, nc) in neighbours {
                let (Some(nr), Some(nc)) = (nr, nc) else {
                    continue;
                };
                if mask.get((nr, nc)) != Some(&true) {
                    continue;
                }
                if let Some(v) = visited.get_mut((nr, nc)) {
                    if !*v {
                        *v = true;
                        queue.push_back((nr, nc));
                    }
                }
            }
        }
        components.push(component);
    }
    components
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::f64::consts::E;

    fn make_gaussian(
        rows: usize,
        cols: usize,
        cx: f64,
        cy: f64,
        sigma: f64,
        peak: f64,
        bg: f64,
    ) -> Array2<u16> {
        let mut arr = Array2::<u16>::from_elem((rows, cols), bg as u16);
        for r in 0..rows {
            for c in 0..cols {
                let dx = r as f64 - cx;
                let dy = c as f64 - cy;
                let exponent = -(dx * dx + dy * dy) / (2.0 * sigma * sigma);
                let v = bg + peak * E.powf(exponent);
                arr[[r, c]] = v.round().clamp(0.0, 65535.0) as u16;
            }
        }
        arr
    }

    fn default_params(min_area: usize, max_area: usize) -> DetectionParams {
        DetectionParams {
            threshold_sigma: 5.0,
            smoothing_sigma: 1.0,
            min_area,
            max_area,
            max_adu: Some(65535),
        }
    }

    #[test]
    fn detects_single_gaussian_with_recovered_centroid() {
        let arr = make_gaussian(64, 64, 32.5, 32.5, 1.5, 20_000.0, 1000.0);
        let bg = BackgroundStats {
            mean: 1000.0,
            stddev: 5.0,
            median: 1000.0,
            n_pixels: 4096,
        };
        let stars = detect_stars(&arr.view(), &bg, &default_params(5, 200));
        assert_eq!(
            stars.len(),
            1,
            "expected exactly one star, got {}",
            stars.len()
        );
        let s = &stars[0];
        assert!(
            (s.centroid_x - 32.5).abs() < 0.5,
            "centroid_x = {}",
            s.centroid_x
        );
        assert!(
            (s.centroid_y - 32.5).abs() < 0.5,
            "centroid_y = {}",
            s.centroid_y
        );
        assert!(s.total_flux > 0.0);
        assert!(s.pixels.len() >= 5);
    }

    #[test]
    fn detects_two_well_separated_stars() {
        let mut arr = make_gaussian(64, 64, 16.0, 16.0, 1.5, 20_000.0, 1000.0);
        let arr2 = make_gaussian(64, 64, 48.0, 48.0, 1.5, 20_000.0, 0.0);
        for r in 0..64 {
            for c in 0..64 {
                arr[[r, c]] = arr[[r, c]].saturating_add(arr2[[r, c]]);
            }
        }
        let bg = BackgroundStats {
            mean: 1000.0,
            stddev: 5.0,
            median: 1000.0,
            n_pixels: 4096,
        };
        let stars = detect_stars(&arr.view(), &bg, &default_params(5, 200));
        assert_eq!(stars.len(), 2, "expected two stars, got {}", stars.len());
    }

    #[test]
    fn below_threshold_returns_empty() {
        // Peak only ~2σ above background — should not pass 5σ threshold.
        let arr = make_gaussian(64, 64, 32.0, 32.0, 1.5, 10.0, 1000.0);
        let bg = BackgroundStats {
            mean: 1000.0,
            stddev: 5.0,
            median: 1000.0,
            n_pixels: 4096,
        };
        let stars = detect_stars(&arr.view(), &bg, &default_params(5, 200));
        assert!(stars.is_empty(), "expected zero stars, got {}", stars.len());
    }

    #[test]
    fn rejects_too_small_area() {
        let arr = make_gaussian(64, 64, 32.0, 32.0, 1.5, 20_000.0, 1000.0);
        let bg = BackgroundStats {
            mean: 1000.0,
            stddev: 5.0,
            median: 1000.0,
            n_pixels: 4096,
        };
        let mut params = default_params(1000, 2000);
        params.min_area = 1000;
        let stars = detect_stars(&arr.view(), &bg, &params);
        assert!(stars.is_empty(), "min_area should reject the star");
    }

    #[test]
    fn rejects_too_large_area() {
        let arr = make_gaussian(64, 64, 32.0, 32.0, 1.5, 20_000.0, 1000.0);
        let bg = BackgroundStats {
            mean: 1000.0,
            stddev: 5.0,
            median: 1000.0,
            n_pixels: 4096,
        };
        let mut params = default_params(5, 5);
        params.max_area = 5;
        let stars = detect_stars(&arr.view(), &bg, &params);
        assert!(stars.is_empty(), "max_area should reject the star");
    }

    #[test]
    fn rejects_components_touching_border() {
        // Star centered at (0, 32) — its bbox necessarily touches the edge.
        let arr = make_gaussian(64, 64, 0.0, 32.0, 1.5, 20_000.0, 1000.0);
        let bg = BackgroundStats {
            mean: 1000.0,
            stddev: 5.0,
            median: 1000.0,
            n_pixels: 4096,
        };
        let stars = detect_stars(&arr.view(), &bg, &default_params(5, 200));
        assert!(
            stars.is_empty(),
            "border-touching component should be rejected"
        );
    }

    #[test]
    fn flags_saturated_pixels_without_rejecting() {
        // Saturated PSF: peak well above max_adu, so the core clips.
        let mut arr = make_gaussian(64, 64, 32.5, 32.5, 1.5, 200_000.0, 1000.0);
        // Force several center pixels to exact max_adu.
        for r in 30..35 {
            for c in 30..35 {
                arr[[r, c]] = 65535;
            }
        }
        let bg = BackgroundStats {
            mean: 1000.0,
            stddev: 5.0,
            median: 1000.0,
            n_pixels: 4096,
        };
        let stars = detect_stars(&arr.view(), &bg, &default_params(5, 1000));
        assert_eq!(stars.len(), 1, "saturated star should still be detected");
        assert!(
            stars[0].saturated_pixel_count >= 25,
            "saturated count = {}",
            stars[0].saturated_pixel_count
        );
    }

    #[test]
    fn no_max_adu_means_zero_saturated_count() {
        let mut arr = make_gaussian(64, 64, 32.5, 32.5, 1.5, 200_000.0, 1000.0);
        for r in 30..35 {
            for c in 30..35 {
                arr[[r, c]] = 65535;
            }
        }
        let bg = BackgroundStats {
            mean: 1000.0,
            stddev: 5.0,
            median: 1000.0,
            n_pixels: 4096,
        };
        let mut params = default_params(5, 1000);
        params.max_adu = None;
        let stars = detect_stars(&arr.view(), &bg, &params);
        assert_eq!(stars.len(), 1);
        assert_eq!(stars[0].saturated_pixel_count, 0);
    }

    #[test]
    fn peak_is_raw_pixel_max_not_background_subtracted() {
        // Centered on an integer grid so the (32, 32) cell hits the full peak:
        // raw = bg + amplitude * exp(0) = 1000 + 20_000 = 21_000.
        let arr = make_gaussian(64, 64, 32.0, 32.0, 1.5, 20_000.0, 1000.0);
        let bg = BackgroundStats {
            mean: 1000.0,
            stddev: 5.0,
            median: 1000.0,
            n_pixels: 4096,
        };
        let stars = detect_stars(&arr.view(), &bg, &default_params(5, 200));
        assert_eq!(stars.len(), 1);
        let p = stars[0].peak;
        assert!(
            (p - 21_000.0).abs() < 1.0,
            "peak should be raw pixel max (~21000), got {p}"
        );
    }

    /// HDR-amplitude i32 Gaussian. Same shape as `make_gaussian` but builds
    /// `Array2<i32>` so we exercise the I32 monomorphization with values
    /// exceeding `u16::MAX`.
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
                let exponent = -(dx * dx + dy * dy) / (2.0 * sigma * sigma);
                arr[[r, c]] = (bg + peak * E.powf(exponent)).round() as i32;
            }
        }
        arr
    }

    #[test]
    fn detects_single_gaussian_in_hdr_i32_pixels() {
        let arr = make_gaussian_i32(64, 64, 32.5, 32.5, 1.5, 200_000.0, 1000.0);
        let bg = BackgroundStats {
            mean: 1000.0,
            stddev: 5.0,
            median: 1000.0,
            n_pixels: 4096,
        };
        let stars = detect_stars(&arr.view(), &bg, &default_params(5, 200));
        assert_eq!(stars.len(), 1);
        let s = &stars[0];
        assert!(
            (s.centroid_x - 32.5).abs() < 0.5,
            "centroid_x = {}",
            s.centroid_x
        );
        assert!(
            (s.centroid_y - 32.5).abs() < 0.5,
            "centroid_y = {}",
            s.centroid_y
        );
        // Peak must retain HDR magnitude — a stray `as u16` cast would wrap.
        assert!(
            s.peak > 65_535.0,
            "peak = {} (expected HDR > u16::MAX)",
            s.peak
        );
    }

    #[test]
    fn empty_view_returns_empty() {
        let arr: Array2<u16> = Array2::zeros((0, 0));
        let bg = BackgroundStats {
            mean: 0.0,
            stddev: 0.0,
            median: 0.0,
            n_pixels: 0,
        };
        let stars = detect_stars(&arr.view(), &bg, &default_params(5, 200));
        assert!(stars.is_empty());
    }

    #[test]
    fn bfs_finds_4_connected_component() {
        // Build a 5x5 plus sign with arms of length 2 (full middle row + full
        // middle column = 5 + 5 − 1 shared center = 9 true pixels).
        // 4-connectivity should yield one component of 9 pixels.
        let mask = Array2::from_shape_vec(
            (5, 5),
            vec![
                false, false, true, false, false, false, false, true, false, false, true, true,
                true, true, true, false, false, true, false, false, false, false, true, false,
                false,
            ],
        )
        .unwrap();
        let comps = connected_components_4(&mask.view());
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].len(), 9);
    }

    #[test]
    fn bfs_separates_disjoint_components() {
        // Two diagonal pixels — under 4-connectivity they are NOT connected.
        let mask = Array2::from_shape_vec(
            (3, 3),
            vec![true, false, false, false, false, false, false, false, true],
        )
        .unwrap();
        let comps = connected_components_4(&mask.view());
        assert_eq!(comps.len(), 2);
    }
}
