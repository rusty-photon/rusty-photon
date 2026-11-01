//! Half-flux radius in an aperture that follows each star, one star per
//! aperture, and the aggregate over the brightest stars.
//!
//! A detection's own component caps its half-flux radius: out of focus
//! the threshold keeps only the brightest parts of a star's blur disc.
//! Each detection is therefore measured again, starting at its centroid,
//! in an aperture of [`APERTURE_FACTOR`] times its half-flux radius whose
//! centre and radius are refined round by round against the background
//! of an annulus just outside it. The fragments of a defocused star all
//! settle on its centre; ranking the detections by aperture flux and
//! dropping each one that lies inside a brighter one's aperture leaves
//! one star per aperture. See `docs/services/rp.md` (`measure_basic`
//! algorithm steps 9–11).

use std::f64::consts::PI;
use std::ops::Range;

use ndarray::ArrayView2;
use tracing::debug;

use super::background::sigma_clipped_values;
use super::hfr::star_hfr;
use super::pixel::Pixel;
use super::stars::{Detection, Star, MIN_STAR_HFR};

/// The aperture's radius as a multiple of the half-flux radius measured
/// in it.
///
/// Three times the HFR holds 99.8% of a Gaussian star's light and 2.1
/// radii of a uniform disc; on rig2's sweeps any factor from 2.5 to 5
/// fits the same vertex.
pub const APERTURE_FACTOR: f64 = 3.0;

/// The smallest aperture radius (px), for a star sharper than a pixel.
pub const MIN_APERTURE_RADIUS: f64 = 3.0;

/// How many of the brightest stars the aggregate half-flux radius takes
/// the median of.
pub const BRIGHTEST: usize = 20;

/// Gap (px) between the aperture's edge and the background annulus.
const ANNULUS_GAP: f64 = 2.0;

/// The annulus is half the aperture's radius wide, and at least this
/// wide (px).
const MIN_ANNULUS_WIDTH: f64 = 6.0;

/// Sigma-clip of the annulus: the same `k` and iteration cap as the
/// frame's own background.
const ANNULUS_CLIP_SIGMA: f64 = 3.0;
const ANNULUS_CLIP_ITERATIONS: usize = 5;

/// A detection that has not settled after this many rounds is no star.
const MAX_ROUNDS: usize = 40;

/// A round settles when the centre moves less than this (px) …
const CENTRE_TOLERANCE: f64 = 0.05;

/// … and the radius changes by less than this fraction of itself.
const RADIUS_TOLERANCE: f64 = 0.05;

/// A star measured in its aperture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeasuredStar {
    /// The aperture's centre along the second array axis ("x": columns).
    pub x: f64,
    /// The aperture's centre along the first array axis ("y": rows).
    pub y: f64,
    /// Half-flux radius (px) in the aperture.
    pub hfr: f64,
    /// Sum of the aperture's signed, background-subtracted pixels (ADU).
    pub flux: f64,
    /// The aperture's radius (px).
    pub radius: f64,
    /// Whether the aperture holds a pixel at or above the camera's
    /// `max_adu`. Always `false` when `max_adu` is `None`.
    pub saturated: bool,
    /// The index, in the detection list, of the detection the star was
    /// measured from.
    pub detection: usize,
}

/// One round's measurement of an aperture.
#[derive(Debug, Clone, Copy)]
struct Aperture {
    x: f64,
    y: f64,
    radius: f64,
    flux: f64,
    hfr: f64,
}

/// How a detection's rounds ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Star,
    NoFlux,
    Unsettled,
    Edge,
    TooLarge,
    Floor,
}

/// How many detections were not stars, by reason.
#[derive(Debug, Default)]
struct Rejections {
    no_flux: usize,
    unsettled: usize,
    edge: usize,
    too_large: usize,
    floor: usize,
    inside_brighter: usize,
}

