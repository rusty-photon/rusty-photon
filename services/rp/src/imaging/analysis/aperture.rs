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
//!
//! A list of stars already known is measured the same way, each aperture
//! starting where its star was last seen ([`measure_listed`]; the
//! `measure_stars` contract's `stars` input).

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

/// The SNR a listed star must exceed to count as measured: the
/// threshold `CCDciel`'s autofocus keeps a star at.
pub const MIN_LISTED_SNR: f64 = 3.0;

/// A star to measure where it was last seen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Listed {
    /// Where the star was last seen along the columns.
    pub x: f64,
    /// Where the star was last seen along the rows.
    pub y: f64,
    /// The radius of the aperture it was last measured in (px).
    pub radius: f64,
}

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
    /// Whether the aperture holds a pixel at or above the frame's
    /// saturation level. Always `false` when the level is `None`, and
    /// for a star [`measure_listed`] measured, which takes no level.
    pub saturated: bool,
    /// The index, in the detection list, of the detection the star was
    /// measured from. For a star [`measure_listed`] measured, the index of
    /// its entry in `listed`.
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
    faint: usize,
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
/// has no HFR. `saturation_level` (`super::saturation::saturation_level`)
/// flags a star whose aperture holds a pixel at or above it; saturation
/// does not reject it.
#[must_use]
pub fn measure_in_apertures<T: Pixel>(
    view: &ArrayView2<T>,
    detection: &Detection,
    background_mean: f64,
    max_area: usize,
    saturation_level: Option<u32>,
) -> Vec<MeasuredStar> {
    let max_hfr = half_flux_radius_cap(max_area);
    let mut rejected = Rejections::default();
    let mut measured: Vec<(usize, Aperture, Verdict)> = Vec::new();
    for (index, star) in detection.stars.iter().enumerate() {
        let (last, verdict) = follow_star(view, star, &detection.hot, background_mean, max_hfr);
        match last {
            Some(aperture) => measured.push((index, aperture, verdict)),
            None => rejected.count(verdict),
        }
    }
    let kept = one_per_aperture(measured, &mut rejected);

    let stars: Vec<MeasuredStar> = kept
        .into_iter()
        .filter(|(_, _, verdict)| *verdict == Verdict::Star)
        .map(|(index, aperture, _)| MeasuredStar {
            x: aperture.x,
            y: aperture.y,
            hfr: aperture.hfr,
            flux: aperture.flux,
            radius: aperture.radius,
            saturated: holds_saturated_pixel(view, &aperture, &detection.hot, saturation_level),
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

/// Measure each listed star in an aperture that starts where it was last
/// seen, and keep one star per aperture.
///
/// Each aperture starts at the star's `x` and `y` with its `radius`, held
/// between [`MIN_APERTURE_RADIUS`] and the size cap, and follows the star
/// as a detection's does. One that finds no star there starts again at
/// twice the radius, up to the cap: a star that defocused since it was
/// last measured is larger than its last aperture, and a small aperture
/// on the dark centre of a defocused star finds no light. A listed star
/// is measured when its aperture settles as a star, lies inside no
/// brighter listed star's aperture, and its SNR exceeds
/// [`MIN_LISTED_SNR`]. Returns, in the order given, each listed star as
/// measured, or `None`; `background_stddev` is the frame's, for the SNR.
#[must_use]
pub fn measure_listed<T: Pixel>(
    view: &ArrayView2<T>,
    listed: &[Listed],
    hot: &[(usize, usize)],
    background_stddev: f64,
    max_area: usize,
) -> Vec<Option<MeasuredStar>> {
    let max_hfr = half_flux_radius_cap(max_area);
    let mut rejected = Rejections::default();
    let mut measured: Vec<(usize, Aperture, Verdict)> = Vec::new();
    for (index, star) in listed.iter().enumerate() {
        let (last, verdict) = follow_listed(view, star, hot, max_hfr);
        match last {
            Some(aperture) => measured.push((index, aperture, verdict)),
            None => rejected.count(verdict),
        }
    }
    let mut stars: Vec<Option<MeasuredStar>> = vec![None; listed.len()];
    for (index, aperture, verdict) in one_per_aperture(measured, &mut rejected) {
        if verdict != Verdict::Star {
            continue;
        }
        if aperture_snr(aperture.flux, aperture.radius, background_stddev) <= MIN_LISTED_SNR {
            rejected.faint = rejected.faint.saturating_add(1);
            continue;
        }
        if let Some(slot) = stars.get_mut(index) {
            *slot = Some(MeasuredStar {
                x: aperture.x,
                y: aperture.y,
                hfr: aperture.hfr,
                flux: aperture.flux,
                radius: aperture.radius,
                saturated: false,
                detection: index,
            });
        }
    }
    debug!(
        listed = listed.len(),
        measured = stars.iter().flatten().count(),
        ?rejected,
        "listed stars measured in apertures"
    );
    stars
}

/// The signal-to-noise ratio of an aperture's flux: `flux / √(flux +
/// π·radius²·σ²)`, `compute_snr`'s noise model over the aperture, with the
/// gain taken as 1 ADU/electron.
#[must_use]
pub fn aperture_snr(flux: f64, radius: f64, background_stddev: f64) -> f64 {
    let sky = PI * radius * radius;
    flux / sky
        .mul_add(background_stddev * background_stddev, flux)
        .sqrt()
}

/// The brightest raw pixel in `star`'s aperture, hot pixels left out.
/// `None` when the aperture holds no pixel of the frame.
#[must_use]
pub fn peak_in_aperture<T: Pixel>(
    view: &ArrayView2<T>,
    star: &MeasuredStar,
    hot: &[(usize, usize)],
) -> Option<f64> {
    let mut peak: Option<f64> = None;
    for_each_pixel_within(view, hot, star.x, star.y, star.radius, |_, value| {
        peak = Some(peak.map_or(value, |p| p.max(value)));
    });
    peak
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

/// Rank the measured apertures by flux and keep each one that lies inside
/// no brighter kept one's aperture, counting the rest.
fn one_per_aperture(
    mut measured: Vec<(usize, Aperture, Verdict)>,
    rejected: &mut Rejections,
) -> Vec<(usize, Aperture, Verdict)> {
    measured.sort_by(|a, b| b.1.flux.total_cmp(&a.1.flux).then(a.0.cmp(&b.0)));
    let mut kept: Vec<(usize, Aperture, Verdict)> = Vec::new();
    for candidate in measured {
        let inside = kept.iter().any(|(_, brighter, _)| {
            (candidate.1.x - brighter.x).hypot(candidate.1.y - brighter.y) <= brighter.radius
        });
        if inside {
            rejected.inside_brighter = rejected.inside_brighter.saturating_add(1);
        } else {
            rejected.count(candidate.2);
            kept.push(candidate);
        }
    }
    kept
}

/// Refine one detection's aperture round by round, from a start sized by
/// its component ([`follow`] has what it returns).
fn follow_star<T: Pixel>(
    view: &ArrayView2<T>,
    star: &Star,
    hot: &[(usize, usize)],
    background_mean: f64,
    max_hfr: f64,
) -> (Option<Aperture>, Verdict) {
    let (x, y) = (star.centroid_x, star.centroid_y);
    let component_hfr = star_hfr(view, star, background_mean).unwrap_or(0.0);
    let extent = star
        .pixels
        .iter()
        .filter_map(|&cell| distance(cell, x, y))
        .fold(0.0, f64::max);
    // The start is capped like any later radius, but is not itself a
    // verdict on size: a compact star whose component reaches far, along
    // a diffraction spike or a bloom trail, shrinks to its own aperture
    // from there.
    let radius = (APERTURE_FACTOR * component_hfr)
        .max(extent + 1.0)
        .max(MIN_APERTURE_RADIUS)
        .min(APERTURE_FACTOR * max_hfr);
    follow(view, hot, x, y, radius, max_hfr)
}

/// Refine a listed star's aperture from where it was last seen, starting
/// again at twice the radius while it finds no star, up to the cap.
fn follow_listed<T: Pixel>(
    view: &ArrayView2<T>,
    star: &Listed,
    hot: &[(usize, usize)],
    max_hfr: f64,
) -> (Option<Aperture>, Verdict) {
    let max_radius = APERTURE_FACTOR * max_hfr;
    let mut radius = star.radius.max(MIN_APERTURE_RADIUS).min(max_radius);
    loop {
        let (last, verdict) = follow(view, hot, star.x, star.y, radius, max_hfr);
        let found_nothing = matches!(
            verdict,
            Verdict::NoFlux | Verdict::Unsettled | Verdict::Floor
        );
        if !found_nothing || radius >= max_radius {
            return (last, verdict);
        }
        radius = (2.0 * radius).min(max_radius);
    }
}

/// Refine an aperture round by round from `(x, y)` and `radius`. Returns
/// how the rounds ended and the aperture claimed, if any: the one it
/// settled in, the last one it was measured in when it stopped settling,
/// or — when the size cap or the frame's edge stopped it while it grew —
/// the larger one it was growing into, with the last measured flux. An
/// aperture never measured claims nothing.
fn follow<T: Pixel>(
    view: &ArrayView2<T>,
    hot: &[(usize, usize)],
    x: f64,
    y: f64,
    radius: f64,
    max_hfr: f64,
) -> (Option<Aperture>, Verdict) {
    let max_radius = APERTURE_FACTOR * max_hfr;
    let (mut x, mut y, mut radius) = (x, y, radius);
    let mut last = None;
    for _ in 0..MAX_ROUNDS {
        let growing_into = |last: Option<Aperture>| last.map(|a| Aperture { x, y, radius, ..a });
        if radius > max_radius {
            return (growing_into(last), Verdict::TooLarge);
        }
        let Some(background) = annulus_background(view, hot, x, y, radius) else {
            return (growing_into(last), Verdict::Edge);
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
            } else if round.hfr > max_hfr {
                Verdict::TooLarge
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

/// Whether the aperture holds a pixel at or above the frame's saturation
/// level, hot pixels left out.
fn holds_saturated_pixel<T: Pixel>(
    view: &ArrayView2<T>,
    aperture: &Aperture,
    hot: &[(usize, usize)],
    saturation_level: Option<u32>,
) -> bool {
    let Some(level) = saturation_level.map(f64::from) else {
        return false;
    };
    let mut saturated = false;
    for_each_pixel_within(
        view,
        hot,
        aperture.x,
        aperture.y,
        aperture.radius,
        |_, value| saturated = saturated || value >= level,
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
    fn a_compact_star_whose_component_trails_far_is_measured_at_its_own_size() {
        // A disc of radius 3 (HFR ≈ 2.1) whose component runs on along a
        // faint trail to 30 px: its starting radius, 31 px, is past the
        // cap of `max_area` 100 (3 × 5.6 ≈ 17 px), but its own aperture
        // is not.
        let mut arr = flat(100, 100);
        add_disc(&mut arr, 50.0, 50.0, 3.0, 500);
        for c in 54..=80 {
            arr[(50, c)] += 5;
        }
        let component = pixels_near(&arr, 50.0, 50.0, 30.0, |_, _| true);
        let stars = measure(&arr, vec![detection_of(&arr, component)], 100);
        assert_eq!(stars.len(), 1, "{stars:?}");
        assert!((stars[0].hfr - 3.0 / 2_f64.sqrt()).abs() < 0.3, "{stars:?}");
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

    /// Raise every pixel between `inner` and `outer` from `(x, y)` by
    /// `height`: the ring of a defocused star with a central obstruction.
    fn add_ring(arr: &mut Array2<u16>, x: f64, y: f64, inner: f64, outer: f64, height: u16) {
        for ((r, c), v) in arr.indexed_iter_mut() {
            let d = (c as f64 - x).hypot(r as f64 - y);
            if (inner..=outer).contains(&d) {
                *v += height;
            }
        }
    }

    fn listed(x: f64, y: f64, radius: f64) -> Listed {
        Listed { x, y, radius }
    }

    /// The flat frame's noise is 0; an SNR of a listed star is then its
    /// flux's square root, far above the threshold for these discs.
    fn measure_list(
        arr: &Array2<u16>,
        stars: &[Listed],
        max_area: usize,
    ) -> Vec<Option<MeasuredStar>> {
        measure_listed(&arr.view(), stars, &[], 0.0, max_area)
    }

    #[test]
    fn a_listed_star_is_measured_where_it_was_last_seen() {
        let mut arr = flat(64, 64);
        add_disc(&mut arr, 32.0, 32.0, 4.0, 500);
        let stars = measure_list(&arr, &[listed(31.0, 33.0, 3.0)], 4096);
        let star = stars[0].expect("measured");
        assert!((star.hfr - 4.0 / 2_f64.sqrt()).abs() < 0.3, "{star:?}");
        assert!(
            (star.x - 32.0).abs() < 0.1 && (star.y - 32.0).abs() < 0.1,
            "{star:?}"
        );
    }

    #[test]
    fn a_small_aperture_on_a_rings_dark_centre_starts_again_larger() {
        // A ring from 8 to 20 px: an aperture of 3 px at its centre holds
        // no light, so it starts again at 6, 12 and 24 px. It settles near
        // 46 px, its annulus out to about 70.
        let mut arr = flat(200, 200);
        add_ring(&mut arr, 100.0, 100.0, 8.0, 20.0, 100);
        let stars = measure_list(&arr, &[listed(100.0, 100.0, 3.0)], 40_000);
        let ring = stars[0].expect("measured from a larger start");
        assert!(
            (ring.x - 100.0).abs() < 0.3 && (ring.y - 100.0).abs() < 0.3,
            "{ring:?}"
        );
        assert!(ring.hfr > 10.0 && ring.hfr < 17.0, "{ring:?}");
    }

    #[test]
    fn a_listed_place_with_no_star_is_not_measured() {
        let mut arr = flat(64, 64);
        add_disc(&mut arr, 20.0, 20.0, 3.0, 500);
        let stars = measure_list(
            &arr,
            &[listed(44.0, 44.0, 3.0), listed(-10.0, 30.0, 3.0)],
            4096,
        );
        assert_eq!(stars, [None, None]);
    }

    #[test]
    fn listed_stars_come_back_in_the_order_given() {
        let mut arr = flat(64, 96);
        add_disc(&mut arr, 24.0, 32.0, 3.0, 200);
        add_disc(&mut arr, 72.0, 32.0, 3.0, 600);
        let stars = measure_list(
            &arr,
            &[listed(24.0, 32.0, 3.0), listed(72.0, 32.0, 3.0)],
            4096,
        );
        let (faint, bright) = (stars[0].expect("faint"), stars[1].expect("bright"));
        assert!(
            (faint.x - 24.0).abs() < 0.1 && faint.flux < bright.flux,
            "{stars:?}"
        );
        assert!((bright.x - 72.0).abs() < 0.1, "{stars:?}");
    }

    #[test]
    fn two_listed_stars_on_one_disc_count_once() {
        let mut arr = flat(100, 100);
        add_disc(&mut arr, 50.0, 50.0, 12.0, 100);
        let stars = measure_list(
            &arr,
            &[listed(46.0, 50.0, 3.0), listed(55.0, 50.0, 3.0)],
            4096,
        );
        assert_eq!(stars.iter().flatten().count(), 1, "{stars:?}");
    }

    #[test]
    fn a_listed_star_at_or_below_the_snr_threshold_is_not_measured() {
        // A disc of radius 3 and height 10 has a flux of about 280; it
        // settles in an aperture of about 6 px, so a background noise of
        // 15 puts its SNR under 3 and one of 1 well over it.
        let mut arr = flat(64, 64);
        add_disc(&mut arr, 32.0, 32.0, 3.0, 10);
        let noisy = measure_listed(&arr.view(), &[listed(32.0, 32.0, 3.0)], &[], 15.0, 4096);
        assert_eq!(noisy, [None]);
        let quiet = measure_listed(&arr.view(), &[listed(32.0, 32.0, 3.0)], &[], 1.0, 4096);
        assert!(quiet[0].is_some(), "{quiet:?}");
    }

    #[test]
    fn a_listed_star_larger_than_the_cap_is_not_measured() {
        let mut arr = flat(100, 100);
        add_disc(&mut arr, 50.0, 50.0, 12.0, 100);
        assert!(measure_list(&arr, &[listed(50.0, 50.0, 3.0)], 300)[0].is_some());
        assert_eq!(measure_list(&arr, &[listed(50.0, 50.0, 3.0)], 200), [None]);
    }

    #[test]
    fn aperture_snr_is_the_flux_over_its_shot_and_sky_noise() {
        // flux 100, radius 1, σ 2: √(100 + 4π) ≈ 10.60.
        let snr = aperture_snr(100.0, 1.0, 2.0);
        assert!(
            (snr - 100.0 / (100.0 + 4.0 * PI).sqrt()).abs() < 1e-12,
            "{snr}"
        );
        // With no sky noise it is the shot noise alone.
        assert!((aperture_snr(100.0, 5.0, 0.0) - 10.0).abs() < 1e-12);
    }

    #[test]
    fn the_peak_in_an_aperture_is_its_brightest_pixel() {
        let mut arr = flat(64, 64);
        add_disc(&mut arr, 32.0, 32.0, 4.0, 500);
        arr[(33, 32)] = 2000;
        let star = MeasuredStar {
            x: 32.0,
            y: 32.0,
            radius: 6.0,
            ..star(1.0)
        };
        assert_eq!(peak_in_aperture(&arr.view(), &star, &[]), Some(2000.0));
        assert_eq!(
            peak_in_aperture(&arr.view(), &star, &[(33, 32)]),
            Some(1500.0)
        );
    }
}
