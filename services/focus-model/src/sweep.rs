//! The V-curve sweep (docs/services/focus-model.md § The sweep).
//!
//! The grid and its clamp, the sparse gate, the weighted parabola, the
//! confirmation frame and the bounded retry — the semantics `rp`'s
//! capture sweep has today, run here through `rp`'s primitives. The
//! measurement side stays `rp`'s: this module talks to it through
//! [`SweepOps`], which `workflow.rs` implements over the MCP client and
//! the unit tests implement over recorded curves.

use std::cmp::Ordering;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::FocusModelError;

/// The most grid positions a sweep may walk: a guardrail against a
/// configuration that would tie the rig up for hours.
pub const MAX_GRID_POINTS: usize = 1000;

/// Why a sweep sample was excluded from the fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    /// The sample's star count fell below the sparse gate.
    Sparse,
}

/// One measured point of the sweep, as the record keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint {
    pub position: i32,
    /// `None` for a starless frame or a non-finite reading.
    pub hfr: Option<f64>,
    pub star_count: u32,
    #[serde(default)]
    pub document_id: String,
    /// `None` for a sample that entered the fit; `Some` names why an
    /// otherwise measurable sample was left out.
    #[serde(default)]
    pub rejected: Option<Rejection>,
}

impl CurvePoint {
    /// The `(position, hfr, star_count)` triple the fit consumes, or
    /// `None` for a starless or rejected sample.
    #[must_use]
    pub const fn accepted_sample(&self) -> Option<(i32, f64, u32)> {
        match (self.hfr, self.rejected) {
            (Some(hfr), None) => Some((self.position, hfr, self.star_count)),
            _ => None,
        }
    }
}

/// What one grid position measured.
#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    pub hfr: Option<f64>,
    pub star_count: u32,
    pub document_id: String,
}

/// The frame captured at the fitted position, and its verdict.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Confirmation {
    pub document_id: String,
    pub hfr: Option<f64>,
    pub star_count: u32,
    pub accepted: bool,
}

/// Walk order of the sweep grid: the focuser's backlash approach
/// direction, so every sample is reached from the side of the final
/// move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// Smallest position first — the order for an outward approach.
    #[default]
    Ascending,
    /// Largest position first — the order for an inward approach.
    Descending,
}

/// What the sweep needs beyond the grid's centre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SweepParams {
    pub step_size: i32,
    pub half_width: i32,
    pub min_fit_points: usize,
    pub min_star_fraction: f64,
    pub confirmation_tolerance: f64,
    pub max_attempts: u32,
    pub direction: Direction,
    pub min_position: Option<i32>,
    pub max_position: Option<i32>,
}

impl SweepParams {
    const fn bounds(self) -> (Option<i32>, Option<i32>) {
        (self.min_position, self.max_position)
    }
}

/// Why a sweep's accepted samples produced no trusted vertex.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FitError {
    #[error(
        "not enough stars: only {got} of {needed} required samples are accepted \
         (non-null HFR, past the sparse gate)"
    )]
    NotEnoughStars { got: usize, needed: usize },
    #[error("monotonic curve: {0}")]
    MonotonicCurve(String),
}

impl FitError {
    /// The name this failure is recorded under.
    #[must_use]
    pub const fn outcome(&self) -> &'static str {
        match self {
            Self::NotEnoughStars { .. } => "not_enough_stars",
            Self::MonotonicCurve(_) => "monotonic_curve",
        }
    }
}

/// How a sweep ended when it did not end in a curve.
#[derive(Debug, thiserror::Error)]
pub enum SweepFailure {
    /// Every permitted attempt failed to fit; the run's curve rides
    /// along so it is diagnosable without re-measuring.
    #[error("{error}")]
    Fit {
        error: FitError,
        attempts: u32,
        curve_points: Vec<CurvePoint>,
    },
    /// A grid that cannot be walked, before any motion.
    #[error("{0}")]
    Grid(String),
    /// A primitive call failed or the caller cancelled, with whatever
    /// the run had measured by then.
    #[error("{error}")]
    Rig {
        error: FocusModelError,
        curve_points: Vec<CurvePoint>,
    },
}

/// A sweep that produced a trusted position.
#[derive(Debug, Clone, PartialEq)]
pub struct SweepOutcome {
    /// Where the focuser was left and what was measured there.
    pub position: i32,
    pub hfr: f64,
    pub best_position: i32,
    pub best_hfr: f64,
    pub fit_r_squared: f64,
    pub samples_used: usize,
    pub attempts: u32,
    pub wing_slope: Option<f64>,
    pub confirmed: bool,
    pub confirmation: Confirmation,
    pub curve_points: Vec<CurvePoint>,
}

/// What the sweep needs from `rp`.
///
/// Every fallible method returns the primitive's own failure, or the
/// caller's cancellation, as a [`FocusModelError`].
#[async_trait]
#[expect(
    clippy::missing_errors_doc,
    reason = "the sentence above covers every method here; a per-method # Errors section would only repeat it"
)]
pub trait SweepOps: Send + Sync {
    /// Move the focuser and return the read-back position.
    async fn move_focuser(&self, position: i32) -> Result<i32, FocusModelError>;
    /// Capture and measure at the current position.
    async fn measure(&self) -> Result<Measurement, FocusModelError>;
    /// The caller's cancellation, checked between primitive calls.
    fn check_cancelled(&self) -> Result<(), FocusModelError>;
    /// One `notifications/progress` tick.
    async fn tick(&self, position: i32, measurement: &Measurement);
}