impl Rejections {
    const fn count(&mut self, verdict: Verdict) {
        let slot = match verdict {
            Verdict::Star => return,
            Verdict::NoFlux => &mut self.no_flux,
            Verdict::Unsettled => &mut self.unsettled,
            Verdict::Edge => &mut self.edge,
            Verdict::TooLarge => &mut self.too_large,
            Verdict::Floor => &mut self.floor,
        };
        *slot = slot.saturating_add(1);
    }
}

/// Measure every detection in its aperture and keep one star per
/// aperture, brightest first.
///
/// `max_area` bounds a star's size: one whose half-flux disc is larger
/// has no HFR. `max_adu` flags a star whose aperture holds a saturated
/// pixel; saturation does not reject it.
#[must_use]
pub fn measure_in_apertures<T: Pixel>(
    view: &ArrayView2<T>,
    detection: &Detection,
    background_mean: f64,
    max_area: usize,
    max_adu: Option<u32>,
) -> Vec<MeasuredStar> {
    let max_radius = APERTURE_FACTOR * half_flux_radius_cap(max_area);
    let mut rejected = Rejections::default();
    let mut measured: Vec<(usize, Aperture, Verdict)> = Vec::new();
    for (index, star) in detection.stars.iter().enumerate() {
        let (last, verdict) = follow_star(view, star, &detection.hot, background_mean, max_radius);
        rejected.count(verdict);
        if let Some(aperture) = last {
            measured.push((index, aperture, verdict));
        }
    }
    measured.sort_by(|a, b| b.1.flux.total_cmp(&a.1.flux).then(a.0.cmp(&b.0)));

    let mut kept: Vec<(usize, Aperture, Verdict)> = Vec::new();
    for candidate in measured {
        let inside = kept.iter().any(|(_, brighter, _)| {
            (candidate.1.x - brighter.x).hypot(candidate.1.y - brighter.y) <= brighter.radius
        });
        if inside {
            rejected.inside_brighter = rejected.inside_brighter.saturating_add(1);
        } else {
            kept.push(candidate);
        }
    }

    let stars: Vec<MeasuredStar> = kept
        .into_iter()
        .filter(|(_, _, verdict)| *verdict == Verdict::Star)
        .map(|(index, aperture, _)| MeasuredStar {
            x: aperture.x,
            y: aperture.y,
            hfr: aperture.hfr,
            flux: aperture.flux,
            radius: aperture.radius,
            saturated: holds_saturated_pixel(view, &aperture, &detection.hot, max_adu),
            detection: index,
        })
        .collect();
    debug!(
        detections = detection.stars.len(),
        stars = stars.len(),
        ?rejected,
        "detections measured in apertures"
    );
    stars
}

/// The median half-flux radius of the [`BRIGHTEST`] stars of `stars`.
///
/// [`measure_in_apertures`] orders the stars brightest first; all of them
/// count when there are fewer. The two middle values are averaged for an
/// even count. `None` when `stars` is empty.
#[must_use]
pub fn aggregate_hfr(stars: &[MeasuredStar]) -> Option<f64> {
    let mut hfrs: Vec<f64> = stars.iter().take(BRIGHTEST).map(|s| s.hfr).collect();
    hfrs.sort_by(f64::total_cmp);
    let middle = hfrs.len() / 2;
    let upper = hfrs.get(middle).copied()?;
    if hfrs.len() % 2 == 1 {
        Some(upper)
    } else {
        let lower = hfrs.get(middle.checked_sub(1)?).copied()?;
        Some(f64::midpoint(lower, upper))
    }
}

/// The largest half-flux radius (px) a star may measure: the radius of a
/// disc of `max_area` pixels.
fn half_flux_radius_cap(max_area: usize) -> f64 {
    u32::try_from(max_area).map_or(f64::INFINITY, |area| (f64::from(area) / PI).sqrt())
}

