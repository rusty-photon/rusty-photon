//! Star detection: hot-pixel suppression → smoothing → thresholding →
//! connected-components labelling → component filtering → centroiding →
//! half-flux-radius floor.
//!
//! Coordinate convention follows the rest of the imaging pipeline: the
//! frame is row-major, exactly as `capture` shapes the cache — ndarray's
//! first axis is "y" (rows, the image height / FITS `NAXIS2`), the second
//! axis is "x" (columns, the width / `NAXIS1`). `centroid_x` is the
//! flux-weighted mean of the column index; `centroid_y` of the row index.
//!
//! Saturated components are *not* rejected — they're flagged via
//! [`Star::saturated_pixel_count`] so downstream consumers (`auto_focus`,
//! quality-screen tools) apply their own policy. See `docs/services/rp.md`
//! (MVP `measure_basic` Contract, algorithm step 7) for the rationale.
//!
//! Connected-components is a hand-rolled 4-connectivity BFS over `Array2<bool>`
//! because `ndarray-ndimage` 0.6's `label` is 3D-only with a hard `assert!`
//! on a 3×3×3 structuring element.

use std::collections::VecDeque;

use ndarray::{Array2, ArrayView2};
use ndarray_ndimage::{gaussian_filter, BorderMode};
use tracing::debug;

use super::background::BackgroundStats;
use super::hfr::star_hfr;
use super::pixel::Pixel;

/// Smallest half-flux radius (px) a component may measure and still be
/// admitted as a star.
///
/// A component under it carries its flux in the one pixel under its
/// centroid: a hot pixel that suppression kept because the sky around it
/// is brighter than the frame's mean, or a cosmic-ray hit. A star
/// measures under it when it is centred near a pixel's centre and
/// undersampled: FWHM under about 1.1 px, or about 1.3 px for a faint
/// star whose component is only its core and four neighbours. On rig2's
/// QHY600M frames, binned 1× and 2×, 96–100% of the detections it
/// removes are bright at the same sensor pixel in a frame taken hours
/// or days later.
pub const MIN_STAR_HFR: f64 = 0.25;

/// How far above the background mean, in background standard deviations,
/// a 3×3 window's median must be for the window to hold a star's body
/// (capped at the detection threshold). The median of nine background
/// pixels scatters by about 0.4 σ, so a median 2 σ up is signal, while
/// a hot structure of up to four pixels leaves it in the background's
/// noise. A level at the threshold would also erase the faint stars
/// that have only one to four pixels above it.
const BODY_SIGMA: f64 = 2.0;

/// A detected star.
#[derive(Debug, Clone)]
pub struct Star {
    /// Flux-weighted centroid along the second array axis ("x": columns,
    /// the image width).
    pub centroid_x: f64,
    /// Flux-weighted centroid along the first array axis ("y": rows, the
    /// image height).
    pub centroid_y: f64,
    /// Sum of background-subtracted, non-negative flux over the component.
    pub total_flux: f64,
    /// Maximum *raw* pixel value over the component (not background-subtracted).
    /// Useful for saturation awareness and as an FWHM-fit initial guess.
    pub peak: f64,
    /// Pixel coordinates `(row, column)` — `(y, x)` — belonging to this
    /// component, its hot pixels left out.
    pub pixels: Vec<(usize, usize)>,
    /// Inclusive bounding box `(min_x, min_y, max_x, max_y)` of the whole
    /// component, hot pixels included.
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

    let (threshold, body_level) = detection_levels(background, params.threshold_sigma);

    // Clear hot pixels from a f64 copy of the input, then smooth it.
    let mut f64_data: Array2<f64> = view.mapv(super::pixel::Pixel::to_f64);
    let hot = suppress_hot_pixels(view, &mut f64_data, threshold, body_level);
    let smoothed = if params.smoothing_sigma > 0.0 && rows > 4 && cols > 4 {
        gaussian_filter(&f64_data, params.smoothing_sigma, 0, BorderMode::Reflect, 4)
    } else {
        f64_data
    };

    let mask = smoothed.mapv(|v| v > threshold);

    let components = connected_components_4(&mask.view());

    components
        .into_iter()
        .filter_map(|pixels| build_star(view, pixels, &hot, background.mean, params, rows, cols))
        .collect()
}

/// The detection threshold for `threshold_sigma`, and the body level
/// hot-pixel suppression tests windows against.
#[expect(
    clippy::suboptimal_flops,
    reason = "mean + k·stddev is the detection threshold in its textbook shape; fusing hides it for no observable gain"
)]
fn detection_levels(background: &BackgroundStats, threshold_sigma: f64) -> (f64, f64) {
    let threshold = background.mean + threshold_sigma * background.stddev;
    let body_level = (background.mean + BODY_SIGMA * background.stddev).min(threshold);
    (threshold, body_level)
}

