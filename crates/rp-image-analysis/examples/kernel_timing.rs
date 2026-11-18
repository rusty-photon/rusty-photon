//! Times the single-frame image kernels on synthetic guide frames.
//!
//! A guider measures every frame of a loop of short exposures, so what a
//! frame costs to measure on the guider's host decides whether the loop
//! keeps up. Each frame here is `u16`: a flat background of 1000 ADU with
//! Gaussian read noise of 20 ADU, and N Gaussian stars of FWHM 3 px whose
//! peaks spread from 20 to 2000 times the noise, built from a fixed seed so
//! every run times the same frames. The sizes are a 512 × 512 subframe and
//! the full and 2 × 2 binned frames of the QHY5III715C and QHY5III678M
//! sensors.
//!
//! For each frame every kernel runs once to warm up and then
//! [`ITERATIONS`] times on one thread, as the kernels run in `rp`, and the
//! table gives the median and the 95th percentile (nearest rank) in
//! milliseconds. The last three rows of each frame time the paths a guider
//! composes from them: star selection (background, detection and a fit per
//! detected star), and the per-frame path (background and the listed stars
//! measured where they are), alone and with the hot-pixel scan
//! `measure_listed_stars` runs before it measures.
//!
//! ```sh
//! cargo run --release -p rp-image-analysis --example kernel_timing
//! ```

use std::hint::black_box;
use std::time::Instant;

use ndarray::{Array2, ArrayView2};
use rp_image_analysis::aperture::{measure_listed, Listed, MIN_APERTURE_RADIUS};
use rp_image_analysis::background::{estimate_background, BackgroundStats};
use rp_image_analysis::fwhm::{fit_2d_gaussian, GaussianFit2D, FWHM_OVER_SIGMA};
use rp_image_analysis::hfr::star_hfr;
use rp_image_analysis::stars::{detect_stars, hot_pixels, DetectionParams, Star};

/// Timed runs of each kernel on each frame, after one warm-up run.
const ITERATIONS: usize = 30;

/// Each frame's name, width, height, number of stars and seed.
const FRAMES: [(&str, usize, usize, usize, u64); 5] = [
    ("subframe 512x512", 512, 512, 5, 1),
    ("715C bin2 1932x1096", 1932, 1096, 30, 2),
    ("715C full 3864x2192", 3864, 2192, 30, 3),
    ("678M bin2 1928x1090", 1928, 1090, 30, 4),
    ("678M full 3856x2180", 3856, 2180, 30, 5),
];

const BACKGROUND_ADU: f64 = 1000.0;
const READ_NOISE_ADU: f64 = 20.0;
const FWHM_PX: f64 = 3.0;

/// The faintest and the brightest star's peak above the background (ADU).
const PEAKS_ADU: (f64, f64) = (400.0, 40_000.0);

/// How close a star comes to the frame's edge and to another star (px).
const MARGIN_PX: usize = 40;

/// How far from its centre a star is drawn (px): over nine sigmas.
const REACH_PX: usize = 12;

/// The threshold and smoothing `rp`'s tools default to, the camera's
/// 16-bit ceiling as the saturation level, and areas (which have no
/// default) sized for stars a few pixels across.
const PARAMS: DetectionParams = DetectionParams {
    threshold_sigma: 5.0,
    smoothing_sigma: 1.0,
    min_area: 5,
    max_area: 1000,
    saturation_level: Some(65_535),
};

/// The largest half-flux disc a listed star is measured in (px²).
const MAX_HALF_FLUX_AREA: usize = 1000;

/// `measure_stars`' initial sigma and default stamp half-size for a fit.
const FIT_SIGMA: f64 = 1.5;
const STAMP_HALF_SIZE: usize = 8;

fn main() {
    println!(
        "rp kernel timing: {ITERATIONS} runs after one warm-up, one thread, {}",
        std::env::consts::ARCH
    );
    println!();
    println!("| frame | kernel | count | median ms | p95 ms |");
    println!("|---|---|---:|---:|---:|");
    for (name, width, height, stars, seed) in FRAMES {
        let (frame, places) = synthetic_frame(width, height, stars, seed);
        time_frame(name, &frame.view(), &places);
    }
}