/// Refine one detection's aperture round by round. Returns the last
/// aperture it was measured in, if any, and how the rounds ended.
fn follow_star<T: Pixel>(
    view: &ArrayView2<T>,
    star: &Star,
    hot: &[(usize, usize)],
    background_mean: f64,
    max_radius: f64,
) -> (Option<Aperture>, Verdict) {
    let (mut x, mut y) = (star.centroid_x, star.centroid_y);
    let component_hfr = star_hfr(view, star, background_mean).unwrap_or(0.0);
    let extent = star
        .pixels
        .iter()
        .filter_map(|&cell| distance(cell, x, y))
        .fold(0.0, f64::max);
    let mut radius = (APERTURE_FACTOR * component_hfr)
        .max(extent + 1.0)
        .max(MIN_APERTURE_RADIUS);
    let mut last = None;
    for _ in 0..MAX_ROUNDS {
        if radius > max_radius {
            return (last, Verdict::TooLarge);
        }
        let Some(background) = annulus_background(view, hot, x, y, radius) else {
            return (last, Verdict::Edge);
        };
        let Some(round) = measure_round(view, hot, x, y, radius, background) else {
            return (None, Verdict::NoFlux);
        };
        let aperture = Aperture {
            x,
            y,
            radius,
            flux: round.flux,
            hfr: round.hfr,
        };
        last = Some(aperture);
        let next_radius = (APERTURE_FACTOR * round.hfr).max(MIN_APERTURE_RADIUS);
        let shift = (round.centre_x - x).hypot(round.centre_y - y);
        if shift < CENTRE_TOLERANCE && (next_radius - radius).abs() < RADIUS_TOLERANCE * radius {
            let verdict = if round.hfr < MIN_STAR_HFR {
                Verdict::Floor
            } else {
                Verdict::Star
            };
            return (last, verdict);
        }
        (x, y, radius) = (round.centre_x, round.centre_y, next_radius);
    }
    (last, Verdict::Unsettled)
}

/// The sigma-clipped mean of the annulus around an aperture of `radius`
/// at `(x, y)`, hot pixels left out. `None` when the annulus reaches past
/// the frame's edge.
fn annulus_background<T: Pixel>(
    view: &ArrayView2<T>,
    hot: &[(usize, usize)],
    x: f64,
    y: f64,
    radius: f64,
) -> Option<f64> {
    let inner = radius + ANNULUS_GAP;
    let outer = inner + (radius / 2.0).max(MIN_ANNULUS_WIDTH);
    let (rows, cols) = view.dim();
    let last_row = f64::from(u32::try_from(rows.checked_sub(1)?).ok()?);
    let last_col = f64::from(u32::try_from(cols.checked_sub(1)?).ok()?);
    if x - outer < 0.0 || y - outer < 0.0 || x + outer > last_col || y + outer > last_row {
        return None;
    }
    let mut values = Vec::new();
    for_each_pixel_within(view, hot, x, y, outer, |d, value| {
        if d >= inner {
            values.push(value);
        }
    });
    sigma_clipped_values(values, ANNULUS_CLIP_SIGMA, ANNULUS_CLIP_ITERATIONS).map(|s| s.mean)
}

/// One round's measurement inside an aperture.
struct Round {
    flux: f64,
    hfr: f64,
    centre_x: f64,
    centre_y: f64,
}

/// Measure the aperture of `radius` at `(x, y)` against `background`.
/// `None` when its flux is not positive.
fn measure_round<T: Pixel>(
    view: &ArrayView2<T>,
    hot: &[(usize, usize)],
    x: f64,
    y: f64,
    radius: f64,
    background: f64,
) -> Option<Round> {
    let mut samples: Vec<(f64, f64)> = Vec::new();
    let (mut weight, mut sum_x, mut sum_y) = (0.0, 0.0, 0.0);
    let mut flux = 0.0;
    for_each_cell_within(view, hot, x, y, radius, |(r, c), d, value| {
        let f = value - background;
        samples.push((d, f));
        flux += f;
        if f > 0.0 {
            weight += f;
            sum_x = f.mul_add(c, sum_x);
            sum_y = f.mul_add(r, sum_y);
        }
    });
    if flux <= 0.0 || weight <= 0.0 {
        return None;
    }
    samples.sort_by(|a, b| a.0.total_cmp(&b.0));
    Some(Round {
        flux,
        hfr: half_flux_distance(&samples, flux),
        centre_x: sum_x / weight,
        centre_y: sum_y / weight,
    })
}