/// A measured HFR the fit may use: `None` for a starless frame and for
/// a non-finite reading, which would poison the lowest-sample choice,
/// the parabola and the wing slope.
#[must_use]
pub fn finite_hfr(hfr: Option<f64>) -> Option<f64> {
    hfr.filter(|value| value.is_finite())
}

/// The sparse gate's threshold for a sweep whose densest frame counted
/// `max_stars` stars.
#[must_use]
pub fn sparse_threshold(max_stars: u32, min_star_fraction: f64) -> f64 {
    min_star_fraction * f64::from(max_stars)
}

/// Mark every measurable sample below the gate as [`Rejection::Sparse`]
/// and return the threshold applied, which the confirmation frame is
/// held to as well.
pub fn apply_sparse_gate(points: &mut [CurvePoint], min_star_fraction: f64) -> f64 {
    let max_stars = points
        .iter()
        .filter(|p| p.hfr.is_some())
        .map(|p| p.star_count)
        .max()
        .unwrap_or(0);
    let threshold = sparse_threshold(max_stars, min_star_fraction);
    for point in points.iter_mut().filter(|p| p.hfr.is_some()) {
        if f64::from(point.star_count) < threshold {
            point.rejected = Some(Rejection::Sparse);
        }
    }
    threshold
}

/// The lowest-HFR sample; ties go to the denser frame.
#[must_use]
pub fn lowest_sample(samples: &[(i32, f64, u32)]) -> Option<(i32, f64, u32)> {
    samples.iter().copied().min_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.2.cmp(&a.2))
    })
}

/// The confirmation verdict: the frame has stars, passes the sweep's
/// gate, and measures no worse than the best accepted sample by more
/// than `tolerance`.
#[must_use]
pub fn confirmation_accepted(
    hfr: Option<f64>,
    star_count: u32,
    gate_threshold: f64,
    lowest_hfr: f64,
    tolerance: f64,
) -> bool {
    hfr.is_some_and(|hfr| {
        f64::from(star_count) >= gate_threshold && hfr <= lowest_hfr * (1.0 + tolerance)
    })
}

/// Ordinary least-squares slope of HFR against position, in pixels per
/// focuser step; `None` with fewer than two samples or no spread.
fn line_slope(samples: &[(i32, f64)]) -> Option<f64> {
    if samples.len() < 2 {
        return None;
    }
    let mut n = 0.0_f64;
    let mut sum_x = 0.0_f64;
    let mut sum_y = 0.0_f64;
    for (x, y) in samples {
        n += 1.0;
        sum_x += f64::from(*x);
        sum_y += y;
    }
    let mean_x = sum_x / n;
    let mean_y = sum_y / n;
    let mut sxx = 0.0_f64;
    let mut sxy = 0.0_f64;
    for (x, y) in samples {
        let dx = f64::from(*x) - mean_x;
        sxx = dx.mul_add(dx, sxx);
        sxy = dx.mul_add(y - mean_y, sxy);
    }
    if sxx > 0.0 {
        Some(sxy / sxx)
    } else {
        None
    }
}

/// The V's steepness in HFR pixels per 100 focuser steps: the steeper
/// of the two wings, each the accepted samples strictly on one side of
/// the lowest accepted sample.
#[must_use]
pub fn wing_slope(points: &[CurvePoint]) -> Option<f64> {
    let accepted: Vec<(i32, f64, u32)> = points
        .iter()
        .filter_map(CurvePoint::accepted_sample)
        .collect();
    let (lowest_position, _, _) = lowest_sample(&accepted)?;
    let wing = |on_side: fn(i32, i32) -> bool| {
        let samples: Vec<(i32, f64)> = accepted
            .iter()
            .filter(|(position, _, _)| on_side(*position, lowest_position))
            .map(|(position, hfr, _)| (*position, *hfr))
            .collect();
        line_slope(&samples).map(|slope| slope.abs() * 100.0)
    };
    match (
        wing(|position, lowest| position < lowest),
        wing(|position, lowest| position > lowest),
    ) {
        (Some(inner), Some(outer)) => Some(inner.max(outer)),
        (inner, outer) => inner.or(outer),
    }
}

/// Where the next attempt's grid is centred after a failed fit.
///
/// The same place after `not_enough_stars`; after `monotonic_curve`,
/// moved by `half_width` toward the lowest accepted sample, clamped to
/// the focuser's bounds.
#[must_use]
pub fn retry_centre(
    centre: i32,
    error: &FitError,
    curve_points: &[CurvePoint],
    half_width: i32,
    bounds: (Option<i32>, Option<i32>),
) -> i32 {
    let FitError::MonotonicCurve(_) = error else {
        return centre;
    };
    let accepted: Vec<(i32, f64, u32)> = curve_points
        .iter()
        .filter_map(CurvePoint::accepted_sample)
        .collect();
    let Some((lowest_position, _, _)) = lowest_sample(&accepted) else {
        return centre;
    };
    let shifted = match lowest_position.cmp(&centre) {
        Ordering::Less => centre.saturating_sub(half_width),
        Ordering::Greater => centre.saturating_add(half_width),
        Ordering::Equal => centre,
    };
    let shifted = bounds.0.map_or(shifted, |min| shifted.max(min));
    bounds.1.map_or(shifted, |max| shifted.min(max))
}