/// Times every kernel on `view`, whose stars are at `places` (column,
/// row), and prints a row for each.
fn time_frame(name: &str, view: &ArrayView2<u16>, places: &[(f64, f64)]) {
    let row = |kernel: &str, count: &str, (median, p95): (f64, f64)| {
        println!("| {name} | {kernel} | {count} | {median:.3} | {p95:.3} |");
    };
    let Some(background) = estimate_background(view) else {
        println!("| {name} | estimate_background | failed | | |");
        return;
    };
    let hot = hot_pixels(view, &background, PARAMS.threshold_sigma);
    let stars = detect_stars(view, &background, &PARAMS);
    let fitted = stars
        .iter()
        .filter_map(|star| fit(view, star, &background))
        .count();
    let listed = listed_where_measured(view, places, &hot, &background);
    let measured = measure_listed(view, &listed, &hot, background.stddev, MAX_HALF_FLUX_AREA)
        .iter()
        .flatten()
        .count();

    row(
        "estimate_background",
        "",
        time(|| estimate_background(black_box(view))),
    );
    row(
        "hot_pixels",
        &hot.len().to_string(),
        time(|| hot_pixels(black_box(view), &background, PARAMS.threshold_sigma)),
    );
    row(
        "detect_stars",
        &format!("{} of {}", stars.len(), places.len()),
        time(|| detect_stars(black_box(view), &background, &PARAMS)),
    );
    row(
        "fit_2d_gaussian, each detected",
        &format!("{fitted} of {}", stars.len()),
        time(|| {
            stars
                .iter()
                .filter_map(|star| fit(black_box(view), star, &background))
                .count()
        }),
    );
    row(
        "measure_listed",
        &format!("{measured} of {}", listed.len()),
        time(|| {
            measure_listed(
                black_box(view),
                &listed,
                &hot,
                background.stddev,
                MAX_HALF_FLUX_AREA,
            )
        }),
    );
    row(
        "star_hfr, each detected",
        &stars.len().to_string(),
        time(|| {
            stars
                .iter()
                .filter_map(|star| star_hfr(black_box(view), star, background.mean))
                .count()
        }),
    );
    row(
        "per frame: background + listed",
        "",
        time(|| per_frame(view, &listed, Some(hot.as_slice()))),
    );
    row(
        "per frame, with hot_pixels",
        "",
        time(|| per_frame(view, &listed, None)),
    );
    row(
        "selection: background + detect + fits",
        "",
        time(|| {
            let background = estimate_background(black_box(view))?;
            let stars = detect_stars(view, &background, &PARAMS);
            Some(
                stars
                    .iter()
                    .filter_map(|star| fit(view, star, &background))
                    .count(),
            )
        }),
    );
}

/// The guider's per-frame measurement: the frame's background, then the
/// listed stars where they are. Without `hot`, the hot pixels come from a
/// scan of the frame, as `measure_listed_stars` finds them.
fn per_frame(
    view: &ArrayView2<u16>,
    listed: &[Listed],
    hot: Option<&[(usize, usize)]>,
) -> Option<usize> {
    let background = estimate_background(black_box(view))?;
    let scanned;
    let hot = if let Some(hot) = hot {
        hot
    } else {
        scanned = hot_pixels(view, &background, PARAMS.threshold_sigma);
        &scanned
    };
    let measured = measure_listed(view, listed, hot, background.stddev, MAX_HALF_FLUX_AREA);
    Some(measured.iter().flatten().count())
}

/// The stars at `places` as a guider lists them after the frame before:
/// at their places, each in the aperture it measured in there.
fn listed_where_measured(
    view: &ArrayView2<u16>,
    places: &[(f64, f64)],
    hot: &[(usize, usize)],
    background: &BackgroundStats,
) -> Vec<Listed> {
    let first: Vec<Listed> = places
        .iter()
        .map(|&(x, y)| Listed {
            x,
            y,
            radius: MIN_APERTURE_RADIUS,
        })
        .collect();
    let measured = measure_listed(view, &first, hot, background.stddev, MAX_HALF_FLUX_AREA);
    first
        .iter()
        .zip(measured)
        .map(|(&listed, star)| Listed {
            radius: star.map_or(listed.radius, |star| star.radius),
            ..listed
        })
        .collect()
}

/// A Gaussian fit to `star`, seeded as `measure_stars` seeds it.
fn fit(view: &ArrayView2<u16>, star: &Star, background: &BackgroundStats) -> Option<GaussianFit2D> {
    fit_2d_gaussian(
        view,
        star.centroid_x,
        star.centroid_y,
        star.peak,
        background.mean,
        FIT_SIGMA,
        STAMP_HALF_SIZE,
    )
}