/// The distance at which the running sum of `samples`, sorted by
/// distance, first reaches half of `flux`, interpolated linearly between
/// the two samples either side.
fn half_flux_distance(samples: &[(f64, f64)], flux: f64) -> f64 {
    let half = flux / 2.0;
    let (mut previous_distance, mut previous_sum) = (0.0, 0.0);
    for &(d, f) in samples {
        let sum = previous_sum + f;
        if sum >= half {
            let span = sum - previous_sum;
            if span <= 0.0 {
                return d;
            }
            let t = (half - previous_sum) / span;
            return t.mul_add(d - previous_distance, previous_distance);
        }
        previous_distance = d;
        previous_sum = sum;
    }
    previous_distance
}

/// Whether the aperture holds a pixel at or above `max_adu`, hot pixels
/// left out.
fn holds_saturated_pixel<T: Pixel>(
    view: &ArrayView2<T>,
    aperture: &Aperture,
    hot: &[(usize, usize)],
    max_adu: Option<u32>,
) -> bool {
    let Some(max_adu) = max_adu.map(f64::from) else {
        return false;
    };
    let mut saturated = false;
    for_each_pixel_within(
        view,
        hot,
        aperture.x,
        aperture.y,
        aperture.radius,
        |_, value| saturated = saturated || value >= max_adu,
    );
    saturated
}

/// [`for_each_cell_within`], without the cell.
fn for_each_pixel_within<T: Pixel>(
    view: &ArrayView2<T>,
    hot: &[(usize, usize)],
    x: f64,
    y: f64,
    radius: f64,
    mut visit: impl FnMut(f64, f64),
) {
    for_each_cell_within(view, hot, x, y, radius, |_, d, value| visit(d, value));
}

/// Call `visit` with each pixel within `radius` of `(x, y)` that is not
/// hot: its cell as `(row, column)` coordinates, its distance from
/// `(x, y)`, and its raw value.
fn for_each_cell_within<T: Pixel>(
    view: &ArrayView2<T>,
    hot: &[(usize, usize)],
    x: f64,
    y: f64,
    radius: f64,
    mut visit: impl FnMut((f64, f64), f64, f64),
) {
    let (rows, cols) = view.dim();
    for r in indices_between(y - radius, y + radius, rows) {
        let Some(row) = coordinate(r) else { continue };
        for c in indices_between(x - radius, x + radius, cols) {
            let Some(col) = coordinate(c) else { continue };
            let d = (col - x).hypot(row - y);
            if d > radius || hot.binary_search(&(r, c)).is_ok() {
                continue;
            }
            if let Some(value) = view.get((r, c)) {
                visit((row, col), d, value.to_f64());
            }
        }
    }
}

/// The indices along an axis of `len` pixels whose coordinate lies in
/// `[low, high]`.
fn indices_between(low: f64, high: f64, len: usize) -> Range<usize> {
    first_index(len, |v| v < low)..first_index(len, |v| v <= high)
}

/// The first index in `0..len` whose coordinate does not satisfy
/// `before`, which must hold for a prefix of the indices; `len` when
/// every index satisfies it.
fn first_index(len: usize, before: impl Fn(f64) -> bool) -> usize {
    let (mut low, mut high) = (0, len);
    while low < high {
        let middle = low.midpoint(high);
        if coordinate(middle).is_some_and(&before) {
            low = middle.saturating_add(1);
        } else {
            high = middle;
        }
    }
    low
}

/// A pixel index as a coordinate. Indices of an in-memory frame fit
/// `u32`, so `None` is never seen.
fn coordinate(index: usize) -> Option<f64> {
    u32::try_from(index).ok().map(f64::from)
}