/// Build the grid `[centre − half_width, centre + half_width]` in
/// `step` increments, clamped to the bounds.
///
/// Out-of-range points are dropped, not coerced: coercion would
/// produce duplicate samples at a bound and distort the fit. The walk
/// stops after [`MAX_GRID_POINTS`] positions whether or not the bounds
/// kept them, so a half width near the `i32` rail cannot spin here;
/// [`sweep_grid`] refuses such a sweep before calling.
#[must_use]
pub fn build_grid(
    centre: i32,
    step: i32,
    half_width: i32,
    bounds: (Option<i32>, Option<i32>),
) -> Vec<i32> {
    let start = centre.saturating_sub(half_width);
    let end = centre.saturating_add(half_width);
    let mut grid = Vec::new();
    let mut visited: usize = 0;
    let mut p = start;
    loop {
        let in_min = bounds.0.is_none_or(|min| p >= min);
        let in_max = bounds.1.is_none_or(|max| p <= max);
        if in_min && in_max {
            grid.push(p);
        }
        visited = visited.saturating_add(1);
        let next = p.saturating_add(step);
        if p == end || next <= p || next > end || visited > MAX_GRID_POINTS {
            break;
        }
        p = next;
    }
    grid
}

/// How many positions a `centre ± half_width` walk visits at
/// `step_size`, before any clamping. The bounds can only drop points,
/// never lower the cost of finding them.
#[must_use]
pub fn planned_points(half_width: i32, step_size: i32) -> usize {
    let span = i64::from(half_width).saturating_mul(2).max(0);
    let step = i64::from(step_size).max(1);
    let points = span.checked_div(step).unwrap_or(0).saturating_add(1);
    usize::try_from(points).unwrap_or(usize::MAX)
}

/// How many positions the sweep will walk from `centre`, after the
/// bounds have had their say: what a progress total counts, and one
/// attempt's worth of frames.
#[must_use]
pub fn grid_length(centre: i32, params: SweepParams) -> u32 {
    let walked = build_grid(centre, params.step_size, params.half_width, params.bounds()).len();
    u32::try_from(walked).unwrap_or(u32::MAX)
}

/// The walk-ordered grid around `centre`.
fn sweep_grid(centre: i32, params: SweepParams) -> Result<Vec<i32>, SweepFailure> {
    let planned = planned_points(params.half_width, params.step_size);
    if planned > MAX_GRID_POINTS {
        return Err(SweepFailure::Grid(format!(
            "the sweep grid would hold {planned} positions, more than the cap of \
             {MAX_GRID_POINTS} (raise step_size or lower half_width)"
        )));
    }
    let mut grid = build_grid(centre, params.step_size, params.half_width, params.bounds());
    if grid.len() < params.min_fit_points {
        return Err(SweepFailure::Grid(format!(
            "the sweep grid holds {} positions after clamping to the focuser's bounds; \
             min_fit_points is {}",
            grid.len(),
            params.min_fit_points
        )));
    }
    if params.direction == Direction::Descending {
        grid.reverse();
    }
    Ok(grid)
}

/// Result of fitting `hfr = a·x'² + b·x' + c` where `x' = x − offset_x`.
#[derive(Debug, Clone, Copy)]
pub struct ParabolaFit {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub offset_x: f64,
    /// Weighted coefficient of determination, clamped to `[0, 1]`.
    pub r_squared: f64,
}

impl ParabolaFit {
    #[must_use]
    pub fn vertex_position(&self) -> i32 {
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            reason = "`f64` to `i32` has no total spelling; `as` saturates at the rails, and the caller's grid-range check rejects a rail-hitting vertex"
        )]
        let vertex = (-self.b / (2.0 * self.a) + self.offset_x).round() as i32;
        vertex
    }

    #[must_use]
    pub fn vertex_value(&self) -> f64 {
        self.c - (self.b * self.b) / (4.0 * self.a)
    }
}

/// Determinant of a 3×3 matrix given by rows.
const fn det3(r0: [f64; 3], r1: [f64; 3], r2: [f64; 3]) -> f64 {
    r0[0] * (r1[1] * r2[2] - r1[2] * r2[1]) - r0[1] * (r1[0] * r2[2] - r1[2] * r2[0])
        + r0[2] * (r1[0] * r2[1] - r1[1] * r2[0])
}

/// The weighted normal-equation moments of the recentred samples.
struct Moments {
    m4: f64,
    m3: f64,
    m2: f64,
    m1: f64,
    m0: f64,
    t2: f64,
    t1: f64,
    t0: f64,
}

