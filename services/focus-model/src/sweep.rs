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
    pub const fn outcome(&self) -> FitOutcome {
        match self {
            Self::NotEnoughStars { .. } => FitOutcome::NotEnoughStars,
            Self::MonotonicCurve(_) => FitOutcome::MonotonicCurve,
        }
    }
}

/// The name a failed fit is recorded under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitOutcome {
    /// Too few samples survived the sparse gate.
    NotEnoughStars,
    /// No minimum inside the sampled range.
    MonotonicCurve,
}

impl FitOutcome {
    /// The name the record and the log carry.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotEnoughStars => "not_enough_stars",
            Self::MonotonicCurve => "monotonic_curve",
        }
    }
}

/// What a run did after an attempt that failed to fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retry {
    /// The next attempt walked the same grid.
    SameGrid,
    /// The next attempt's grid was centred `half_width` (or as far as
    /// the bounds allowed) toward the end of the accepted samples their
    /// lowest one sits nearer.
    Shift,
    /// The attempt called for a shift and the focuser's bounds took it
    /// back whole, so the next attempt walked the same grid.
    ShiftAbsorbed,
    /// The shifted grid would have held too few positions to fit, so
    /// no retry was made and the run ended.
    GridTooSmall,
    /// The attempt was the last the run was allowed.
    NoAttemptsLeft,
}

impl Retry {
    /// The name the record and the log carry.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SameGrid => "same_grid",
            Self::Shift => "shift",
            Self::ShiftAbsorbed => "shift_absorbed",
            Self::GridTooSmall => "grid_too_small",
            Self::NoAttemptsLeft => "no_attempts_left",
        }
    }
}

/// One attempt that failed to fit, as the run records it: why, where
/// its grid was centred, what the gate left of it, and what the run did
/// next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedAttempt {
    pub attempt: u32,
    pub outcome: FitOutcome,
    /// The failure in words.
    pub error: String,
    pub centre: i32,
    /// How many of the run's curve points this attempt measured: the
    /// run keeps every attempt's, in order, so the counts split them.
    pub points: usize,
    pub accepted: usize,
    pub sparse: usize,
    pub starless: usize,
    pub retry: Retry,
    /// Where the next grid was centred — or, after
    /// [`Retry::GridTooSmall`], would have been; `None` when no
    /// attempts were left.
    pub next_centre: Option<i32>,
}