/// The distance of `(row, column)` from `(x, y)`.
fn distance((r, c): (usize, usize), x: f64, y: f64) -> Option<f64> {
    Some((coordinate(c)? - x).hypot(coordinate(r)? - y))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use ndarray::Array2;

    const BACKGROUND: f64 = 1000.0;

    fn flat(rows: usize, cols: usize) -> Array2<u16> {
        Array2::from_elem((rows, cols), 1000)
    }

    /// Raise every pixel within `radius` of `(x, y)` by `height`.
    fn add_disc(arr: &mut Array2<u16>, x: f64, y: f64, radius: f64, height: u16) {
        for ((r, c), v) in arr.indexed_iter_mut() {
            if (c as f64 - x).hypot(r as f64 - y) <= radius {
                *v += height;
            }
        }
    }

    /// A detection made of `pixels`, centred on their flux-weighted mean.
    fn detection_of(arr: &Array2<u16>, pixels: Vec<(usize, usize)>) -> Star {
        let flux = |&(r, c): &(usize, usize)| f64::from(arr[(r, c)]) - BACKGROUND;
        let total: f64 = pixels.iter().map(flux).sum();
        let x = pixels.iter().map(|p| flux(p) * p.1 as f64).sum::<f64>() / total;
        let y = pixels.iter().map(|p| flux(p) * p.0 as f64).sum::<f64>() / total;
        let (min_x, max_x) = (
            pixels.iter().map(|p| p.1).min().unwrap(),
            pixels.iter().map(|p| p.1).max().unwrap(),
        );
        let (min_y, max_y) = (
            pixels.iter().map(|p| p.0).min().unwrap(),
            pixels.iter().map(|p| p.0).max().unwrap(),
        );
        Star {
            centroid_x: x,
            centroid_y: y,
            total_flux: total,
            peak: pixels
                .iter()
                .map(|&(r, c)| f64::from(arr[(r, c)]))
                .fold(0.0, f64::max),
            pixels,
            bounding_box: (min_x, min_y, max_x, max_y),
            saturated_pixel_count: 0,
        }
    }

    /// The pixels of `arr` above the background within `radius` of
    /// `(x, y)` that `keep` admits.
    fn pixels_near(
        arr: &Array2<u16>,
        x: f64,
        y: f64,
        radius: f64,
        keep: impl Fn(usize, usize) -> bool,
    ) -> Vec<(usize, usize)> {
        arr.indexed_iter()
            .filter(|&((r, c), &v)| {
                f64::from(v) > BACKGROUND
                    && (c as f64 - x).hypot(r as f64 - y) <= radius
                    && keep(r, c)
            })
            .map(|(cell, _)| cell)
            .collect()
    }

    fn measure(arr: &Array2<u16>, stars: Vec<Star>, max_area: usize) -> Vec<MeasuredStar> {
        let detection = Detection {
            stars,
            hot: Vec::new(),
        };
        measure_in_apertures(&arr.view(), &detection, BACKGROUND, max_area, None)
    }

    fn star(hfr: f64) -> MeasuredStar {
        MeasuredStar {
            x: 0.0,
            y: 0.0,
            hfr,
            flux: 1.0,
            radius: 3.0,
            saturated: false,
            detection: 0,
        }
    }

    #[test]
    fn half_flux_distance_interpolates_between_the_two_samples_either_side() {
        // Running sums 1, 2, 4 at distances 1, 2, 3: half of 4 is reached
        // at distance 2 exactly.
        let samples = [(1.0, 1.0), (2.0, 1.0), (3.0, 2.0)];
        assert!((half_flux_distance(&samples, 4.0) - 2.0).abs() < 1e-12);
        // Half of 3 is 1.5, half way between distance 1 (sum 1) and 2 (sum 2).
        let samples = [(1.0, 1.0), (2.0, 1.0), (3.0, 1.0)];
        assert!((half_flux_distance(&samples, 3.0) - 1.5).abs() < 1e-12);
    }

    #[test]
    fn half_flux_distance_counts_negative_samples() {
        // Running sums 0.5, 0, 2: the negative sample takes the sum back
        // to 0, so half of 2 is reached half way from distance 2 to 3.
        let samples = [(1.0, 0.5), (2.0, -0.5), (3.0, 2.0)];
        assert!((half_flux_distance(&samples, 2.0) - 2.5).abs() < 1e-12);
    }

    #[test]
    fn indices_between_covers_the_coordinates_in_the_closed_range() {
        assert_eq!(indices_between(2.5, 5.0, 10), 3..6);
        assert_eq!(indices_between(-3.0, 2.0, 10), 0..3);
        assert_eq!(indices_between(8.2, 20.0, 10), 9..10);
        assert!(indices_between(4.2, 4.8, 10).is_empty());
    }

    #[test]
    fn aggregate_is_the_median_of_an_odd_count() {
        let stars = [star(3.0), star(1.0), star(2.0)];
        assert!((aggregate_hfr(&stars).unwrap() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn aggregate_averages_the_two_middle_values_of_an_even_count() {
        let stars = [star(4.0), star(1.0), star(2.0), star(3.0)];
        assert!((aggregate_hfr(&stars).unwrap() - 2.5).abs() < 1e-12);
    }

    #[test]
    fn aggregate_reads_only_the_brightest_stars() {
        // Stars come brightest first: the twenty brightest measure 1 px,
        // the ten after them 50 px, which the aggregate does not see.
        let stars: Vec<MeasuredStar> = std::iter::repeat_n(star(1.0), BRIGHTEST)
            .chain(std::iter::repeat_n(star(50.0), 10))
            .collect();
        assert!((aggregate_hfr(&stars).unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn aggregate_of_no_stars_is_none() {
        assert!(aggregate_hfr(&[]).is_none());
    }

    #[test]
    fn a_compact_star_is_measured_at_its_own_half_flux_radius() {
        // A uniform disc of radius 4: half its light lies within 4/√2.
        let mut arr = flat(64, 64);
        add_disc(&mut arr, 32.0, 32.0, 4.0, 500);
        let disc = pixels_near(&arr, 32.0, 32.0, 4.0, |_, _| true);
        let stars = measure(&arr, vec![detection_of(&arr, disc)], 4096);
        assert_eq!(stars.len(), 1);
        assert!((stars[0].hfr - 4.0 / 2_f64.sqrt()).abs() < 0.3, "{stars:?}");
        assert!((stars[0].x - 32.0).abs() < 0.1 && (stars[0].y - 32.0).abs() < 0.1);
    }

    #[test]
    fn a_fragment_of_a_disc_is_measured_as_the_whole_disc() {
        // The detection is a sliver at the disc's right-hand edge, as the
        // threshold leaves of a faint defocused star. Its aperture grows
        // and walks to the disc's centre.
        let mut arr = flat(100, 100);
        add_disc(&mut arr, 50.0, 50.0, 12.0, 100);
        let sliver = pixels_near(&arr, 50.0, 50.0, 12.0, |r, c| {
            c >= 58 && r.abs_diff(50) <= 3
        });
        let stars = measure(&arr, vec![detection_of(&arr, sliver)], 4096);
        assert_eq!(stars.len(), 1);
        let disc = stars[0];
        assert!(
            (disc.x - 50.0).abs() < 0.2 && (disc.y - 50.0).abs() < 0.2,
            "{disc:?}"
        );
        assert!((disc.hfr - 12.0 / 2_f64.sqrt()).abs() < 0.3, "{disc:?}");
    }

    #[test]
    fn two_fragments_of_one_disc_are_one_star() {
        let mut arr = flat(100, 100);
        add_disc(&mut arr, 50.0, 50.0, 12.0, 100);
        let right = pixels_near(&arr, 50.0, 50.0, 12.0, |r, c| {
            c >= 58 && r.abs_diff(50) <= 3
        });
        let left = pixels_near(&arr, 50.0, 50.0, 12.0, |r, c| {
            c <= 42 && r.abs_diff(50) <= 3
        });
        let stars = measure(
            &arr,
            vec![detection_of(&arr, right), detection_of(&arr, left)],
            4096,
        );
        assert_eq!(stars.len(), 1, "{stars:?}");
    }

    #[test]
    fn two_separate_stars_are_listed_brightest_first() {
        let mut arr = flat(64, 96);
        add_disc(&mut arr, 24.0, 32.0, 3.0, 200);
        add_disc(&mut arr, 72.0, 32.0, 3.0, 600);
        let faint = pixels_near(&arr, 24.0, 32.0, 3.0, |_, _| true);
        let bright = pixels_near(&arr, 72.0, 32.0, 3.0, |_, _| true);
        let stars = measure(
            &arr,
            vec![detection_of(&arr, faint), detection_of(&arr, bright)],
            4096,
        );
        assert_eq!(stars.len(), 2);
        assert!((stars[0].x - 72.0).abs() < 0.1, "{stars:?}");
        assert_eq!(stars[0].detection, 1);
        assert!((stars[1].x - 24.0).abs() < 0.1, "{stars:?}");
    }

    #[test]
    fn a_star_whose_half_flux_disc_exceeds_max_area_is_not_measured() {
        // HFR 12/√2 ≈ 8.5 px: a half-flux disc of about 226 px².
        let mut arr = flat(100, 100);
        add_disc(&mut arr, 50.0, 50.0, 12.0, 100);
        let disc = pixels_near(&arr, 50.0, 50.0, 12.0, |_, _| true);
        assert_eq!(
            measure(&arr, vec![detection_of(&arr, disc.clone())], 300).len(),
            1
        );
        assert_eq!(measure(&arr, vec![detection_of(&arr, disc)], 200), []);
    }

    #[test]
    fn a_star_whose_annulus_reaches_past_the_edge_is_not_measured() {
        // Radius 3 + gap 2 + width 6: the annulus needs 11 px of margin.
        let mut arr = flat(64, 64);
        add_disc(&mut arr, 9.0, 32.0, 2.0, 500);
        let disc = pixels_near(&arr, 9.0, 32.0, 2.0, |_, _| true);
        assert_eq!(measure(&arr, vec![detection_of(&arr, disc)], 4096), []);
    }

    #[test]
    fn a_hot_pixel_in_the_aperture_is_left_out() {
        let mut arr = flat(64, 64);
        add_disc(&mut arr, 32.0, 32.0, 4.0, 500);
        let disc = pixels_near(&arr, 32.0, 32.0, 4.0, |_, _| true);
        let clean = measure(&arr, vec![detection_of(&arr, disc.clone())], 4096);
        arr[[32, 37]] = 60_000;
        let detection = Detection {
            stars: vec![detection_of(&arr, disc)],
            hot: vec![(32, 37)],
        };
        let with_hot = measure_in_apertures(&arr.view(), &detection, BACKGROUND, 4096, None);
        assert_eq!(with_hot.len(), 1);
        assert!(
            (with_hot[0].hfr - clean[0].hfr).abs() < 1e-9,
            "{with_hot:?} vs {clean:?}"
        );
    }

    #[test]
    fn a_saturated_pixel_in_the_aperture_flags_the_star() {
        let mut arr = flat(64, 64);
        add_disc(&mut arr, 32.0, 32.0, 4.0, 500);
        arr[[32, 32]] = 4000;
        let disc = pixels_near(&arr, 32.0, 32.0, 4.0, |_, _| true);
        let detection = Detection {
            stars: vec![detection_of(&arr, disc)],
            hot: Vec::new(),
        };
        let flagged = measure_in_apertures(&arr.view(), &detection, BACKGROUND, 4096, Some(4000));
        assert!(flagged[0].saturated);
        let unknown = measure_in_apertures(&arr.view(), &detection, BACKGROUND, 4096, None);
        assert!(!unknown[0].saturated);
    }
}