fn moments(filtered: &[(f64, f64, f64)], offset_x: f64) -> Moments {
    let mut m = Moments {
        m4: 0.0,
        m3: 0.0,
        m2: 0.0,
        m1: 0.0,
        m0: 0.0,
        t2: 0.0,
        t1: 0.0,
        t0: 0.0,
    };
    for (x, y, w) in filtered {
        let xc = x - offset_x;
        let xc2 = xc * xc;
        let xc3 = xc2 * xc;
        let xc4 = xc3 * xc;
        m.m4 = w.mul_add(xc4, m.m4);
        m.m3 = w.mul_add(xc3, m.m3);
        m.m2 = w.mul_add(xc2, m.m2);
        m.m1 = w.mul_add(xc, m.m1);
        m.m0 += w;
        m.t2 = (w * xc2).mul_add(*y, m.t2);
        m.t1 = (w * xc).mul_add(*y, m.t1);
        m.t0 = w.mul_add(*y, m.t0);
    }
    m
}

/// Weighted least-squares fit of a parabola to `(position, hfr, weight)`
/// samples, the weight being the frame's star count.
///
/// The fit runs in a frame recentred on the weighted mean position: at
/// real focuser scales the fourth moment otherwise overflows f64's
/// working precision and a perfectly fittable V is rejected as
/// singular.
///
/// # Errors
///
/// [`FitError::NotEnoughStars`] with fewer than three weighted samples,
/// [`FitError::MonotonicCurve`] when the curve has no minimum or the
/// design matrix is too ill-conditioned to invert.
pub fn fit_parabola(samples: &[(i32, f64, u32)]) -> Result<ParabolaFit, FitError> {
    let filtered: Vec<(f64, f64, f64)> = samples
        .iter()
        .filter(|(_, _, w)| *w > 0)
        .map(|(x, y, w)| (f64::from(*x), *y, f64::from(*w)))
        .collect();
    if filtered.len() < 3 {
        return Err(FitError::NotEnoughStars {
            got: filtered.len(),
            needed: 3,
        });
    }
    let total_w: f64 = filtered.iter().map(|(_, _, w)| *w).sum();
    let offset_x: f64 = filtered.iter().map(|(x, _, w)| w * x).sum::<f64>() / total_w;
    let m = moments(&filtered, offset_x);

    let det = det3([m.m4, m.m3, m.m2], [m.m3, m.m2, m.m1], [m.m2, m.m1, m.m0]);
    let det_scale = (m.m4.abs() * m.m2.abs() * m.m0.abs()).max(1.0);
    if det.abs() < det_scale * 1e-12 {
        return Err(FitError::MonotonicCurve(format!(
            "design matrix is singular (det={det:.3e}, scale={det_scale:.3e})"
        )));
    }
    let a = det3([m.t2, m.m3, m.m2], [m.t1, m.m2, m.m1], [m.t0, m.m1, m.m0]) / det;
    let b = det3([m.m4, m.t2, m.m2], [m.m3, m.t1, m.m1], [m.m2, m.t0, m.m0]) / det;
    let c = det3([m.m4, m.m3, m.t2], [m.m3, m.m2, m.t1], [m.m2, m.m1, m.t0]) / det;
    if a <= 0.0 {
        return Err(FitError::MonotonicCurve(format!(
            "non-positive leading coefficient (a={a:.3e})"
        )));
    }

    let y_mean = m.t0 / m.m0;
    let mut ss_res = 0.0;
    let mut ss_tot = 0.0;
    for (x, y, w) in &filtered {
        let xc = x - offset_x;
        let fitted = (a * xc).mul_add(xc, b.mul_add(xc, c));
        let residual = y - fitted;
        let spread = y - y_mean;
        ss_res = (w * residual).mul_add(residual, ss_res);
        ss_tot = (w * spread).mul_add(spread, ss_tot);
    }
    let r_squared = if ss_tot > 0.0 {
        (1.0 - ss_res / ss_tot).clamp(0.0, 1.0)
    } else {
        0.0
    };
    Ok(ParabolaFit {
        a,
        b,
        c,
        offset_x,
        r_squared,
    })
}

/// What the gate-and-fit stage hands to the confirmation stage.
#[derive(Debug, Clone, Copy)]
struct FitStage {
    best_position: i32,
    best_hfr: f64,
    r_squared: f64,
    gate_threshold: f64,
    lowest_position: i32,
    lowest_hfr: f64,
    samples_used: usize,
}