/// Runs `kernel` once to warm up and then [`ITERATIONS`] times, and
/// returns the median and the 95th percentile of its run time (ms).
fn time<R>(mut kernel: impl FnMut() -> R) -> (f64, f64) {
    black_box(kernel());
    let mut ms: Vec<f64> = (0..ITERATIONS)
        .map(|_| {
            let start = Instant::now();
            black_box(kernel());
            start.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    (nearest_rank(&ms, 50), nearest_rank(&ms, 95))
}

/// The `percent`-th percentile of the sorted `samples`, by nearest rank.
fn nearest_rank(samples: &[f64], percent: usize) -> f64 {
    let rank = samples.len().saturating_mul(percent).div_ceil(100);
    samples
        .get(rank.saturating_sub(1))
        .copied()
        .unwrap_or(f64::NAN)
}

/// A `width` × `height` frame from `seed`, and where its stars are
/// (column, row): the background with read noise, and `stars` stars, the
/// faintest first, each at least [`MARGIN_PX`] from the edge and from the
/// others.
fn synthetic_frame(
    width: usize,
    height: usize,
    stars: usize,
    seed: u64,
) -> (Array2<u16>, Vec<(f64, f64)>) {
    let mut rng = SplitMix64(seed);
    let mut frame = Array2::from_shape_simple_fn((height, width), || {
        READ_NOISE_ADU.mul_add(rng.gaussian(), BACKGROUND_ADU)
    });

    let mut centres: Vec<(usize, usize)> = Vec::with_capacity(stars);
    for _ in 0..stars.saturating_mul(1000) {
        if centres.len() == stars {
            break;
        }
        let column = rng.between(MARGIN_PX, width.saturating_sub(MARGIN_PX));
        let row = rng.between(MARGIN_PX, height.saturating_sub(MARGIN_PX));
        if centres
            .iter()
            .all(|&(c, r)| c.abs_diff(column).max(r.abs_diff(row)) >= MARGIN_PX)
        {
            centres.push((column, row));
        }
    }

    let sigma = FWHM_PX / FWHM_OVER_SIGMA;
    let last = to_f64(centres.len().saturating_sub(1)).max(1.0);
    let mut places = Vec::with_capacity(centres.len());
    for (index, &(column, row)) in centres.iter().enumerate() {
        let peak = PEAKS_ADU.0 * (PEAKS_ADU.1 / PEAKS_ADU.0).powf(to_f64(index) / last);
        let x = to_f64(column) + rng.uniform();
        let y = to_f64(row) + rng.uniform();
        for r in row.saturating_sub(REACH_PX)..=row.saturating_add(REACH_PX) {
            for c in column.saturating_sub(REACH_PX)..=column.saturating_add(REACH_PX) {
                let dx = to_f64(c) - x;
                let dy = to_f64(r) - y;
                let profile = (-dx.mul_add(dx, dy * dy) / (2.0 * sigma * sigma)).exp();
                if let Some(value) = frame.get_mut((r, c)) {
                    *value = peak.mul_add(profile, *value);
                }
            }
        }
        places.push((x, y));
    }
    (frame.mapv(adu), places)
}

/// `value` rounded to the nearest ADU a 16-bit camera can report.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is rounded and clamped to the u16 range first, and no f64-to-u16 conversion exists but `as`"
)]
fn adu(value: f64) -> u16 {
    value.round().clamp(0.0, f64::from(u16::MAX)) as u16
}

/// A pixel index or count as `f64`; frame sizes fit `u32`.
fn to_f64(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// `SplitMix64`, a small seeded generator, so every run builds the same
/// frames.
struct SplitMix64(u64);

impl SplitMix64 {
    const fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1): the top 52 bits as the mantissa of a number in
    /// [1, 2), less one.
    fn uniform(&mut self) -> f64 {
        f64::from_bits(0x3FF0_0000_0000_0000 | (self.next() >> 12)) - 1.0
    }

    /// Standard normal, by the Box–Muller transform.
    fn gaussian(&mut self) -> f64 {
        let u1 = 1.0 - self.uniform();
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }

    /// Uniform in [`low`, `high`), or `low` when the range is empty.
    fn between(&mut self, low: usize, high: usize) -> usize {
        let span = u64::try_from(high.saturating_sub(low)).unwrap_or(u64::MAX);
        let offset = self.next().checked_rem(span).unwrap_or(0);
        low.saturating_add(usize::try_from(offset).unwrap_or(0))
    }
}