/// How a sweep ended when it did not end in a curve.
#[derive(Debug, thiserror::Error)]
pub enum SweepFailure {
    /// Every permitted attempt failed to fit; the run's curve and the
    /// account of each attempt ride along so it is diagnosable without
    /// re-measuring.
    #[error("{error}")]
    Fit {
        error: FitError,
        attempts: u32,
        attempts_log: Vec<FailedAttempt>,
        curve_points: Vec<CurvePoint>,
    },
    /// A grid that cannot be walked, before any motion.
    #[error("{0}")]
    Grid(String),
    /// A primitive call failed or the caller cancelled, with whatever
    /// the run had measured by then and the attempts that had already
    /// failed to fit. The error is boxed to keep the failure small
    /// (`clippy::result_large_err`).
    #[error("{error}")]
    Rig {
        error: Box<FocusModelError>,
        attempts_log: Vec<FailedAttempt>,
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
    /// The attempts that failed to fit before the one that produced
    /// this outcome.
    pub attempts_log: Vec<FailedAttempt>,
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

/// Which way focus lies from a sweep whose stars ran out on one side
/// only: [`Ordering::Less`] below the accepted samples, [`Ordering::Greater`]
/// above them, `None` when the samples do not agree on a side.
///
/// Ordered by position, with sparse and starless points alike counted
/// as not accepted, they agree when at least two points are accepted and
/// at least one is not, every point that is not accepted lies beyond all
/// the accepted ones on one side, and the accepted HFRs rise strictly
/// toward that side, each above the one before it. Focus then lies the
/// other way, past the lowest accepted sample. A tie, a dip, a point
/// that is not accepted on the near side or among the accepted ones,
/// and two accepted points at one position all leave the side
/// undecided: a shift costs a whole grid, so the test is strict.
#[must_use]
pub fn one_sided_starvation(curve_points: &[CurvePoint]) -> Option<Ordering> {
    let mut ordered: Vec<&CurvePoint> = curve_points.iter().collect();
    ordered.sort_by_key(|point| point.position);
    let accepted: Vec<(i32, f64)> = ordered
        .iter()
        .filter_map(|point| point.accepted_sample())
        .map(|(position, hfr, _)| (position, hfr))
        .collect();
    // Two accepted points at least, and at least one that is not.
    let [(lowest_accepted, _), .., (highest_accepted, _)] = accepted.as_slice() else {
        return None;
    };
    let (lowest_accepted, highest_accepted) = (*lowest_accepted, *highest_accepted);
    if accepted.len() == ordered.len() {
        return None;
    }
    let mut not_accepted = ordered
        .iter()
        .filter(|point| point.accepted_sample().is_none())
        .map(|point| point.position);
    let rises_with =
        |pair: &[(i32, f64)]| matches!(pair, [(x0, hfr0), (x1, hfr1)] if x0 < x1 && hfr0 < hfr1);
    let falls_with =
        |pair: &[(i32, f64)]| matches!(pair, [(x0, hfr0), (x1, hfr1)] if x0 < x1 && hfr0 > hfr1);
    if not_accepted
        .clone()
        .all(|position| position > highest_accepted)
    {
        // Starved above: focus lies below when HFR rises toward the
        // starved end.
        accepted
            .windows(2)
            .all(rises_with)
            .then_some(Ordering::Less)
    } else if not_accepted.all(|position| position < lowest_accepted) {
        accepted
            .windows(2)
            .all(falls_with)
            .then_some(Ordering::Greater)
    } else {
        None
    }
}

/// Where a failed attempt sends the next grid, before the run checks
/// that the grid can be walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPlan {
    /// [`Retry::SameGrid`], [`Retry::Shift`] or [`Retry::ShiftAbsorbed`].
    pub retry: Retry,
    pub centre: i32,
}

/// Where the next attempt's grid is centred after a failed fit.
///
/// Moved by `half_width` when the attempt says focus lies past one end
/// of its accepted samples — after `monotonic_curve` the end
/// [`monotonic_side`] finds, after `not_enough_stars` the side
/// [`one_sided_starvation`] finds — clamped to the focuser's bounds;
/// the same place otherwise.
///
/// Either side is read from the samples alone, never against the
/// centre: a grid a bound clipped, or one whose frames on one side of
/// the centre all lost their stars, can hold every accepted sample on
/// one side of a centre they were never measured around, and a
/// comparison with that centre names the wrong side.
#[must_use]
pub fn retry_centre(
    centre: i32,
    error: &FitError,
    curve_points: &[CurvePoint],
    half_width: i32,
    bounds: (Option<i32>, Option<i32>),
) -> RetryPlan {
    let same_grid = RetryPlan {
        retry: Retry::SameGrid,
        centre,
    };
    let accepted: Vec<(i32, f64, u32)> = curve_points
        .iter()
        .filter_map(CurvePoint::accepted_sample)
        .collect();
    let Some((lowest_position, _, _)) = lowest_sample(&accepted) else {
        return same_grid;
    };
    let direction = match error {
        FitError::MonotonicCurve(_) => monotonic_side(lowest_position, &accepted, curve_points),
        FitError::NotEnoughStars { .. } => {
            one_sided_starvation(curve_points).unwrap_or(Ordering::Equal)
        }
    };
    let shifted = match direction {
        Ordering::Less => centre.saturating_sub(half_width),
        Ordering::Greater => centre.saturating_add(half_width),
        Ordering::Equal => return same_grid,
    };
    let shifted = bounds.0.map_or(shifted, |min| shifted.max(min));
    let shifted = bounds.1.map_or(shifted, |max| shifted.min(max));
    if shifted == centre {
        RetryPlan {
            retry: Retry::ShiftAbsorbed,
            centre,
        }
    } else {
        RetryPlan {
            retry: Retry::Shift,
            centre: shifted,
        }
    }
}

/// Which way focus lies from a sweep whose curve fell toward one end:
/// toward the end of the accepted samples the lowest one sits nearer —
/// [`Ordering::Less`] the low end, [`Ordering::Greater`] the high end —
/// or [`Ordering::Equal`], the same grid, when it sits midway between
/// them or a point that is not accepted lies past that end.
///
/// HFR falling toward such a point puts it nearer focus than any
/// accepted sample, so the sky took its stars, not the far wing leaving
/// the detector's band: the frames that would place focus are ones the
/// grid already walks, and the same grid is the retry that recovers
/// them once the sky does.
fn monotonic_side(
    lowest: i32,
    accepted: &[(i32, f64, u32)],
    curve_points: &[CurvePoint],
) -> Ordering {
    let (low_end, high_end) = accepted
        .iter()
        .fold((lowest, lowest), |(low_end, high_end), (sample, _, _)| {
            (low_end.min(*sample), high_end.max(*sample))
        });
    let side = lowest.abs_diff(low_end).cmp(&high_end.abs_diff(lowest));
    let past_end = |position: i32| match side {
        Ordering::Less => position < low_end,
        Ordering::Greater => position > high_end,
        Ordering::Equal => false,
    };
    let lost_past_end = curve_points
        .iter()
        .any(|point| point.accepted_sample().is_none() && past_end(point.position));
    if lost_past_end {
        Ordering::Equal
    } else {
        side
    }
}

/// The accepted, sparse and starless points of one attempt, after the
/// gate has marked them.
fn gate_counts(curve_points: &[CurvePoint]) -> (usize, usize, usize) {
    let accepted = curve_points
        .iter()
        .filter(|point| point.accepted_sample().is_some())
        .count();
    let sparse = curve_points
        .iter()
        .filter(|point| point.hfr.is_some() && point.rejected.is_some())
        .count();
    let starless = curve_points
        .iter()
        .filter(|point| point.hfr.is_none())
        .count();
    (accepted, sparse, starless)
}

/// The lowest and highest positions of a grid, whatever order it is
/// walked in.
fn grid_span(grid: &[i32]) -> (Option<i32>, Option<i32>) {
    (grid.iter().min().copied(), grid.iter().max().copied())
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

/// Whether a sweep this wide at this step can be walked at all,
/// before any centre is known: the cap [`sweep_grid`] refuses on, for
/// a caller that sizes several sweeps before it moves anything.
///
/// The other refusal — too few positions left after the focuser's
/// bounds have clamped the grid — depends on where the sweep is
/// centred, so it belongs to the run and not to the configuration.
///
/// # Errors
///
/// [`SweepFailure::Grid`] when the walk would hold more than
/// [`MAX_GRID_POINTS`] positions.
pub fn check_span(half_width: i32, step_size: i32) -> Result<(), SweepFailure> {
    let planned = planned_points(half_width, step_size);
    if planned > MAX_GRID_POINTS {
        return Err(SweepFailure::Grid(format!(
            "the sweep grid would hold {planned} positions, more than the cap of \
             {MAX_GRID_POINTS} (raise step_size or lower half_width)"
        )));
    }
    Ok(())
}

/// Whether the sweep around `centre` can be walked at all: the same
/// refusals [`run_sweep`] makes before its first move, available to a
/// caller that has its own moves to make first.
///
/// # Errors
///
/// [`SweepFailure::Grid`] when the grid would hold more than
/// [`MAX_GRID_POINTS`] positions, or fewer than `min_fit_points` after
/// the focuser's bounds have clamped it.
pub fn check_grid(centre: i32, params: SweepParams) -> Result<(), SweepFailure> {
    sweep_grid(centre, params).map(|_| ())
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
    check_span(params.half_width, params.step_size)?;
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

/// Determinant of a 3×3 matrix given by rows: the first-row cofactor
/// expansion with every multiply-add fused. Fusing leaves a rank-deficient
/// matrix a rounding-level residual rather than an exact zero, which is why
/// `fit_parabola` tests singularity against a relative scale, never `== 0`.
const fn det3(r0: [f64; 3], r1: [f64; 3], r2: [f64; 3]) -> f64 {
    let minor0 = r1[2].mul_add(-r2[1], r1[1] * r2[2]);
    let minor1 = r1[2].mul_add(-r2[0], r1[0] * r2[2]);
    let minor2 = r1[1].mul_add(-r2[0], r1[0] * r2[1]);
    r0[2].mul_add(minor2, r0[1].mul_add(-minor1, r0[0] * minor0))
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
/// against the positions those samples were measured at — not the
/// grid that was asked for, which a focuser settling short of its
/// targets does not describe. Pure: the caller decides what to do on
/// failure.
fn gate_and_fit(
    curve_points: &mut [CurvePoint],
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
    let (Some(sampled_min), Some(sampled_max)) = (
        accepted.iter().map(|(position, _, _)| *position).min(),
        accepted.iter().map(|(position, _, _)| *position).max(),
    ) else {
        return Err(FitError::MonotonicCurve(
            "no accepted sample to bound the vertex with".to_owned(),
        ));
    };
    if best_position < sampled_min || best_position > sampled_max {
        return Err(FitError::MonotonicCurve(format!(
            "fitted vertex {best_position} is outside sampled grid \
             [{sampled_min}, {sampled_max}]"
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
        // Where the focuser reports it is, not where it was sent: rp
        // answers a move that settled idle short of its target with
        // the read-back, and a sample belongs at the position it was
        // taken at.
        let reached = ops.move_focuser(*position).await?;
        ops.check_cancelled()?;
        let measurement = ops.measure().await?;
        ops.tick(reached, &measurement).await;
        curve_points.push(CurvePoint {
            position: reached,
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
        if position == stage.lowest_position {
            (position, stage.lowest_hfr)
        } else {
            // The focuser settled short of the sample this fell back
            // to, so the HFR measured there is not this position's.
            // One frame says what is true here rather than reporting
            // a pair that was never measured together — and a frame
            // with no stars leaves the run with no trustworthy
            // position at all, which is a failed sweep, not a
            // fallback wearing another position's focus quality.
            ops.check_cancelled()?;
            let measurement = ops.measure().await?;
            ops.tick(position, &measurement).await;
            let measured = finite_hfr(measurement.hfr).ok_or_else(|| {
                FocusModelError::Workflow(format!(
                    "the focuser settled at {position} instead of {}, and that position \
                     measured no stars",
                    stage.lowest_position
                ))
            })?;
            (position, measured)
        }
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
    let mut attempts_log: Vec<FailedAttempt> = Vec::new();
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
                error: Box::new(error),
                attempts_log,
                curve_points: measured,
            });
        }

        let error = match gate_and_fit(&mut curve_points, params) {
            Ok(stage) => {
                let wing_slope = wing_slope(&curve_points);
                let confirmed = confirm(ops, params, stage).await;
                measured.append(&mut curve_points);
                let (confirmation, position, hfr) = match confirmed {
                    Ok(confirmed) => confirmed,
                    Err(error) => {
                        return Err(SweepFailure::Rig {
                            error: Box::new(error),
                            attempts_log,
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
                    attempts_log,
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
        let plan = (attempts < params.max_attempts).then(|| {
            retry_centre(
                centre,
                &error,
                &curve_points,
                params.half_width,
                params.bounds(),
            )
        });
        let (retry, next_centre, next_grid) =
            match plan.map(|plan| (plan, sweep_grid(plan.centre, params))) {
                Some((plan, Ok(next_grid))) => (plan.retry, Some(plan.centre), Some(next_grid)),
                Some((plan, Err(_))) => (Retry::GridTooSmall, Some(plan.centre), None),
                None => (Retry::NoAttemptsLeft, None, None),
            };
        let (accepted, sparse, starless) = gate_counts(&curve_points);
        let failed = FailedAttempt {
            attempt: attempts,
            outcome: error.outcome(),
            error: error.to_string(),
            centre,
            points: curve_points.len(),
            accepted,
            sparse,
            starless,
            retry,
            next_centre,
        };
        log_failed_attempt(&failed, params.max_attempts, &grid, next_grid.as_deref());
        attempts_log.push(failed);
        measured.append(&mut curve_points);
        if let (Some(next_centre), Some(next_grid)) = (next_centre, next_grid) {
            centre = next_centre;
            grid = next_grid;
            continue;
        }
        return Err(SweepFailure::Fit {
            error,
            attempts,
            attempts_log,
            curve_points: measured,
        });
    }
}

/// The `warn` line an attempt that failed to fit leaves: why, the grid
/// it walked, what the gate left of it, and the grid that comes next or
/// why none does.
fn log_failed_attempt(
    failed: &FailedAttempt,
    max_attempts: u32,
    grid: &[i32],
    next_grid: Option<&[i32]>,
) {
    let (grid_min, grid_max) = grid_span(grid);
    let (next_grid_min, next_grid_max) = next_grid.map_or((None, None), grid_span);
    let outcome = failed.outcome.as_str();
    let retry = failed.retry.as_str();
    warn!(
        attempt = failed.attempt,
        max_attempts,
        outcome,
        error = %failed.error,
        centre = failed.centre,
        grid_min = ?grid_min,
        grid_max = ?grid_max,
        accepted = failed.accepted,
        sparse = failed.sparse,
        starless = failed.starless,
        retry,
        next_centre = ?failed.next_centre,
        next_grid_min = ?next_grid_min,
        next_grid_max = ?next_grid_max,
        "sweep attempt failed to fit"
    );
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
        /// Fail the move after this many have been made.
        fail_move_after: Mutex<Option<usize>>,
        /// Land this many steps short of every target, as a focuser
        /// that settles idle before it arrives does.
        short_by: Mutex<i32>,
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
                fail_move_after: Mutex::new(None),
                short_by: Mutex::new(0),
            }
        }

        fn starless() -> Self {
            Self {
                curve: Box::new(|_| None),
                star_count: 0,
                moves: Mutex::new(Vec::new()),
                cancel_after: Mutex::new(None),
                fail_move_after: Mutex::new(None),
                short_by: Mutex::new(0),
            }
        }

        fn moves(&self) -> Vec<i32> {
            self.moves.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SweepOps for ScriptedRig {
        async fn move_focuser(&self, position: i32) -> Result<i32, FocusModelError> {
            let mut budget = self.fail_move_after.lock().unwrap();
            match budget.as_mut() {
                Some(0) => {
                    return Err(FocusModelError::ToolCall("the focuser jammed".to_owned()));
                }
                Some(remaining) => *remaining = remaining.saturating_sub(1),
                None => {}
            }
            drop(budget);
            let reached = position - *self.short_by.lock().unwrap();
            self.moves.lock().unwrap().push(reached);
            Ok(reached)
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
    fn a_monotonic_curve_shifts_toward_the_lowest_sample() {
        let points = vec![point(80, Some(1.0), 100), point(120, Some(4.0), 100)];
        let monotonic = FitError::MonotonicCurve("no minimum".to_owned());
        assert_eq!(
            retry_centre(100, &monotonic, &points, 40, (None, None)),
            RetryPlan {
                retry: Retry::Shift,
                centre: 60
            }
        );
    }

    /// Frames past the lowest sample that lost their stars sit nearer
    /// focus than any accepted one: the sky took them, and the same grid
    /// recovers them. The centre, above every accepted sample, would
    /// have sent the shift down, away from focus.
    #[test]
    fn a_monotonic_curve_that_lost_the_frames_past_its_lowest_sample_repeats_the_grid() {
        let points = vec![
            point(60, Some(5.0), 100),
            point(70, Some(4.0), 100),
            point(80, Some(3.0), 100),
            point(90, Some(2.0), 100),
            point(100, None, 0),
            point(110, None, 0),
            point(120, None, 0),
        ];
        let monotonic = FitError::MonotonicCurve("no minimum".to_owned());
        assert_eq!(
            retry_centre(100, &monotonic, &points, 40, (None, None)),
            RetryPlan {
                retry: Retry::SameGrid,
                centre: 100
            }
        );
    }

    /// A thinner cloud: frames the gate rejected past a lowest sample on
    /// the centre are lost frames too, and they stay out of the span the
    /// end is measured on.
    #[test]
    fn a_monotonic_curve_that_lost_the_frames_past_its_lowest_sample_to_the_gate_repeats_the_grid()
    {
        let sparse = |position, hfr| CurvePoint {
            rejected: Some(Rejection::Sparse),
            ..point(position, Some(hfr), 5)
        };
        let points = vec![
            point(50, Some(6.0), 100),
            point(60, Some(5.0), 100),
            point(70, Some(4.0), 100),
            point(80, Some(3.0), 100),
            point(90, Some(2.0), 100),
            point(100, Some(1.5), 100),
            sparse(110, 1.2),
            sparse(120, 1.4),
        ];
        let monotonic = FitError::MonotonicCurve("no minimum".to_owned());
        assert_eq!(
            retry_centre(100, &monotonic, &points, 40, (None, None)),
            RetryPlan {
                retry: Retry::SameGrid,
                centre: 100
            }
        );
    }

    /// Frames lost past the far end are the far wing leaving the
    /// detector's band, not the sky; they leave the shift alone.
    #[test]
    fn a_monotonic_curve_that_lost_its_far_wing_still_shifts() {
        let points = vec![
            point(60, Some(2.0), 100),
            point(70, Some(3.0), 100),
            point(80, Some(4.0), 100),
            point(90, Some(5.0), 100),
            point(100, None, 0),
            point(110, None, 0),
        ];
        let monotonic = FitError::MonotonicCurve("no minimum".to_owned());
        assert_eq!(
            retry_centre(100, &monotonic, &points, 40, (None, None)),
            RetryPlan {
                retry: Retry::Shift,
                centre: 60
            }
        );
    }

    /// The mirror: frames lost below a lowest sample at the bottom of the
    /// accepted ones keep the grid, though the centre sits above it.
    #[test]
    fn a_monotonic_curve_that_lost_the_frames_below_its_lowest_sample_repeats_the_grid() {
        let points = vec![
            point(80, None, 0),
            point(90, None, 0),
            point(100, Some(2.0), 100),
            point(110, Some(3.0), 100),
            point(120, Some(4.0), 100),
            point(130, Some(5.0), 100),
            point(140, Some(6.0), 100),
        ];
        let monotonic = FitError::MonotonicCurve("no minimum".to_owned());
        assert_eq!(
            retry_centre(110, &monotonic, &points, 40, (None, None)),
            RetryPlan {
                retry: Retry::SameGrid,
                centre: 110
            }
        );
    }

    /// The mirror of the far wing: frames lost below a curve falling
    /// toward the top leave the shift up alone.
    #[test]
    fn a_monotonic_curve_that_lost_its_far_wing_below_still_shifts() {
        let points = vec![
            point(50, None, 0),
            point(60, None, 0),
            point(70, Some(6.0), 100),
            point(80, Some(5.0), 100),
            point(90, Some(4.0), 100),
            point(100, Some(3.0), 100),
            point(110, Some(2.0), 100),
        ];
        let monotonic = FitError::MonotonicCurve("no minimum".to_owned());
        assert_eq!(
            retry_centre(100, &monotonic, &points, 40, (None, None)),
            RetryPlan {
                retry: Retry::Shift,
                centre: 140
            }
        );
    }

    /// A frame lost between the lowest sample and the end it sits
    /// nearer is inside the samples, not past them: an accepted sample
    /// beyond it already says where the curve turns, and the shift
    /// stands.
    #[test]
    fn a_monotonic_curve_that_lost_a_frame_inside_its_samples_still_shifts() {
        let points = vec![
            point(60, Some(6.0), 100),
            point(70, Some(5.0), 100),
            point(80, Some(4.0), 100),
            point(90, Some(3.0), 100),
            point(100, Some(2.0), 100),
            point(110, None, 0),
            point(120, Some(2.05), 100),
        ];
        let monotonic = FitError::MonotonicCurve("no minimum".to_owned());
        assert_eq!(
            retry_centre(90, &monotonic, &points, 40, (None, None)),
            RetryPlan {
                retry: Retry::Shift,
                centre: 130
            }
        );
    }

    /// The mirror: a frame lost inside the samples below the lowest one.
    #[test]
    fn a_monotonic_curve_that_lost_a_frame_inside_its_samples_below_still_shifts() {
        let points = vec![
            point(80, Some(2.05), 100),
            point(90, None, 0),
            point(100, Some(2.0), 100),
            point(110, Some(3.0), 100),
            point(120, Some(4.0), 100),
            point(130, Some(5.0), 100),
            point(140, Some(6.0), 100),
        ];
        let monotonic = FitError::MonotonicCurve("no minimum".to_owned());
        assert_eq!(
            retry_centre(110, &monotonic, &points, 40, (None, None)),
            RetryPlan {
                retry: Retry::Shift,
                centre: 70
            }
        );
    }

    /// A bound that clipped every grid point below the centre leaves the
    /// lowest sample at the bottom of the walk, above the centre; focus
    /// lies below it, toward the bound.
    #[test]
    fn a_monotonic_curve_a_bound_clipped_above_its_centre_shifts_toward_the_bound() {
        let bounds = (Some(98), None);
        let grid = build_grid(100, 10, 45, bounds);
        assert_eq!(
            grid,
            [105, 115, 125, 135, 145],
            "nothing walked below the centre"
        );
        let points: Vec<CurvePoint> = grid
            .iter()
            .zip([2.0, 3.0, 4.0, 5.0, 6.0])
            .map(|(position, hfr)| point(*position, Some(hfr), 100))
            .collect();
        let monotonic = FitError::MonotonicCurve("no minimum".to_owned());
        assert_eq!(
            retry_centre(100, &monotonic, &points, 45, bounds),
            RetryPlan {
                retry: Retry::Shift,
                centre: 98
            }
        );
    }

    /// A lowest sample midway between the accepted samples' ends names
    /// neither of them, wherever the centre sits and whatever was lost
    /// past them.
    #[test]
    fn a_monotonic_curve_lowest_midway_between_its_ends_repeats_the_grid() {
        let points = vec![
            point(110, Some(3.0), 100),
            point(130, Some(1.0), 100),
            point(150, Some(3.5), 100),
            point(160, None, 0),
        ];
        let monotonic = FitError::MonotonicCurve("no minimum".to_owned());
        assert_eq!(
            retry_centre(100, &monotonic, &points, 40, (None, None)),
            RetryPlan {
                retry: Retry::SameGrid,
                centre: 100
            }
        );
    }

    /// Two accepted points with nothing rejected say nothing about
    /// where the stars ran out.
    #[test]
    fn a_short_sweep_with_nothing_rejected_repeats_the_grid() {
        let points = vec![point(80, Some(1.0), 100), point(120, Some(4.0), 100)];
        let sparse = FitError::NotEnoughStars { got: 2, needed: 5 };
        assert_eq!(
            retry_centre(100, &sparse, &points, 40, (None, None)),
            RetryPlan {
                retry: Retry::SameGrid,
                centre: 100
            }
        );
    }

    #[test]
    fn a_shift_the_bounds_absorb_repeats_the_grid() {
        let points = vec![point(80, Some(1.0), 100), point(120, Some(4.0), 100)];
        let monotonic = FitError::MonotonicCurve("no minimum".to_owned());
        assert_eq!(
            retry_centre(100, &monotonic, &points, 40, (Some(100), None)),
            RetryPlan {
                retry: Retry::ShiftAbsorbed,
                centre: 100
            }
        );
    }

    #[test]
    fn a_shift_the_bounds_absorb_in_part_still_moves() {
        let points = vec![point(80, Some(1.0), 100), point(120, Some(4.0), 100)];
        let monotonic = FitError::MonotonicCurve("no minimum".to_owned());
        assert_eq!(
            retry_centre(100, &monotonic, &points, 40, (Some(90), None)),
            RetryPlan {
                retry: Retry::Shift,
                centre: 90
            }
        );
    }

    // --- the retry after a sweep starved on one side ---

    /// The rig2 sweep of 2026-09-26, gated as the sweep gates it:
    /// three accepted samples rising toward four sparse ones and two
    /// starless ones, focus near 29750 below the grid.
    fn starved_above() -> Vec<CurvePoint> {
        let mut points = vec![
            point(29800, Some(2.17), 88),
            point(29900, Some(5.98), 46),
            point(30000, Some(6.97), 16),
            point(30100, Some(10.71), 6),
            point(30200, Some(20.91), 1),
            point(30300, Some(25.10), 1),
            point(30400, Some(29.30), 1),
            point(30500, None, 0),
            point(30600, None, 0),
        ];
        apply_sparse_gate(&mut points, 0.1);
        points
    }

    fn not_enough_stars() -> FitError {
        FitError::NotEnoughStars { got: 3, needed: 5 }
    }

    #[test]
    fn a_sweep_starved_above_points_below() {
        assert_eq!(one_sided_starvation(&starved_above()), Some(Ordering::Less));
    }

    #[test]
    fn a_sweep_starved_above_shifts_down_by_half_width() {
        assert_eq!(
            retry_centre(
                30200,
                &not_enough_stars(),
                &starved_above(),
                400,
                (None, None)
            ),
            RetryPlan {
                retry: Retry::Shift,
                centre: 29800
            }
        );
    }

    #[test]
    fn a_sweep_starved_below_points_above() {
        let points = vec![
            point(100, None, 0),
            point(110, Some(9.0), 5),
            point(120, Some(7.0), 100),
            point(130, Some(4.0), 100),
        ];
        assert_eq!(one_sided_starvation(&points), Some(Ordering::Greater));
        assert_eq!(
            retry_centre(120, &not_enough_stars(), &points, 40, (None, None)),
            RetryPlan {
                retry: Retry::Shift,
                centre: 160
            }
        );
    }

    /// The side is read from positions, so the walk order — ascending
    /// or descending with the backlash approach — does not change it.
    #[test]
    fn the_walk_order_does_not_change_the_side() {
        let mut points = starved_above();
        points.reverse();
        assert_eq!(one_sided_starvation(&points), Some(Ordering::Less));
    }

    /// A ±1200 sweep recorded on rig2: one accepted sample each side of
    /// focus and starless frames beyond both, so the samples name no
    /// side.
    #[test]
    fn a_sweep_starved_on_both_sides_repeats_the_grid() {
        let mut points = vec![
            point(28537, None, 0),
            point(28837, None, 0),
            point(29137, None, 0),
            point(29437, Some(14.4), 1),
            point(29737, Some(1.12), 8),
            point(30037, Some(14.0), 1),
            point(30337, None, 0),
            point(30637, None, 0),
            point(30937, None, 0),
        ];
        apply_sparse_gate(&mut points, 0.1);
        assert_eq!(one_sided_starvation(&points), None);
        assert_eq!(
            retry_centre(29737, &not_enough_stars(), &points, 1200, (None, None)),
            RetryPlan {
                retry: Retry::SameGrid,
                centre: 29737
            }
        );
    }

    #[test]
    fn a_dip_in_the_accepted_samples_names_no_side() {
        let points = vec![
            point(100, Some(2.0), 100),
            point(110, Some(5.0), 100),
            point(120, Some(4.0), 100),
            point(130, None, 0),
        ];
        assert_eq!(one_sided_starvation(&points), None);
    }

    #[test]
    fn a_tie_in_the_accepted_samples_names_no_side() {
        let points = vec![
            point(100, Some(2.0), 100),
            point(110, Some(5.0), 100),
            point(120, Some(5.0), 100),
            point(130, None, 0),
        ];
        assert_eq!(one_sided_starvation(&points), None);
    }

    /// HFR falling toward the side the stars left is not the far wing
    /// leaving the detector's band — a cloud, a donut the detector lost
    /// — and focus does not lie the other way.
    #[test]
    fn samples_falling_toward_the_starved_side_name_no_side() {
        let points = vec![
            point(100, Some(8.0), 100),
            point(110, Some(6.0), 100),
            point(120, Some(4.0), 100),
            point(130, None, 0),
        ];
        assert_eq!(one_sided_starvation(&points), None);
    }

    #[test]
    fn a_starless_point_among_the_accepted_ones_names_no_side() {
        let points = vec![
            point(100, Some(2.0), 100),
            point(110, None, 0),
            point(120, Some(5.0), 100),
            point(130, None, 0),
        ];
        assert_eq!(one_sided_starvation(&points), None);
    }

    /// The near-side point reads below every accepted one, so only its
    /// rejection — sparse counted as not accepted, like starless —
    /// keeps the samples from naming a side.
    #[test]
    fn a_rejection_on_the_near_side_names_no_side() {
        let mut points = vec![
            point(90, Some(1.5), 5),
            point(100, Some(2.0), 100),
            point(110, Some(5.0), 100),
            point(120, None, 0),
        ];
        apply_sparse_gate(&mut points, 0.1);
        assert_eq!(points.first().unwrap().rejected, Some(Rejection::Sparse));
        assert_eq!(one_sided_starvation(&points), None);
    }

    #[test]
    fn a_single_accepted_sample_names_no_side() {
        let points = vec![
            point(100, Some(2.0), 100),
            point(110, None, 0),
            point(120, None, 0),
        ];
        assert_eq!(one_sided_starvation(&points), None);
    }

    /// A focuser that settles short can report one position twice; two
    /// samples at one position have no order to agree on.
    #[test]
    fn two_samples_at_one_position_name_no_side() {
        let points = vec![
            point(100, Some(2.0), 100),
            point(100, Some(3.0), 100),
            point(110, Some(5.0), 100),
            point(120, None, 0),
        ];
        assert_eq!(one_sided_starvation(&points), None);
    }

    /// A bound that clipped every grid point below the centre leaves
    /// all the samples above it; the side they name is still the side
    /// focus lies on, and a bound sitting at the centre absorbs the
    /// shift rather than hiding that it was asked for.
    #[test]
    fn a_grid_clipped_at_its_centre_reports_the_shift_the_bound_absorbs() {
        let grid = build_grid(100, 10, 55, (Some(100), None));
        assert_eq!(grid.first(), Some(&105), "nothing walked below the centre");
        let points = vec![
            point(105, Some(2.0), 100),
            point(115, Some(3.0), 100),
            point(125, None, 0),
            point(135, None, 0),
        ];
        assert_eq!(
            retry_centre(100, &not_enough_stars(), &points, 55, (Some(100), None)),
            RetryPlan {
                retry: Retry::ShiftAbsorbed,
                centre: 100
            }
        );
    }

    #[test]
    fn the_gate_counts_split_accepted_sparse_and_starless() {
        assert_eq!(gate_counts(&starved_above()), (3, 4, 2));
    }

    /// The log line and the record name an outcome and a retry the same
    /// way.
    #[test]
    fn the_logged_names_are_the_recorded_names() {
        for outcome in [FitOutcome::NotEnoughStars, FitOutcome::MonotonicCurve] {
            assert_eq!(
                serde_json::to_value(outcome).unwrap(),
                serde_json::json!(outcome.as_str())
            );
        }
        for retry in [
            Retry::SameGrid,
            Retry::Shift,
            Retry::ShiftAbsorbed,
            Retry::GridTooSmall,
            Retry::NoAttemptsLeft,
        ] {
            assert_eq!(
                serde_json::to_value(retry).unwrap(),
                serde_json::json!(retry.as_str())
            );
        }
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
            fail_move_after: Mutex::new(None),
            short_by: Mutex::new(0),
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
            attempts_log,
            curve_points,
        } = failure
        else {
            panic!("expected a fit failure, got {failure:?}");
        };
        assert_eq!(attempts, 2, "max_attempts is 2");
        assert_eq!(error.outcome(), FitOutcome::NotEnoughStars);
        assert_eq!(curve_points.len(), 18, "both attempts, nine points each");
        assert!(curve_points.iter().all(|p| p.hfr.is_none()));
        // Two full walks, and no move to a vertex that never fitted.
        assert_eq!(rig.moves().len(), 18);
        assert_eq!(
            attempts_log,
            [
                FailedAttempt {
                    attempt: 1,
                    outcome: FitOutcome::NotEnoughStars,
                    error: error.to_string(),
                    centre: 100,
                    points: 9,
                    accepted: 0,
                    sparse: 0,
                    starless: 9,
                    retry: Retry::SameGrid,
                    next_centre: Some(100),
                },
                FailedAttempt {
                    attempt: 2,
                    outcome: FitOutcome::NotEnoughStars,
                    error: error.to_string(),
                    centre: 100,
                    points: 9,
                    accepted: 0,
                    sparse: 0,
                    starless: 9,
                    retry: Retry::NoAttemptsLeft,
                    next_centre: None,
                },
            ],
            "a starless sweep names no side, so the retry walked the same grid"
        );
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
            ..
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

    /// A fallback that lands somewhere the sweep cannot measure leaves
    /// no trustworthy position: the run fails rather than reporting
    /// the lowest sample's focus quality at a position it never had.
    #[tokio::test]
    async fn a_fallback_that_lands_somewhere_starless_fails_the_sweep() {
        let rig = ScriptedRig::starless();
        *rig.short_by.lock().unwrap() = 2;
        let stage = FitStage {
            best_position: 100,
            best_hfr: 1.0,
            r_squared: 0.9,
            gate_threshold: 0.0,
            lowest_position: 90,
            lowest_hfr: 1.1,
            samples_used: 5,
        };
        let err = confirm(&rig, params(), stage).await.unwrap_err();
        assert!(err.tool_message().contains("measured no stars"), "{err}");
        assert!(
            err.tool_message().contains("settled at 88 instead of 90"),
            "{err}"
        );
    }

    /// A focuser that settles short samples a range the requested grid
    /// does not describe, so the vertex is judged against the samples:
    /// a fit inside what was actually measured is a fit.
    #[tokio::test]
    async fn the_vertex_is_bounded_by_the_samples_not_the_request() {
        let rig = ScriptedRig::parabola(58);
        // Every move lands two steps short, so the samples run
        // [58, 138] while the requested grid ran [60, 140].
        *rig.short_by.lock().unwrap() = 2;
        let outcome = run_sweep(&rig, 100, params()).await.unwrap();
        assert_eq!(
            outcome.best_position, 58,
            "the vertex sits below the requested grid's first point"
        );
    }

    /// `rp` answers a move that settled idle short of its target with
    /// the read-back, so the sample belongs at that position — a curve
    /// fitted against where the focuser was asked to be would be a
    /// curve of a sweep that never happened.
    #[tokio::test]
    async fn a_focuser_that_lands_short_is_recorded_where_it_landed() {
        let rig = ScriptedRig::starless();
        *rig.short_by.lock().unwrap() = 2;
        let single = SweepParams {
            max_attempts: 1,
            ..params()
        };
        let failure = run_sweep(&rig, 100, single).await.unwrap_err();
        let SweepFailure::Fit { curve_points, .. } = &failure else {
            panic!("expected a fit failure, got {failure:?}");
        };
        let positions: Vec<i32> = curve_points.iter().map(|point| point.position).collect();
        assert_eq!(positions.first(), Some(&58), "the grid starts at 60");
        assert_eq!(positions, rig.moves(), "every sample where it landed");
    }

    /// A confirmation that cannot be measured is still a run with a
    /// curve: the walk that fitted it has already been measured, and
    /// the failure carries it.
    #[tokio::test]
    async fn a_failed_confirmation_keeps_the_walk_it_fitted() {
        let rig = ScriptedRig::parabola(100);
        // Nine grid moves land; the move to the vertex does not.
        *rig.fail_move_after.lock().unwrap() = Some(9);
        let failure = run_sweep(&rig, 100, params()).await.unwrap_err();
        let SweepFailure::Rig {
            error,
            curve_points,
            ..
        } = &failure
        else {
            panic!("expected a rig failure, got {failure:?}");
        };
        assert!(error.tool_message().contains("jammed"), "{error}");
        assert_eq!(curve_points.len(), 9, "the whole walk is recorded");
    }

    /// A rig whose stars run out at and above `edge`: the far wing of a
    /// V centred on `vertex` leaving the detector's band.
    fn starless_from(vertex: i32, edge: i32) -> ScriptedRig {
        let rig = ScriptedRig::parabola(vertex);
        ScriptedRig {
            curve: Box::new(move |position| {
                let dx = f64::from(position - vertex);
                (position < edge).then_some(1.0 + dx * dx / 400.0)
            }),
            ..rig
        }
    }

    /// A sweep starved on one side, end to end: the first grid holds
    /// four accepted samples rising toward the starless ones above them,
    /// so the retry moves half a width down, brackets the vertex and
    /// fits.
    #[tokio::test]
    async fn a_sweep_starved_on_one_side_shifts_and_fits_on_the_retry() {
        let rig = starless_from(40, 100);
        let outcome = run_sweep(&rig, 100, params()).await.unwrap();
        assert_eq!(outcome.attempts, 2);
        assert_eq!(outcome.best_position, 40);
        assert!(outcome.confirmed);
        assert_eq!(
            outcome.attempts_log,
            [FailedAttempt {
                attempt: 1,
                outcome: FitOutcome::NotEnoughStars,
                error: FitError::NotEnoughStars { got: 4, needed: 5 }.to_string(),
                centre: 100,
                points: 9,
                accepted: 4,
                sparse: 0,
                starless: 5,
                retry: Retry::Shift,
                next_centre: Some(60),
            }]
        );
        assert_eq!(
            outcome.curve_points.len(),
            18,
            "both walks, split by the log's nine"
        );
        assert_eq!(
            rig.moves().get(9),
            Some(&20),
            "the second walk starts half a width lower"
        );
    }

    /// A rig under a cloud that blanks every frame at a position `under`
    /// covers, for the first `frames` frames, over the V `hfr` draws.
    fn clouded(
        hfr: impl Fn(i32) -> f64 + Send + Sync + 'static,
        under: impl Fn(i32) -> bool + Send + Sync + 'static,
        frames: usize,
    ) -> ScriptedRig {
        let taken = std::sync::atomic::AtomicUsize::new(0);
        ScriptedRig {
            curve: Box::new(move |position| {
                let frame = taken.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                (frame >= frames || !under(position)).then(|| hfr(position))
            }),
            ..ScriptedRig::parabola(0)
        }
    }

    /// A hyperbolic V focused at 30000: 2 px there, 6 px 400 steps out.
    fn roster_v(position: i32) -> f64 {
        let dx = f64::from(position - 30_000) / 400.0;
        32.0_f64.mul_add(dx * dx, 4.0).sqrt()
    }

    /// A cloud over the half of the first walk nearest focus, end to
    /// end: the accepted samples all sit below the centre, falling
    /// toward focus above them, so the retry walks the same grid, which
    /// the cleared sky lets bracket the vertex.
    #[tokio::test]
    async fn a_monotonic_curve_a_cloud_cut_short_walks_the_same_grid_and_fits() {
        let rig = clouded(
            |position| {
                let dx = f64::from(position - 115);
                1.0 + dx * dx / 400.0
            },
            |position| position >= 100,
            17,
        );
        let wide = SweepParams {
            half_width: 80,
            ..params()
        };
        let outcome = run_sweep(&rig, 100, wide).await.unwrap();
        assert_eq!(outcome.attempts, 2);
        assert_eq!(outcome.best_position, 115);
        assert!(outcome.confirmed);
        let first = outcome.attempts_log.first().unwrap();
        assert_eq!(
            (
                first.outcome,
                first.accepted,
                first.starless,
                first.retry,
                first.next_centre
            ),
            (FitOutcome::MonotonicCurve, 8, 9, Retry::SameGrid, Some(100))
        );
        assert_eq!(
            rig.moves().get(17),
            Some(&20),
            "the second walk is the first one again"
        );
    }

    /// Focus where the prediction put it, on the centre, and a cloud
    /// over the frames above it: a parabola through the one wing left
    /// puts its vertex past the samples, so the attempt fails as a
    /// monotonic curve. A shift would start the next grid at focus and
    /// fail the same way; the same grid, under a cleared sky, fits.
    #[tokio::test]
    async fn a_monotonic_curve_a_cloud_cut_short_at_focus_fits_on_the_same_grid() {
        let rig = clouded(roster_v, |position| position >= 30_100, 9);
        let roster = SweepParams {
            step_size: 100,
            half_width: 400,
            ..params()
        };
        let outcome = run_sweep(&rig, 30_000, roster).await.unwrap();
        assert_eq!(outcome.attempts, 2);
        assert_eq!(outcome.best_position, 30_000);
        assert!(outcome.confirmed);
        let first = outcome.attempts_log.first().unwrap();
        assert_eq!(
            (first.outcome, first.accepted, first.retry),
            (FitOutcome::MonotonicCurve, 5, Retry::SameGrid)
        );
    }

    /// The same sweep walked downward, as an inward backlash approach
    /// walks it: the cloud takes the frames below focus, the curve falls
    /// toward the bottom of what is left, and the same grid fits.
    #[tokio::test]
    async fn a_descending_monotonic_curve_a_cloud_cut_short_at_focus_fits_on_the_same_grid() {
        let rig = clouded(roster_v, |position| position <= 29_900, 9);
        let roster = SweepParams {
            step_size: 100,
            half_width: 400,
            direction: Direction::Descending,
            ..params()
        };
        let outcome = run_sweep(&rig, 30_000, roster).await.unwrap();
        assert_eq!(outcome.attempts, 2);
        assert_eq!(outcome.best_position, 30_000);
        assert!(outcome.confirmed);
        let first = outcome.attempts_log.first().unwrap();
        assert_eq!(
            (first.outcome, first.accepted, first.retry),
            (FitOutcome::MonotonicCurve, 5, Retry::SameGrid)
        );
    }

    /// A shift whose clamped grid holds fewer positions than a fit needs
    /// is not made: the run ends on the attempt that asked for it.
    #[tokio::test]
    async fn a_shift_into_a_grid_too_small_ends_the_run() {
        let rig = starless_from(40, 100);
        let tight = SweepParams {
            min_fit_points: 7,
            min_position: Some(70),
            ..params()
        };
        let failure = run_sweep(&rig, 100, tight).await.unwrap_err();
        let SweepFailure::Fit {
            attempts,
            attempts_log,
            ..
        } = &failure
        else {
            panic!("expected a fit failure, got {failure:?}");
        };
        assert_eq!(*attempts, 1, "no second walk was made");
        assert_eq!(attempts_log.len(), 1);
        let only = attempts_log.first().unwrap();
        assert_eq!(only.retry, Retry::GridTooSmall);
        assert_eq!(
            only.next_centre,
            Some(70),
            "where the shift would have gone"
        );
        assert_eq!(rig.moves().len(), 8, "one walk of 70..140");
    }

    #[tokio::test]
    async fn a_shift_the_bounds_absorb_walks_the_same_grid_again() {
        let rig = starless_from(40, 120);
        let pinned = SweepParams {
            min_position: Some(100),
            ..params()
        };
        let failure = run_sweep(&rig, 100, pinned).await.unwrap_err();
        let SweepFailure::Fit { attempts_log, .. } = &failure else {
            panic!("expected a fit failure, got {failure:?}");
        };
        let retries: Vec<(Retry, Option<i32>)> = attempts_log
            .iter()
            .map(|attempt| (attempt.retry, attempt.next_centre))
            .collect();
        assert_eq!(
            retries,
            [
                (Retry::ShiftAbsorbed, Some(100)),
                (Retry::NoAttemptsLeft, None)
            ]
        );
        assert_eq!(rig.moves().len(), 10, "two walks of 100..140");
    }

    /// A device failure on a later attempt keeps the account of the
    /// attempts that failed to fit before it.
    #[tokio::test]
    async fn a_rig_failure_keeps_the_attempts_that_failed_before_it() {
        let rig = ScriptedRig::starless();
        // The first walk's nine moves and two of the second's land.
        *rig.fail_move_after.lock().unwrap() = Some(11);
        let failure = run_sweep(&rig, 100, params()).await.unwrap_err();
        let SweepFailure::Rig {
            attempts_log,
            curve_points,
            ..
        } = &failure
        else {
            panic!("expected a rig failure, got {failure:?}");
        };
        assert_eq!(attempts_log.len(), 1);
        assert_eq!(attempts_log.first().unwrap().retry, Retry::SameGrid);
        assert_eq!(curve_points.len(), 11, "nine, then two of the retry");
    }

    #[tokio::test]
    async fn a_first_attempt_that_fits_logs_no_failed_attempt() {
        let rig = ScriptedRig::parabola(100);
        let outcome = run_sweep(&rig, 100, params()).await.unwrap();
        assert!(
            outcome.attempts_log.is_empty(),
            "{:?}",
            outcome.attempts_log
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