/// Gate the sweep, fit the accepted samples, and validate the vertex
/// against the grid. Pure: the caller decides what to do on failure.
fn gate_and_fit(
    curve_points: &mut [CurvePoint],
    grid: &[i32],
    params: SweepParams,
) -> Result<FitStage, FitError> {
    let gate_threshold = apply_sparse_gate(curve_points, params.min_star_fraction);
    let accepted: Vec<(i32, f64, u32)> = curve_points
        .iter()
        .filter_map(CurvePoint::accepted_sample)
        .collect();
    debug!(
        gate_threshold,
        accepted = accepted.len(),
        rejected_sparse = curve_points.iter().filter(|p| p.rejected.is_some()).count(),
        "sparse gate applied"
    );
    if accepted.len() < params.min_fit_points {
        return Err(FitError::NotEnoughStars {
            got: accepted.len(),
            needed: params.min_fit_points,
        });
    }

    let fit = fit_parabola(&accepted)?;
    let best_position = fit.vertex_position();
    let (Some(&grid_min), Some(&grid_max)) = (grid.iter().min(), grid.iter().max()) else {
        return Err(FitError::MonotonicCurve(
            "grid is empty despite having accepted samples".to_owned(),
        ));
    };
    if best_position < grid_min || best_position > grid_max {
        return Err(FitError::MonotonicCurve(format!(
            "fitted vertex {best_position} is outside sampled grid [{grid_min}, {grid_max}]"
        )));
    }
    let Some((lowest_position, lowest_hfr, _)) = lowest_sample(&accepted) else {
        return Err(FitError::MonotonicCurve(
            "no accepted sample to hold the confirmation against".to_owned(),
        ));
    };
    Ok(FitStage {
        best_position,
        best_hfr: fit.vertex_value(),
        r_squared: fit.r_squared,
        gate_threshold,
        lowest_position,
        lowest_hfr,
        samples_used: accepted.len(),
    })
}

/// One walk of `grid`: move, capture, measure at every position,
/// pushing each measured point into `curve_points` as it is taken so a
/// failure part-way leaves the caller the samples it already has.
async fn walk<O: SweepOps + ?Sized>(
    ops: &O,
    grid: &[i32],
    curve_points: &mut Vec<CurvePoint>,
) -> Result<(), FocusModelError> {
    for position in grid {
        ops.check_cancelled()?;
        ops.move_focuser(*position).await?;
        ops.check_cancelled()?;
        let measurement = ops.measure().await?;
        ops.tick(*position, &measurement).await;
        curve_points.push(CurvePoint {
            position: *position,
            hfr: finite_hfr(measurement.hfr),
            star_count: measurement.star_count,
            document_id: measurement.document_id,
            rejected: None,
        });
    }
    Ok(())
}

/// Move to the fitted vertex, measure the confirmation frame, and
/// settle on the better of the two measured positions.
async fn confirm<O: SweepOps + ?Sized>(
    ops: &O,
    params: SweepParams,
    stage: FitStage,
) -> Result<(Confirmation, i32, f64), FocusModelError> {
    ops.check_cancelled()?;
    let moved_to = ops.move_focuser(stage.best_position).await?;
    ops.check_cancelled()?;
    let measurement = ops.measure().await?;
    ops.tick(moved_to, &measurement).await;
    let hfr = finite_hfr(measurement.hfr);
    let accepted = confirmation_accepted(
        hfr,
        measurement.star_count,
        stage.gate_threshold,
        stage.lowest_hfr,
        params.confirmation_tolerance,
    );
    debug!(
        best_position = stage.best_position,
        confirmation_hfr = ?hfr,
        confirmation_stars = measurement.star_count,
        lowest_position = stage.lowest_position,
        lowest_hfr = stage.lowest_hfr,
        accepted,
        "confirmation frame measured"
    );
    let confirmation = Confirmation {
        document_id: measurement.document_id,
        hfr,
        star_count: measurement.star_count,
        accepted,
    };
    let (position, final_hfr) = if let (true, Some(hfr)) = (accepted, hfr) {
        (moved_to, hfr)
    } else {
        ops.check_cancelled()?;
        let position = ops.move_focuser(stage.lowest_position).await?;
        (position, stage.lowest_hfr)
    };
    Ok((confirmation, position, final_hfr))
}