/// Replace, in `data`, every hot pixel of `view` with the median of the
/// 3×3 window centred on it, and return the hot pixels in row-major
/// order. A pixel is hot when it is above `threshold` and none of the
/// 3×3 windows that contain it holds a star's body: at least five of its
/// pixels above `body_level`, which in a full window is its median.
///
/// Smoothing first would spread a hot pixel into a blob several pixels
/// wide that passes the area filter. A structure of up to four bright
/// pixels holds no body, so single hot pixels, pairs and 2×2 clusters
/// go. So does a star with only one to four pixels above `body_level`:
/// a faint one, or a bright one undersampled to a FWHM under about 1 px
/// and centred near a pixel corner. Any other star keeps every pixel: a
/// window around its core holds its body, and so does a window around
/// the core side of every pixel of its profile. The same holds for the
/// edge of any extended shape. Windows are read from `view`, so a
/// replacement never feeds the next pixel's test, and they are clipped
/// at the frame's edge.
fn suppress_hot_pixels<T: Pixel>(
    view: &ArrayView2<T>,
    data: &mut Array2<f64>,
    threshold: f64,
    body_level: f64,
) -> Vec<(usize, usize)> {
    let dim = view.dim();
    let holds_body = |(r, c): (usize, usize)| {
        window(dim, r, c)
            .filter(|&cell| view.get(cell).is_some_and(|v| v.to_f64() > body_level))
            .count()
            >= 5
    };
    let mut hot = Vec::new();
    for ((r, c), raw) in view.indexed_iter() {
        if raw.to_f64() <= threshold || window(dim, r, c).any(holds_body) {
            continue;
        }
        if let Some(slot) = data.get_mut((r, c)) {
            *slot = window_median(view, r, c);
            hot.push((r, c));
        }
    }
    debug!(
        replaced = hot.len(),
        "hot pixels replaced before star detection"
    );
    hot
}

/// The cells of the 3×3 window centred on `(r, c)` that lie in a frame
/// of `(rows, cols)`: nine inside it, six on an edge, four in a corner.
fn window(
    (rows, cols): (usize, usize),
    r: usize,
    c: usize,
) -> impl Iterator<Item = (usize, usize)> {
    let col_range = c.saturating_sub(1)..=c.saturating_add(1).min(cols.saturating_sub(1));
    (r.saturating_sub(1)..=r.saturating_add(1).min(rows.saturating_sub(1)))
        .flat_map(move |rr| col_range.clone().map(move |cc| (rr, cc)))
}

/// Median of the 3×3 window centred on `(r, c)`, clipped at the frame's
/// edge; with an even count, the lower of the two middle values, so a
/// hot pair in a four-cell corner window is replaced by background.
fn window_median<T: Pixel>(view: &ArrayView2<T>, r: usize, c: usize) -> f64 {
    let mut values: Vec<f64> = window(view.dim(), r, c)
        .filter_map(|cell| view.get(cell).copied())
        .map(Pixel::to_f64)
        .collect();
    // Never empty: `(r, c)` itself is in the frame.
    let middle = values.len().saturating_sub(1) / 2;
    let (_, median, _) = values.select_nth_unstable_by(middle, f64::total_cmp);
    *median
}