/// Run the V-curve around `centre`: walk, gate, fit, confirm, and
/// retry a failed fit while attempts remain.
///
/// Every point the run measures is kept, attempt by attempt, and rides
/// out on the outcome or the failure: a run that a device error or a
/// cancellation stopped half way is still recorded with the samples it
/// took, which is what the morning after is read from.
///
/// The focuser is left where the sweep ended; putting it back after a
/// failure is the caller's business, because only the caller knows
/// where the run started.
///
/// # Errors
///
/// [`SweepFailure::Grid`] before any motion when the grid cannot be
/// walked, [`SweepFailure::Fit`] when every attempt failed to fit, and
/// [`SweepFailure::Rig`] for a primitive failure or the caller's
/// cancellation.
pub async fn run_sweep<O: SweepOps + ?Sized>(
    ops: &O,
    centre: i32,
    params: SweepParams,
) -> Result<SweepOutcome, SweepFailure> {
    let mut grid = sweep_grid(centre, params)?;
    let mut centre = centre;
    let mut attempts: u32 = 0;
    let mut measured: Vec<CurvePoint> = Vec::new();
    loop {
        attempts = attempts.saturating_add(1);
        debug!(
            attempt = attempts,
            max_attempts = params.max_attempts,
            centre,
            grid_len = grid.len(),
            "sweep starting"
        );
        let mut curve_points = Vec::with_capacity(grid.len());
        if let Err(error) = walk(ops, &grid, &mut curve_points).await {
            measured.append(&mut curve_points);
            return Err(SweepFailure::Rig {
                error,
                curve_points: measured,
            });
        }

        let error = match gate_and_fit(&mut curve_points, &grid, params) {
            Ok(stage) => {
                let wing_slope = wing_slope(&curve_points);
                let confirmed = confirm(ops, params, stage).await;
                measured.append(&mut curve_points);
                let (confirmation, position, hfr) = match confirmed {
                    Ok(confirmed) => confirmed,
                    Err(error) => {
                        return Err(SweepFailure::Rig {
                            error,
                            curve_points: measured,
                        })
                    }
                };
                return Ok(SweepOutcome {
                    position,
                    hfr,
                    best_position: stage.best_position,
                    best_hfr: stage.best_hfr,
                    fit_r_squared: stage.r_squared,
                    samples_used: stage.samples_used,
                    attempts,
                    wing_slope,
                    confirmed: confirmation.accepted,
                    confirmation,
                    curve_points: measured,
                });
            }
            Err(error) => error,
        };

        // The retry reads the attempt that just failed, before its
        // points join the run's.
        let next_centre = (attempts < params.max_attempts).then(|| {
            retry_centre(
                centre,
                &error,
                &curve_points,
                params.half_width,
                params.bounds(),
            )
        });
        measured.append(&mut curve_points);
        if let Some(next_centre) = next_centre {
            match sweep_grid(next_centre, params) {
                Ok(next_grid) => {
                    warn!(
                        error = %error,
                        attempt = attempts,
                        from_centre = centre,
                        to_centre = next_centre,
                        "fit failed; repeating the sweep"
                    );
                    centre = next_centre;
                    grid = next_grid;
                    continue;
                }
                Err(too_small) => debug!(
                    error = %too_small,
                    to_centre = next_centre,
                    "retry abandoned: the shifted grid cannot be walked"
                ),
            }
        }
        return Err(SweepFailure::Fit {
            error,
            attempts,
            curve_points: measured,
        });
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn params() -> SweepParams {
        SweepParams {
            step_size: 10,
            half_width: 40,
            min_fit_points: 5,
            min_star_fraction: 0.1,
            confirmation_tolerance: 0.25,
            max_attempts: 2,
            direction: Direction::Ascending,
            min_position: None,
            max_position: None,
        }
    }

    fn point(position: i32, hfr: Option<f64>, star_count: u32) -> CurvePoint {
        CurvePoint {
            position,
            hfr,
            star_count,
            document_id: format!("doc-{position}"),
            rejected: None,
        }
    }

    /// A rig that answers from a scripted V, and records every move.
    struct ScriptedRig {
        /// `hfr(position)`, or `None` for a starless frame.
        curve: Box<dyn Fn(i32) -> Option<f64> + Send + Sync>,
        star_count: u32,
        moves: Mutex<Vec<i32>>,
        cancel_after: Mutex<Option<usize>>,
    }

    impl ScriptedRig {
        fn parabola(vertex: i32) -> Self {
            Self {
                curve: Box::new(move |position| {
                    let dx = f64::from(position - vertex);
                    Some(1.0 + dx * dx / 400.0)
                }),
                star_count: 100,
                moves: Mutex::new(Vec::new()),
                cancel_after: Mutex::new(None),
            }
        }

        fn starless() -> Self {
            Self {
                curve: Box::new(|_| None),
                star_count: 0,
                moves: Mutex::new(Vec::new()),
                cancel_after: Mutex::new(None),
            }
        }

        fn moves(&self) -> Vec<i32> {
            self.moves.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SweepOps for ScriptedRig {
        async fn move_focuser(&self, position: i32) -> Result<i32, FocusModelError> {
            self.moves.lock().unwrap().push(position);
            Ok(position)
        }

        async fn measure(&self) -> Result<Measurement, FocusModelError> {
            let position = *self.moves.lock().unwrap().last().unwrap();
            let hfr = (self.curve)(position);
            Ok(Measurement {
                hfr,
                star_count: if hfr.is_some() { self.star_count } else { 0 },
                document_id: format!("doc-{position}"),
            })
        }

        fn check_cancelled(&self) -> Result<(), FocusModelError> {
            let mut budget = self.cancel_after.lock().unwrap();
            match budget.as_mut() {
                Some(0) => Err(FocusModelError::Cancelled(
                    "the caller went away".to_owned(),
                )),
                Some(remaining) => {
                    *remaining = remaining.saturating_sub(1);
                    Ok(())
                }
                None => Ok(()),
            }
        }

        async fn tick(&self, _position: i32, _measurement: &Measurement) {}
    }

    // --- pure helpers ---

    #[test]
    fn the_grid_spans_the_half_width_in_steps() {
        assert_eq!(
            build_grid(100, 10, 20, (None, None)),
            [80, 90, 100, 110, 120]
        );
    }

    #[test]
    fn the_grid_drops_positions_outside_the_bounds_instead_of_coercing_them() {
        assert_eq!(
            build_grid(100, 10, 20, (Some(95), Some(115))),
            [100, 110],
            "a coerced point would duplicate a sample at the bound"
        );
    }

    #[test]
    fn a_descending_walk_reverses_the_grid() {
        let descending = SweepParams {
            direction: Direction::Descending,
            ..params()
        };
        let grid = sweep_grid(100, descending).unwrap();
        assert_eq!(grid.first(), Some(&140));
        assert_eq!(grid.last(), Some(&60));
    }

    #[test]
    fn a_grid_below_min_fit_points_is_refused_before_any_motion() {
        let narrow = SweepParams {
            min_position: Some(95),
            max_position: Some(115),
            ..params()
        };
        let err = sweep_grid(100, narrow).unwrap_err();
        assert!(
            matches!(&err, SweepFailure::Grid(message) if message.contains("min_fit_points is 5")),
            "{err:?}"
        );
    }

    #[test]
    fn the_sparse_gate_rejects_the_thin_frames_and_leaves_the_starless_ones_alone() {
        let mut points = vec![
            point(0, Some(4.0), 5),
            point(10, Some(2.0), 100),
            point(20, None, 0),
            point(30, Some(4.2), 90),
        ];
        let threshold = apply_sparse_gate(&mut points, 0.1);
        assert_eq!(threshold, 10.0);
        assert_eq!(points[0].rejected, Some(Rejection::Sparse));
        assert_eq!(points[1].rejected, None);
        assert_eq!(points[2].rejected, None, "a starless frame is not gated");
        assert_eq!(points[3].rejected, None);
    }

    #[test]
    fn a_field_that_is_sparse_everywhere_is_never_gated() {
        let mut points = vec![point(0, Some(4.0), 3), point(10, Some(2.0), 3)];
        apply_sparse_gate(&mut points, 0.5);
        assert!(points.iter().all(|p| p.rejected.is_none()));
    }

    #[test]
    fn the_fit_recovers_a_vertex_at_real_focuser_scales() {
        let samples: Vec<(i32, f64, u32)> = (0..9)
            .map(|i| {
                let position = 29_600 + i * 25;
                let dx = f64::from(position - 29_766);
                (position, 1.0 + dx * dx / 5_000.0, 100)
            })
            .collect();
        let fit = fit_parabola(&samples).unwrap();
        assert_eq!(fit.vertex_position(), 29_766);
        assert!((fit.vertex_value() - 1.0).abs() < 1e-6, "{fit:?}");
        assert!(fit.r_squared > 0.999, "{fit:?}");
    }

    #[test]
    fn a_flat_curve_has_no_vertex() {
        let samples: Vec<(i32, f64, u32)> = (0..6).map(|i| (29_600 + i * 25, 3.0, 100)).collect();
        let err = fit_parabola(&samples).unwrap_err();
        assert!(
            matches!(&err, FitError::MonotonicCurve(_)),
            "a curve with no spread has no vertex: {err:?}"
        );
    }

    #[test]
    fn a_concave_down_curve_has_no_minimum() {
        let samples: Vec<(i32, f64, u32)> = (0..6)
            .map(|i| {
                let position = 100 + i * 10;
                let dx = f64::from(position - 125);
                (position, 5.0 - dx * dx / 100.0, 50)
            })
            .collect();
        let err = fit_parabola(&samples).unwrap_err();
        assert!(
            matches!(&err, FitError::MonotonicCurve(message) if message.contains("leading coefficient")),
            "{err:?}"
        );
    }

    #[test]
    fn fewer_than_three_weighted_samples_cannot_be_fitted() {
        let err = fit_parabola(&[(0, 1.0, 10), (10, 2.0, 0), (20, 3.0, 0)]).unwrap_err();
        assert!(
            matches!(err, FitError::NotEnoughStars { got: 1, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn the_wing_slope_is_the_steeper_wing_in_pixels_per_hundred_steps() {
        let points = vec![
            point(0, Some(3.0), 100),
            point(100, Some(2.0), 100),
            point(200, Some(1.0), 100),
            point(300, Some(3.0), 100),
            point(400, Some(5.0), 100),
        ];
        // Left wing 1 px/100 steps, right wing 2 px/100 steps.
        assert_eq!(wing_slope(&points), Some(2.0));
    }

    #[test]
    fn a_wing_with_one_sample_does_not_count() {
        let points = vec![
            point(0, Some(3.0), 100),
            point(100, Some(1.0), 100),
            point(200, Some(3.0), 100),
        ];
        assert_eq!(wing_slope(&points), None);
    }

    #[test]
    fn the_retry_shifts_toward_the_lowest_sample_only_after_a_monotonic_curve() {
        let points = vec![point(80, Some(1.0), 100), point(120, Some(4.0), 100)];
        let monotonic = FitError::MonotonicCurve("no minimum".to_owned());
        assert_eq!(retry_centre(100, &monotonic, &points, 40, (None, None)), 60);
        let sparse = FitError::NotEnoughStars { got: 1, needed: 5 };
        assert_eq!(retry_centre(100, &sparse, &points, 40, (None, None)), 100);
    }

    #[test]
    fn a_shift_the_bounds_absorb_repeats_the_grid() {
        let points = vec![point(80, Some(1.0), 100), point(120, Some(4.0), 100)];
        let monotonic = FitError::MonotonicCurve("no minimum".to_owned());
        assert_eq!(
            retry_centre(100, &monotonic, &points, 40, (Some(100), None)),
            100
        );
    }

    #[test]
    fn the_confirmation_holds_the_frame_to_the_gate_and_the_tolerance() {
        assert!(confirmation_accepted(Some(1.1), 100, 10.0, 1.0, 0.25));
        assert!(
            !confirmation_accepted(Some(1.1), 5, 10.0, 1.0, 0.25),
            "a frame below the gate is not a confirmation"
        );
        assert!(
            !confirmation_accepted(Some(1.5), 100, 10.0, 1.0, 0.25),
            "1.5 is more than 25 % worse than 1.0"
        );
        assert!(!confirmation_accepted(None, 100, 10.0, 1.0, 0.25));
    }

    #[test]
    fn a_non_finite_reading_is_no_reading() {
        assert_eq!(finite_hfr(Some(f64::NAN)), None);
        assert_eq!(finite_hfr(Some(f64::INFINITY)), None);
        assert_eq!(finite_hfr(Some(1.5)), Some(1.5));
    }

    // --- the driver ---

    #[tokio::test]
    async fn a_clean_v_confirms_the_vertex_on_the_first_attempt() {
        let rig = ScriptedRig::parabola(100);
        let outcome = run_sweep(&rig, 100, params()).await.unwrap();
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.best_position, 100);
        assert!(outcome.confirmed);
        assert_eq!(outcome.position, 100);
        assert_eq!(outcome.samples_used, 9);
        assert_eq!(outcome.curve_points.len(), 9);
        assert!(outcome.wing_slope.is_some());
        // Nine grid moves, then the move to the vertex.
        assert_eq!(rig.moves().len(), 10);
    }

    #[tokio::test]
    async fn a_rejected_confirmation_settles_on_the_lowest_measured_sample() {
        // A V whose vertex sits between samples and whose confirmation
        // frame measures far worse than the sweep's best.
        let rig = ScriptedRig {
            curve: Box::new(|position| {
                if position == 100 {
                    Some(9.0) // the vertex, measured badly on the retake
                } else {
                    let dx = f64::from(position - 100);
                    Some(1.0 + dx.abs() / 100.0)
                }
            }),
            star_count: 100,
            moves: Mutex::new(Vec::new()),
            cancel_after: Mutex::new(None),
        };
        let outcome = run_sweep(&rig, 100, params()).await.unwrap();
        assert!(!outcome.confirmed);
        assert_eq!(outcome.position, 90, "the lowest accepted sample");
        assert_eq!(outcome.hfr, 1.1);
        assert_eq!(rig.moves().last(), Some(&90));
    }

    #[tokio::test]
    async fn a_starless_sweep_is_repeated_and_carries_every_attempt() {
        let rig = ScriptedRig::starless();
        let failure = run_sweep(&rig, 100, params()).await.unwrap_err();
        let SweepFailure::Fit {
            error,
            attempts,
            curve_points,
        } = failure
        else {
            panic!("expected a fit failure, got {failure:?}");
        };
        assert_eq!(attempts, 2, "max_attempts is 2");
        assert_eq!(error.outcome(), "not_enough_stars");
        assert_eq!(curve_points.len(), 18, "both attempts, nine points each");
        assert!(curve_points.iter().all(|p| p.hfr.is_none()));
        // Two full walks, and no move to a vertex that never fitted.
        assert_eq!(rig.moves().len(), 18);
    }

    #[tokio::test]
    async fn a_single_attempt_run_does_not_repeat() {
        let rig = ScriptedRig::starless();
        let single = SweepParams {
            max_attempts: 1,
            ..params()
        };
        let failure = run_sweep(&rig, 100, single).await.unwrap_err();
        assert!(
            matches!(failure, SweepFailure::Fit { attempts: 1, .. }),
            "{failure:?}"
        );
        assert_eq!(rig.moves().len(), 9);
    }

    #[tokio::test]
    async fn a_cancellation_stops_the_walk_where_it_is() {
        let rig = ScriptedRig::parabola(100);
        *rig.cancel_after.lock().unwrap() = Some(3);
        let failure = run_sweep(&rig, 100, params()).await.unwrap_err();
        let SweepFailure::Rig {
            error,
            curve_points,
        } = &failure
        else {
            panic!("expected a rig failure, got {failure:?}");
        };
        assert!(error.is_cancelled(), "{error}");
        assert!(rig.moves().len() < 9, "{:?}", rig.moves());
        assert_eq!(
            curve_points.len(),
            1,
            "the point measured before the cancellation"
        );
    }

    #[test]
    fn an_oversized_sweep_is_refused_before_the_grid_is_built() {
        let huge = SweepParams {
            step_size: 1,
            half_width: i32::MAX,
            ..params()
        };
        let failure = sweep_grid(0, huge).unwrap_err();
        let SweepFailure::Grid(message) = &failure else {
            panic!("expected a grid failure, got {failure:?}");
        };
        assert!(message.contains("more than the cap of 1000"), "{message}");
    }

    /// The cap is on positions walked, not on positions kept: a grid
    /// whose bounds discard every point still stops at the cap.
    #[test]
    fn a_clamped_grid_stops_at_the_cap() {
        let grid = build_grid(0, 1, i32::MAX, (Some(i32::MAX - 1), None));
        assert!(grid.is_empty(), "every point is below the minimum");
        assert!(planned_points(i32::MAX, 1) > MAX_GRID_POINTS);
    }
}