/// Admit `pixels`, one component, as a star and measure it. The area
/// and border verdicts are the whole component's; a pixel in `hot`, the
/// hot pixels suppression replaced in row-major order, is left out of
/// every measurement, since the measurements read raw pixels.
fn build_star<T: Pixel>(
    view: &ArrayView2<T>,
    pixels: Vec<(usize, usize)>,
    hot: &[(usize, usize)],
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
        if c < min_x {
            min_x = c;
        }
        if c > max_x {
            max_x = c;
        }
        if r < min_y {
            min_y = r;
        }
        if r > max_y {
            max_y = r;
        }
    }

    if min_x == 0
        || min_y == 0
        || max_y == rows.saturating_sub(1)
        || max_x == cols.saturating_sub(1)
    {
        return None;
    }

    let pixels: Vec<(usize, usize)> = pixels
        .into_iter()
        .filter(|cell| hot.binary_search(cell).is_err())
        .collect();

    let mut total_flux = 0.0;
    let mut weighted_sum_x = 0.0;
    let mut weighted_sum_y = 0.0;
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
        weighted_sum_x = f.mul_add(c_f, weighted_sum_x);
        weighted_sum_y = f.mul_add(r_f, weighted_sum_y);
        sum_x += c_f;
        sum_y += r_f;
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
        (weighted_sum_x / total_flux, weighted_sum_y / total_flux)
    } else {
        // Unweighted fallback for a fully-background-subtracted blob. A
        // component left with no pixel once its hot pixels are out has
        // nothing to measure and is no star.
        let n = f64::from(u32::try_from(pixels.len()).ok().filter(|&n| n > 0)?);
        (sum_x / n, sum_y / n)
    };

    let star = Star {
        centroid_x: cx,
        centroid_y: cy,
        total_flux,
        peak,
        pixels,
        bounding_box: (min_x, min_y, max_x, max_y),
        saturated_pixel_count: saturated,
    };
    if star_hfr(view, &star, background_mean).is_some_and(|hfr| hfr < MIN_STAR_HFR) {
        return None;
    }
    Some(star)
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
                let dx = c as f64 - cx;
                let dy = r as f64 - cy;
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

    /// The frame is row-major: a star placed at column 60 of a 40-row ×
    /// 80-column frame reports `x ≈ 60`, `y ≈ 10` — an `x` that could
    /// not exist if the axes were read the other way round.
    #[test]
    fn centroid_x_counts_columns_and_y_counts_rows() {
        let arr = make_gaussian(40, 80, 60.0, 10.0, 1.5, 20_000.0, 1000.0);
        let bg = BackgroundStats {
            mean: 1000.0,
            stddev: 5.0,
            median: 1000.0,
            n_pixels: 3200,
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
            (s.centroid_x - 60.0).abs() < 0.5,
            "centroid_x = {}",
            s.centroid_x
        );
        assert!(
            (s.centroid_y - 10.0).abs() < 0.5,
            "centroid_y = {}",
            s.centroid_y
        );
        let (min_x, min_y, max_x, max_y) = s.bounding_box;
        assert!(min_x <= 60 && 60 <= max_x, "bbox x range {min_x}..={max_x}");
        assert!(min_y <= 10 && 10 <= max_y, "bbox y range {min_y}..={max_y}");
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
                let dx = c as f64 - cx;
                let dy = r as f64 - cy;
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

    // ---- hot pixels ----

    /// The background every hot-pixel test pins: 1000 ± 5 ADU, so the
    /// detection threshold at 5σ is 1025 ADU and the body level at 2σ
    /// 1010 ADU.
    fn flat_background() -> BackgroundStats {
        BackgroundStats {
            mean: 1000.0,
            stddev: 5.0,
            median: 1000.0,
            n_pixels: 4096,
        }
    }

    /// A flat 64×64 frame at the pinned background with `pixels` set to
    /// `value`.
    fn flat_with(pixels: &[(usize, usize)], value: u16) -> Array2<u16> {
        let mut arr = Array2::<u16>::from_elem((64, 64), 1000);
        for &(r, c) in pixels {
            arr[(r, c)] = value;
        }
        arr
    }

    /// `suppress_hot_pixels` over `arr` against the pinned background.
    fn suppressed(arr: &Array2<u16>) -> Array2<f64> {
        let mut data = arr.mapv(Pixel::to_f64);
        suppress_hot_pixels(&arr.view(), &mut data, 1025.0, 1010.0);
        data
    }

    /// Detection with a `min_area` of 1, so only suppression and the HFR
    /// floor stand between a hot pixel and a star.
    fn detect_any(arr: &Array2<u16>) -> Vec<Star> {
        detect_stars(&arr.view(), &flat_background(), &default_params(1, 2000))
    }

    /// A 7×7 plateau 100 ADU above the background, the shape a fragment
    /// of a defocused star's annulus takes, with an optional hot pixel at
    /// its centre.
    fn plateau(spike: Option<u16>) -> Array2<u16> {
        let mut arr = flat_with(&[], 1000);
        for r in 29..36 {
            for c in 29..36 {
                arr[[r, c]] = 1100;
            }
        }
        if let Some(value) = spike {
            arr[[32, 32]] = value;
        }
        arr
    }

    #[test]
    fn suppression_replaces_an_isolated_pixel_with_its_neighbourhood_median() {
        let data = suppressed(&flat_with(&[(32, 32)], 30_000));
        assert_eq!(data[[32, 32]], 1000.0);
    }

    #[test]
    fn suppression_replaces_both_pixels_of_a_hot_pair() {
        let data = suppressed(&flat_with(&[(32, 32), (32, 33)], 30_000));
        assert_eq!((data[[32, 32]], data[[32, 33]]), (1000.0, 1000.0));
    }

    #[test]
    fn suppression_replaces_a_hot_pixel_in_the_corner() {
        let data = suppressed(&flat_with(&[(0, 0)], 30_000));
        assert_eq!(data[[0, 0]], 1000.0);
    }

    /// The corner's windows are clipped, not padded with copies of the
    /// pair, so the pair holds no body there either.
    #[test]
    fn suppression_replaces_a_hot_pair_in_the_corner() {
        let data = suppressed(&flat_with(&[(0, 0), (0, 1)], 30_000));
        assert_eq!((data[[0, 0]], data[[0, 1]]), (1000.0, 1000.0));
    }

    /// Stars with a 2 px FWHM, from near the detection limit to bright:
    /// none loses a pixel, so a star's footprint, and the verdict any
    /// `min_area` gives on it, is what it was without suppression.
    #[test]
    fn suppression_leaves_a_compact_star_untouched() {
        for peak in [80.0, 3_000.0, 20_000.0] {
            let arr = make_gaussian(64, 64, 32.0, 32.0, 0.85, peak, 1000.0);
            let data = suppressed(&arr);
            assert_eq!(data, arr.mapv(Pixel::to_f64), "peak {peak}");
        }
    }

    /// A 1.2 px FWHM star whose four arms are above the threshold but
    /// whose arms' own neighbourhood medians are not: its core is kept
    /// by its own neighbourhood median, the arms by the core's.
    #[test]
    fn suppression_leaves_an_undersampled_star_untouched() {
        let arr = make_gaussian(64, 64, 32.0, 32.0, 0.5, 1_000.0, 1000.0);
        assert_eq!(suppressed(&arr), arr.mapv(Pixel::to_f64));
    }

    /// A star at the detection limit whose core alone is above the
    /// threshold: its arms, at 1020 ADU, put the core's window above the
    /// body level.
    #[test]
    fn suppression_leaves_a_faint_undersampled_star_untouched() {
        let arr = make_gaussian(64, 64, 32.0, 32.0, 0.5, 150.0, 1000.0);
        assert_eq!(suppressed(&arr), arr.mapv(Pixel::to_f64));
    }

    /// A faint star centred on a pixel corner: four pixels above the
    /// threshold, the shape a 2×2 hot cluster takes, but its wings, at
    /// 1013 ADU, put the windows around them above the body level.
    #[test]
    fn suppression_leaves_a_faint_star_on_a_pixel_corner_untouched() {
        let arr = make_gaussian(64, 64, 32.5, 32.5, 0.85, 75.0, 1000.0);
        assert_eq!(suppressed(&arr), arr.mapv(Pixel::to_f64));
    }

    /// Only a pixel above the threshold can be hot; one at it, alone on
    /// the background, is kept.
    #[test]
    fn suppression_leaves_a_pixel_at_the_threshold_alone() {
        let data = suppressed(&flat_with(&[(32, 32)], 1025));
        assert_eq!(data[[32, 32]], 1025.0);
    }

    /// The plateau's corners have a background neighbourhood median, but
    /// each sits beside the plateau's body, so the shape keeps them.
    #[test]
    fn suppression_leaves_an_extended_shape_untouched() {
        let arr = plateau(None);
        assert_eq!(suppressed(&arr), arr.mapv(Pixel::to_f64));
    }

    /// A bright pixel on a plateau that is itself above the threshold
    /// has a neighbourhood median above it too, so suppression keeps it;
    /// the HFR floor is what rejects the component it dominates.
    #[test]
    fn suppression_leaves_a_hot_pixel_on_bright_signal() {
        let data = suppressed(&plateau(Some(60_000)));
        assert_eq!(data[[32, 32]], 60_000.0);
    }

    #[test]
    fn isolated_hot_pixel_is_not_a_star() {
        let stars = detect_any(&flat_with(&[(32, 32)], 30_000));
        assert!(stars.is_empty(), "got {} stars", stars.len());
    }

    #[test]
    fn hot_pixel_pair_is_not_a_star() {
        let stars = detect_any(&flat_with(&[(32, 32), (32, 33)], 30_000));
        assert!(stars.is_empty(), "got {} stars", stars.len());
    }

    #[test]
    fn two_by_two_hot_cluster_is_not_a_star() {
        let cluster = [(32, 32), (32, 33), (33, 32), (33, 33)];
        let stars = detect_any(&flat_with(&cluster, 30_000));
        assert!(stars.is_empty(), "got {} stars", stars.len());
    }

    #[test]
    fn star_beside_a_hot_pixel_is_the_only_detection() {
        let mut arr = make_gaussian(64, 64, 32.0, 32.0, 1.5, 20_000.0, 1000.0);
        arr[[32, 46]] = 30_000;
        let stars = detect_any(&arr);
        assert_eq!(stars.len(), 1, "got {} stars", stars.len());
        assert!(
            (stars[0].centroid_x - 32.0).abs() < 0.5,
            "centroid_x = {}",
            stars[0].centroid_x
        );
    }

    /// A bright compact star's smoothed footprint reaches a hot pixel
    /// 4 px from its core that suppression replaced; the star is measured
    /// exactly as it is without the hot pixel.
    #[test]
    fn hot_pixel_in_a_stars_smoothed_footprint_is_not_measured() {
        let star = make_gaussian(64, 64, 32.0, 32.0, 0.6, 60_000.0, 1000.0);
        let mut with_hot = star.clone();
        with_hot[[32, 36]] = 30_000;
        let alone = detect_any(&star);
        let beside = detect_any(&with_hot);
        assert_eq!((alone.len(), beside.len()), (1, 1));
        assert_eq!(
            (beside[0].centroid_x, beside[0].total_flux),
            (alone[0].centroid_x, alone[0].total_flux)
        );
    }

    /// A star 3 px from the edge whose component reaches column 0 only at
    /// a hot pixel: the border verdict is the whole component's, so the
    /// truncated star is rejected rather than measured without that pixel.
    #[test]
    fn component_touching_the_border_only_at_a_hot_pixel_is_rejected() {
        let arr = make_gaussian(16, 16, 3.0, 8.0, 1.0, 5_000.0, 1000.0);
        let mut pixels: Vec<(usize, usize)> = (6..=10)
            .flat_map(|r| (1..=6).map(move |c| (r, c)))
            .collect();
        pixels.push((8, 0));
        let star = build_star(
            &arr.view(),
            pixels,
            &[(8, 0)],
            1000.0,
            &default_params(1, 2000),
            16,
            16,
        );
        assert!(
            star.is_none(),
            "admitted with box {:?}",
            star.map(|s| s.bounding_box)
        );
    }

    #[test]
    fn component_of_hot_pixels_only_is_no_star() {
        let arr = flat_with(&[(8, 8), (8, 9)], 30_000);
        let star = build_star(
            &arr.view(),
            vec![(8, 8), (8, 9)],
            &[(8, 8), (8, 9)],
            1000.0,
            &default_params(1, 2000),
            64,
            64,
        );
        assert!(star.is_none());
    }

    #[test]
    fn component_whose_flux_sits_in_one_pixel_is_rejected() {
        let stars = detect_any(&plateau(Some(60_000)));
        assert!(stars.is_empty(), "got {} stars", stars.len());
    }

    #[test]
    fn plateau_without_a_hot_pixel_is_a_star() {
        let stars = detect_any(&plateau(None));
        assert_eq!(stars.len(), 1, "got {} stars", stars.len());
    }

    /// The `(row, column)` of each of the nine hot pixels a detector that
    /// smooths first counts as stars on the real crop at `threshold_sigma`
    /// 5, `min_area` 4 and `max_area` 400; each is a single pixel.
    const CROP_HOT_PIXELS: [(usize, usize); 9] = [
        (54, 274),
        (71, 59),
        (81, 287),
        (113, 155),
        (212, 354),
        (274, 260),
        (378, 199),
        (380, 249),
        (448, 214),
    ];

    /// A 512×512 crop of a real QHY600M frame at best focus, provenance
    /// in the fixture directory's README. `measure_basic.feature` pins
    /// what `measure_basic` reports on it; this pins that suppression,
    /// not the HFR floor, is what removes its hot pixels at those
    /// settings.
    #[test]
    fn real_frame_hot_pixels_are_replaced_before_smoothing() {
        let bytes: &[u8] =
            include_bytes!("../../../tests/fixtures/star_detection/qhy600m_best_focus_crop.fits");
        let (pixels, width, height) =
            rp_fits::reader::read_primary_as_i32(std::io::Cursor::new(bytes)).unwrap();
        let arr = Array2::from_shape_vec((height, width), pixels).unwrap();
        let bg = super::super::background::estimate_background(&arr.view()).unwrap();
        let (threshold, body_level) = detection_levels(&bg, 5.0);
        let mut data = arr.mapv(Pixel::to_f64);
        let hot = suppress_hot_pixels(&arr.view(), &mut data, threshold, body_level);
        let kept: Vec<_> = CROP_HOT_PIXELS
            .iter()
            .filter(|cell| hot.binary_search(cell).is_err())
            .collect();
        assert!(kept.is_empty(), "hot pixels left in place: {kept:?}");
    }
}
