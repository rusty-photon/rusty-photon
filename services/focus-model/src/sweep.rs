//! The V-curve sweep (docs/services/focus-model.md § The sweep).
//!
//! The grid and its clamp, the star list each attempt measures at every
//! frame, the two-star gate, the hyperbolic fit with its bound and its
//! threshold, the confirmation frame and the bounded retry, run here
//! through `rp`'s primitives. The curve itself is
//! `rp-focus-curve`'s, shared with `rp`'s guide-train sweep. The
//! measurement side stays `rp`'s: this module talks to it through
//! [`SweepOps`], which `workflow.rs` implements over the MCP client and
//! the unit tests implement over recorded curves.

use std::cmp::Ordering;

use async_trait::async_trait;
use rp_focus_curve::{fit_hyperbola, HyperbolaFit};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::FocusModelError;

/// The most grid positions a sweep may walk: a guardrail against a
/// configuration that would tie the rig up for hours.
pub const MAX_GRID_POINTS: usize = 1000;

/// The fewest listed stars a sample, or the confirmation frame, must
/// measure: one star's HFR is that star's, not the focus's.
pub const MIN_SAMPLE_STARS: u32 = 2;

/// The list keeps the reference frame's stars whose HFR lies within this
/// fraction of the frame's median HFR of it (`CCDciel`'s filter).
pub const LIST_HFR_BAND: f64 = 0.5;

/// Why a sweep sample was excluded from the fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    /// The sample measured fewer than [`MIN_SAMPLE_STARS`] listed stars.
    Sparse,
}

/// A star an attempt measures at every frame, where it was last seen.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ListedStar {
    pub x: f64,
    pub y: f64,
    /// The radius of the aperture it was last measured in.
    pub radius: f64,
}

/// A star the reference frame measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReferenceStar {
    pub x: f64,
    pub y: f64,
    pub radius: f64,
    pub hfr: f64,
}

/// What an attempt's reference frame detected.
#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceFrame {
    pub document_id: String,
    pub stars: Vec<ReferenceStar>,
}

/// An attempt's reference frame, as the result and the record keep it:
/// where it was taken and how many stars it listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub position: i32,
    pub document_id: String,
    pub listed: usize,
}

/// Why the confirmation frame was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationRejection {
    /// No listed star measured.
    Starless,
    /// Fewer than [`MIN_SAMPLE_STARS`] listed stars measured.
    Sparse,
    /// Worse than the lowest accepted sample by more than the tolerance.
    Worse,
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
    /// Why it was not accepted; `None` when it was.
    pub rejected: Option<ConfirmationRejection>,
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
    /// A fit whose R² falls below this is `poor_fit`; `None` enforces
    /// nothing.
    pub min_fit_r_squared: Option<f64>,
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
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FitError {
    #[error(
        "not enough stars: only {got} of {needed} required samples are accepted \
         (non-null HFR, past the sparse gate)"
    )]
    NotEnoughStars { got: usize, needed: usize },
    #[error("monotonic curve: {0}")]
    MonotonicCurve(String),
    /// The hyperbola fitted with its vertex outside the positions the
    /// accepted samples were measured at: a monotonic curve, whose
    /// retry centres on that vertex.
    #[error(
        "monotonic curve: fitted vertex {vertex} is outside sampled grid \
         [{sampled_min}, {sampled_max}]"
    )]
    VertexOutside {
        vertex: i32,
        sampled_min: i32,
        sampled_max: i32,
    },
    /// The fit held its centre among the samples and explained less of
    /// them than the train's `min_fit_r_squared` asks.
    #[error("poor fit: fit_r_squared {r_squared:.3} is below min_fit_r_squared {threshold}")]
    PoorFit { r_squared: f64, threshold: f64 },
}

impl FitError {
    /// The name this failure is recorded under.
    #[must_use]
    pub const fn outcome(&self) -> FitOutcome {
        match self {
            Self::NotEnoughStars { .. } => FitOutcome::NotEnoughStars,
            Self::MonotonicCurve(_) | Self::VertexOutside { .. } => FitOutcome::MonotonicCurve,
            Self::PoorFit { .. } => FitOutcome::PoorFit,
        }
    }

    /// The R² a `poor_fit` was refused on; `None` for the failures that
    /// produced no fit to judge.
    #[must_use]
    pub const fn fit_r_squared(&self) -> Option<f64> {
        match self {
            Self::PoorFit { r_squared, .. } => Some(*r_squared),
            Self::NotEnoughStars { .. } | Self::MonotonicCurve(_) | Self::VertexOutside { .. } => {
                None
            }
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
    /// A fit below the train's `min_fit_r_squared`.
    PoorFit,
}

impl FitOutcome {
    /// The name the record and the log carry.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotEnoughStars => "not_enough_stars",
            Self::MonotonicCurve => "monotonic_curve",
            Self::PoorFit => "poor_fit",
        }
    }
}

/// What a run did after an attempt that failed to fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retry {
    /// The next attempt walked the same grid.
    SameGrid,
    /// The next attempt's grid was moved toward the end of the accepted
    /// samples their lowest one sits nearer, as far as the bounds
    /// allowed: centred on the vertex a monotonic curve's fit
    /// extrapolated past that end, at most three half widths from the
    /// centre that failed, or by `half_width` when the fit placed no
    /// vertex there.
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
/// its grid was centred, how many stars its reference frame listed, what
/// the gate left of it, and what the run did next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedAttempt {
    pub attempt: u32,
    pub outcome: FitOutcome,
    /// The failure in words.
    pub error: String,
    pub centre: i32,
    /// The stars the attempt's reference frame listed; 0 made every point
    /// of the attempt starless.
    #[serde(default)]
    pub listed: usize,
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
        /// The last attempt's reference frame.
        reference: Option<Reference>,
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
        /// The reference frame of the attempt that was running, when it
        /// had taken one.
        reference: Option<Reference>,
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
    /// The reference frame of the attempt that produced this outcome.
    pub reference: Reference,
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
    /// Capture at the current position and detect its stars: an
    /// attempt's reference frame.
    async fn reference(&self) -> Result<ReferenceFrame, FocusModelError>;
    /// Capture at the current position and measure the listed stars,
    /// leaving in `list` where each was seen for the next frame. An empty
    /// list measures nothing, and the frame is still captured.
    async fn measure(&self, list: &mut Vec<ListedStar>) -> Result<Measurement, FocusModelError>;
    /// The caller's cancellation, checked between primitive calls.
    fn check_cancelled(&self) -> Result<(), FocusModelError>;
    /// One `notifications/progress` tick.
    async fn tick(&self, position: i32, measurement: &Measurement);
    /// The `notifications/progress` tick of a reference frame, naming the
    /// stars it listed.
    async fn tick_reference(&self, position: i32, listed: usize);
}

/// A measured HFR the fit may use: `None` for a starless frame and for
/// a non-finite reading, which would poison the lowest-sample choice,
/// the fit and the wing slope.
#[must_use]
pub fn finite_hfr(hfr: Option<f64>) -> Option<f64> {
    hfr.filter(|value| value.is_finite())
}

/// The stars of a reference frame an attempt measures at every frame.
///
/// Those whose HFR lies within [`LIST_HFR_BAND`] of the frame's median
/// HFR of it, which leaves out the saturated cores of a frame at focus,
/// reading large, and hot pixels and artefacts, reading small.
#[must_use]
pub fn star_list(stars: &[ReferenceStar]) -> Vec<ListedStar> {
    let mut hfrs: Vec<f64> = stars.iter().map(|star| star.hfr).collect();
    hfrs.sort_by(f64::total_cmp);
    let middle = hfrs.len() / 2;
    let median = match (
        hfrs.get(middle),
        middle.checked_sub(1).and_then(|i| hfrs.get(i)),
    ) {
        (Some(&upper), Some(&lower)) if hfrs.len().is_multiple_of(2) => f64::midpoint(lower, upper),
        (Some(&upper), _) => upper,
        (None, _) => return Vec::new(),
    };
    stars
        .iter()
        .filter(|star| (star.hfr - median).abs() < LIST_HFR_BAND * median)
        .map(|star| ListedStar {
            x: star.x,
            y: star.y,
            radius: star.radius,
        })
        .collect()
}

/// Mark every measurable sample with fewer than [`MIN_SAMPLE_STARS`]
/// listed stars measured as [`Rejection::Sparse`].
pub fn apply_star_floor(points: &mut [CurvePoint]) {
    for point in points.iter_mut().filter(|p| p.hfr.is_some()) {
        if point.star_count < MIN_SAMPLE_STARS {
            point.rejected = Some(Rejection::Sparse);
        }
    }
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

/// The confirmation verdict, `None` when accepted: the frame measures
/// at least [`MIN_SAMPLE_STARS`] listed stars, and no worse than the best
/// accepted sample by more than `tolerance`.
#[must_use]
pub fn confirmation_verdict(
    hfr: Option<f64>,
    star_count: u32,
    lowest_hfr: f64,
    tolerance: f64,
) -> Option<ConfirmationRejection> {
    match hfr {
        None => Some(ConfirmationRejection::Starless),
        Some(_) if star_count < MIN_SAMPLE_STARS => Some(ConfirmationRejection::Sparse),
        Some(hfr) if hfr > lowest_hfr * (1.0 + tolerance) => Some(ConfirmationRejection::Worse),
        Some(_) => None,
    }
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
/// Moved toward focus when the attempt says it lies past one end of its
/// accepted samples — after `monotonic_curve` the end [`monotonic_side`]
/// finds, after `not_enough_stars` the side [`one_sided_starvation`]
/// finds — clamped to the focuser's bounds; the same place otherwise,
/// and always after `poor_fit`, whose centre lay among the samples. The
/// move is the one [`vertex_centre`] finds when the fit extrapolated
/// its vertex past that end, and `half_width` when it placed none
/// there.
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
        FitError::MonotonicCurve(_) | FitError::VertexOutside { .. } => {
            monotonic_side(lowest_position, &accepted, curve_points)
        }
        FitError::NotEnoughStars { .. } => {
            one_sided_starvation(curve_points).unwrap_or(Ordering::Equal)
        }
        FitError::PoorFit { .. } => Ordering::Equal,
    };
    let by_half_width = match direction {
        Ordering::Less => centre.saturating_sub(half_width),
        Ordering::Greater => centre.saturating_add(half_width),
        Ordering::Equal => return same_grid,
    };
    let shifted = match error {
        FitError::VertexOutside { vertex, .. } => {
            vertex_centre(*vertex, direction, &accepted, centre, half_width)
        }
        _ => None,
    }
    .unwrap_or(by_half_width);
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

/// Where a monotonic curve's retry centres on the vertex its fit
/// extrapolated: the vertex, when it lies past the end of the accepted
/// samples `side` names, and no further from the `centre` that failed
/// than three half widths; `None` when it lies on the other side of
/// that end.
///
/// The wing the samples walk fixes the slope the defocus curve falls
/// along, so the vertex lands near focus. Three half widths put the
/// grid a grid's width past the end of the one that failed, and further
/// out than that the fit is guessing; the cap is measured from the
/// centre, not from the samples, because a grid whose half width is not
/// a whole number of steps ends short of it on the high side, and the
/// retry reaches as far either way.
fn vertex_centre(
    vertex: i32,
    side: Ordering,
    accepted: &[(i32, f64, u32)],
    centre: i32,
    half_width: i32,
) -> Option<i32> {
    let low_end = accepted.iter().map(|(position, _, _)| *position).min()?;
    let high_end = accepted.iter().map(|(position, _, _)| *position).max()?;
    let reach = half_width.saturating_mul(3);
    match side {
        Ordering::Less => (vertex < low_end).then_some(vertex.max(centre.saturating_sub(reach))),
        Ordering::Greater => {
            (vertex > high_end).then_some(vertex.min(centre.saturating_add(reach)))
        }
        Ordering::Equal => None,
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

/// The focuser step nearest a fitted position.
const fn nearest_step(position: f64) -> i32 {
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "`f64` to `i32` has no total spelling; `as` saturates at the rails, and the caller's grid-range check rejects a rail-hitting vertex"
    )]
    let step = position.round() as i32;
    step
}

/// Fit the hyperbola to the accepted samples, weighted by star count,
/// the curve's own failures folded into the sweep's.
fn fit(accepted: &[(i32, f64, u32)]) -> Result<HyperbolaFit, FitError> {
    fit_hyperbola(accepted).map_err(|error| match error {
        rp_focus_curve::FitError::NotEnoughStars { got } => {
            FitError::NotEnoughStars { got, needed: 3 }
        }
        rp_focus_curve::FitError::MonotonicCurve(why) => FitError::MonotonicCurve(why),
    })
}

/// What the gate-and-fit stage hands to the confirmation stage.
#[derive(Debug, Clone, Copy)]
struct FitStage {
    best_position: i32,
    best_hfr: f64,
    r_squared: f64,
    lowest_position: i32,
    lowest_hfr: f64,
    samples_used: usize,
}

/// Gate the sweep, fit the accepted samples, and validate the vertex
/// against the positions those samples were measured at — not the
/// grid that was asked for, which a focuser settling short of its
/// targets does not describe — then hold the fit to the train's
/// `min_fit_r_squared`. A centre outside the samples is
/// `monotonic_curve` whatever its R², so a one-wing sweep still shifts.
/// Pure: the caller decides what to do on failure.
fn gate_and_fit(
    curve_points: &mut [CurvePoint],
    params: SweepParams,
) -> Result<FitStage, FitError> {
    apply_star_floor(curve_points);
    let accepted: Vec<(i32, f64, u32)> = curve_points
        .iter()
        .filter_map(CurvePoint::accepted_sample)
        .collect();
    debug!(
        accepted = accepted.len(),
        rejected_sparse = curve_points.iter().filter(|p| p.rejected.is_some()).count(),
        "star floor applied"
    );
    if accepted.len() < params.min_fit_points {
        return Err(FitError::NotEnoughStars {
            got: accepted.len(),
            needed: params.min_fit_points,
        });
    }

    let fit = fit(&accepted)?;
    let best_position = nearest_step(fit.curve.centre);
    let (Some(sampled_min), Some(sampled_max)) = (
        accepted.iter().map(|(position, _, _)| *position).min(),
        accepted.iter().map(|(position, _, _)| *position).max(),
    ) else {
        return Err(FitError::MonotonicCurve(
            "no accepted sample to bound the vertex with".to_owned(),
        ));
    };
    if best_position < sampled_min || best_position > sampled_max {
        return Err(FitError::VertexOutside {
            vertex: best_position,
            sampled_min,
            sampled_max,
        });
    }
    if let Some(threshold) = params
        .min_fit_r_squared
        .filter(|threshold| fit.r_squared < *threshold)
    {
        return Err(FitError::PoorFit {
            r_squared: fit.r_squared,
            threshold,
        });
    }
    let Some((lowest_position, lowest_hfr, _)) = lowest_sample(&accepted) else {
        return Err(FitError::MonotonicCurve(
            "no accepted sample to hold the confirmation against".to_owned(),
        ));
    };
    Ok(FitStage {
        best_position,
        best_hfr: fit.curve.minimum,
        r_squared: fit.r_squared,
        lowest_position,
        lowest_hfr,
        samples_used: accepted.len(),
    })
}

/// An attempt's reference frame at its centre, and the list it gives:
/// the first attempt finds the focuser there already, a retry moves to
/// its own centre first.
async fn take_reference<O: SweepOps + ?Sized>(
    ops: &O,
    centre: i32,
    first: bool,
) -> Result<(Reference, Vec<ListedStar>), FocusModelError> {
    ops.check_cancelled()?;
    let position = if first {
        centre
    } else {
        ops.move_focuser(centre).await?
    };
    ops.check_cancelled()?;
    let frame = ops.reference().await?;
    let list = star_list(&frame.stars);
    ops.tick_reference(position, list.len()).await;
    debug!(
        position,
        detected = frame.stars.len(),
        listed = list.len(),
        "reference frame measured"
    );
    let reference = Reference {
        position,
        document_id: frame.document_id,
        listed: list.len(),
    };
    Ok((reference, list))
}

/// One walk of `grid`: move, capture, measure the listed stars at every
/// position, pushing each measured point into `curve_points` as it is
/// taken so a failure part-way leaves the caller the samples it already
/// has.
async fn walk<O: SweepOps + ?Sized>(
    ops: &O,
    grid: &[i32],
    list: &mut Vec<ListedStar>,
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
        let measurement = ops.measure(list).await?;
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

/// Move to the fitted vertex, measure the confirmation frame on the
/// list, and settle on the better of the two measured positions.
async fn confirm<O: SweepOps + ?Sized>(
    ops: &O,
    params: SweepParams,
    stage: FitStage,
    list: &mut Vec<ListedStar>,
) -> Result<(Confirmation, i32, f64), FocusModelError> {
    ops.check_cancelled()?;
    let moved_to = ops.move_focuser(stage.best_position).await?;
    ops.check_cancelled()?;
    let measurement = ops.measure(list).await?;
    ops.tick(moved_to, &measurement).await;
    let hfr = finite_hfr(measurement.hfr);
    let rejected = confirmation_verdict(
        hfr,
        measurement.star_count,
        stage.lowest_hfr,
        params.confirmation_tolerance,
    );
    let accepted = rejected.is_none();
    debug!(
        best_position = stage.best_position,
        confirmation_hfr = ?hfr,
        confirmation_stars = measurement.star_count,
        lowest_position = stage.lowest_position,
        lowest_hfr = stage.lowest_hfr,
        ?rejected,
        "confirmation frame measured"
    );
    let confirmation = Confirmation {
        document_id: measurement.document_id,
        hfr,
        star_count: measurement.star_count,
        accepted,
        rejected,
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
            let measurement = ops.measure(list).await?;
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

/// Run the V-curve around `centre`, where the focuser is when it starts.
///
/// Each attempt takes a reference frame, walks the grid measuring the
/// stars it listed, gates, fits and confirms; a failed fit is retried
/// while attempts remain.
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
        let (reference, mut list) = match take_reference(ops, centre, attempts == 1).await {
            Ok(taken) => taken,
            Err(error) => {
                return Err(SweepFailure::Rig {
                    error: Box::new(error),
                    attempts_log,
                    reference: None,
                    curve_points: measured,
                })
            }
        };
        let mut curve_points = Vec::with_capacity(grid.len());
        if let Err(error) = walk(ops, &grid, &mut list, &mut curve_points).await {
            measured.append(&mut curve_points);
            return Err(SweepFailure::Rig {
                error: Box::new(error),
                attempts_log,
                reference: Some(reference),
                curve_points: measured,
            });
        }

        let error = match gate_and_fit(&mut curve_points, params) {
            Ok(stage) => {
                let wing_slope = wing_slope(&curve_points);
                let confirmed = confirm(ops, params, stage, &mut list).await;
                measured.append(&mut curve_points);
                let (confirmation, position, hfr) = match confirmed {
                    Ok(confirmed) => confirmed,
                    Err(error) => {
                        return Err(SweepFailure::Rig {
                            error: Box::new(error),
                            attempts_log,
                            reference: Some(reference),
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
                    reference,
                    curve_points: measured,
                });
            }
            Err(error) => error,
        };

        // The retry reads the attempt that just failed, before its
        // points join the run's.
        let attempt = Attempt {
            number: attempts,
            centre,
            grid: &grid,
            reference: &reference,
            curve_points: &curve_points,
        };
        let (failed, next_grid) = failed_attempt(&attempt, &error, params);
        let next_centre = failed.next_centre;
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
            reference: Some(reference),
            curve_points: measured,
        });
    }
}

/// One attempt as the run walked it.
struct Attempt<'a> {
    number: u32,
    centre: i32,
    grid: &'a [i32],
    reference: &'a Reference,
    curve_points: &'a [CurvePoint],
}

/// The account of an attempt that failed to fit, logged, and the grid
/// the next attempt walks, if any: where the retry goes, or why none is
/// made.
fn failed_attempt(
    attempt: &Attempt<'_>,
    error: &FitError,
    params: SweepParams,
) -> (FailedAttempt, Option<Vec<i32>>) {
    let plan = (attempt.number < params.max_attempts).then(|| {
        retry_centre(
            attempt.centre,
            error,
            attempt.curve_points,
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
    let (accepted, sparse, starless) = gate_counts(attempt.curve_points);
    let failed = FailedAttempt {
        attempt: attempt.number,
        outcome: error.outcome(),
        error: error.to_string(),
        centre: attempt.centre,
        listed: attempt.reference.listed,
        points: attempt.curve_points.len(),
        accepted,
        sparse,
        starless,
        retry,
        next_centre,
    };
    log_failed_attempt(
        &failed,
        params.max_attempts,
        attempt.grid,
        next_grid.as_deref(),
    );
    (failed, next_grid)
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
        listed = failed.listed,
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

    use rp_focus_curve::Hyperbola;

    use super::*;
    use crate::config::{SweepConfig, TrainConfig};
    use crate::sizing::{blur_constant, plan_sweep, predicted_slope_per_step, Optics};

    fn params() -> SweepParams {
        SweepParams {
            step_size: 10,
            half_width: 40,
            min_fit_points: 5,
            min_fit_r_squared: None,
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
        /// Where the focuser is before the first move: the centre the
        /// run is started on.
        start: Mutex<i32>,
        references: Mutex<Vec<i32>>,
    }

    impl ScriptedRig {
        fn hyperbola(vertex: i32) -> Self {
            Self {
                curve: Box::new(move |position| {
                    let dx = f64::from(position - vertex);
                    Some((dx / 10.0).hypot(1.0))
                }),
                star_count: 100,
                moves: Mutex::new(Vec::new()),
                cancel_after: Mutex::new(None),
                fail_move_after: Mutex::new(None),
                short_by: Mutex::new(0),
                start: Mutex::new(0),
                references: Mutex::new(Vec::new()),
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
                start: Mutex::new(0),
                references: Mutex::new(Vec::new()),
            }
        }

        fn moves(&self) -> Vec<i32> {
            self.moves.lock().unwrap().clone()
        }

        /// Where each reference frame was taken.
        fn references(&self) -> Vec<i32> {
            self.references.lock().unwrap().clone()
        }

        /// Where the focuser is: the last move's read-back, or the
        /// run's starting centre before any move.
        fn position(&self) -> i32 {
            self.moves
                .lock()
                .unwrap()
                .last()
                .copied()
                .unwrap_or_else(|| *self.start.lock().unwrap())
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

        /// Lists `star_count` stars, whatever the scripted sky does at
        /// the centre: the clouds and edges a scenario scripts act on
        /// the walk, and a rig of no stars lists none.
        async fn reference(&self) -> Result<ReferenceFrame, FocusModelError> {
            let position = self.position();
            let stars = (0..self.star_count)
                .map(|i| ReferenceStar {
                    x: f64::from(i),
                    y: 0.0,
                    radius: 3.0,
                    hfr: 1.0,
                })
                .collect();
            self.references.lock().unwrap().push(position);
            Ok(ReferenceFrame {
                document_id: format!("ref-{position}"),
                stars,
            })
        }

        async fn measure(
            &self,
            list: &mut Vec<ListedStar>,
        ) -> Result<Measurement, FocusModelError> {
            let position = self.position();
            let hfr = (self.curve)(position).filter(|_| !list.is_empty());
            Ok(Measurement {
                hfr,
                star_count: if hfr.is_some() {
                    u32::try_from(list.len()).unwrap()
                } else {
                    0
                },
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

        async fn tick_reference(&self, _position: i32, _listed: usize) {}
    }

    /// Run the sweep on `rig` from `centre`, where the focuser starts.
    async fn sweep(
        rig: &ScriptedRig,
        centre: i32,
        params: SweepParams,
    ) -> Result<SweepOutcome, SweepFailure> {
        *rig.start.lock().unwrap() = centre;
        run_sweep(rig, centre, params).await
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
    fn a_sample_of_one_listed_star_is_sparse_and_a_starless_one_is_left_alone() {
        let mut points = vec![
            point(0, Some(4.0), 1),
            point(10, Some(2.0), 100),
            point(20, None, 0),
            point(30, Some(4.2), 2),
        ];
        apply_star_floor(&mut points);
        assert_eq!(points[0].rejected, Some(Rejection::Sparse));
        assert_eq!(points[1].rejected, None);
        assert_eq!(points[2].rejected, None, "a starless frame is not gated");
        assert_eq!(points[3].rejected, None, "two stars pass the floor");
    }

    #[test]
    fn the_floor_does_not_depend_on_the_densest_frame() {
        // A sample of 3 stars beside one of 2,000 is accepted: the gate
        // is two listed stars, not a fraction of the sweep's best count.
        let mut points = vec![point(0, Some(9.0), 3), point(10, Some(1.0), 2_000)];
        apply_star_floor(&mut points);
        assert!(points.iter().all(|p| p.rejected.is_none()));
    }

    fn reference_star(x: f64, hfr: f64) -> ReferenceStar {
        ReferenceStar {
            x,
            y: 10.0,
            radius: 3.0 * hfr,
            hfr,
        }
    }

    #[test]
    fn the_list_keeps_the_stars_within_half_the_median_hfr_of_it() {
        // Median 1.0: a saturated core at 4.7 and a hot pixel at 0.3 are
        // left out, 0.6 and 1.4 kept; 0.5 and 1.5 lie on the band's edge
        // and are left out.
        let stars = [
            reference_star(1.0, 1.0),
            reference_star(2.0, 4.7),
            reference_star(3.0, 0.3),
            reference_star(4.0, 0.6),
            reference_star(5.0, 1.4),
            reference_star(6.0, 1.0),
            reference_star(7.0, 0.5),
            reference_star(8.0, 1.5),
            reference_star(9.0, 1.0),
        ];
        let listed: Vec<f64> = star_list(&stars).iter().map(|star| star.x).collect();
        assert_eq!(listed, [1.0, 4.0, 5.0, 6.0, 9.0]);
        let first = star_list(&stars)[0];
        assert_eq!(
            first,
            ListedStar {
                x: 1.0,
                y: 10.0,
                radius: 3.0
            }
        );
    }

    #[test]
    fn the_list_of_a_frame_with_no_stars_is_empty() {
        assert_eq!(star_list(&[]), []);
    }

    #[test]
    fn the_lists_median_averages_the_two_middle_hfrs_of_an_even_count() {
        // The median of 1.0, 1.9, 2.1 and 4.0 is 2.0, and the band
        // (1.0, 3.0) leaves out 1.0, which the lower middle's band
        // (0.95, 2.85) would keep, and 4.0.
        let stars = [
            reference_star(1.0, 1.0),
            reference_star(2.0, 1.9),
            reference_star(3.0, 2.1),
            reference_star(4.0, 4.0),
        ];
        let median_stars: Vec<f64> = star_list(&stars).iter().map(|star| star.x).collect();
        assert_eq!(median_stars, [2.0, 3.0]);
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

    // --- the retry onto a monotonic curve's extrapolated vertex ---

    /// One wing rising from 80 to 120: the curve fell toward 80, so
    /// focus lies below the samples.
    fn rising_wing() -> Vec<CurvePoint> {
        (80..=120)
            .step_by(10)
            .map(|position| point(position, Some(f64::from(position - 60) / 10.0), 100))
            .collect()
    }

    fn vertex_outside(vertex: i32) -> FitError {
        FitError::VertexOutside {
            vertex,
            sampled_min: 80,
            sampled_max: 120,
        }
    }

    /// Half a width would centre the next grid on 80, the samples' end.
    #[test]
    fn a_vertex_past_the_low_end_centres_the_retry_on_it() {
        assert_eq!(
            retry_centre(100, &vertex_outside(65), &rising_wing(), 20, (None, None)),
            RetryPlan {
                retry: Retry::Shift,
                centre: 65
            }
        );
    }

    #[test]
    fn a_vertex_past_the_high_end_centres_the_retry_on_it() {
        let falling: Vec<CurvePoint> = (80..=120)
            .step_by(10)
            .map(|position| point(position, Some(f64::from(140 - position) / 10.0), 100))
            .collect();
        assert_eq!(
            retry_centre(100, &vertex_outside(150), &falling, 20, (None, None)),
            RetryPlan {
                retry: Retry::Shift,
                centre: 150
            }
        );
    }

    /// Further out than a grid's width past the samples the fit is
    /// guessing: the centre moves three half widths, 60, and no further.
    #[test]
    fn a_vertex_far_below_the_samples_moves_the_centre_three_half_widths() {
        assert_eq!(
            retry_centre(100, &vertex_outside(-200), &rising_wing(), 20, (None, None)),
            RetryPlan {
                retry: Retry::Shift,
                centre: 40
            }
        );
    }

    #[test]
    fn a_vertex_far_above_the_samples_moves_the_centre_three_half_widths() {
        let falling: Vec<CurvePoint> = (80..=120)
            .step_by(10)
            .map(|position| point(position, Some(f64::from(140 - position) / 10.0), 100))
            .collect();
        assert_eq!(
            retry_centre(100, &vertex_outside(400), &falling, 20, (None, None)),
            RetryPlan {
                retry: Retry::Shift,
                centre: 160
            }
        );
    }

    /// A vertex past the end the lowest sample does not sit nearer
    /// contradicts the samples: the shift is half a width toward the
    /// lowest one.
    #[test]
    fn a_vertex_past_the_other_end_shifts_half_a_width() {
        assert_eq!(
            retry_centre(100, &vertex_outside(130), &rising_wing(), 20, (None, None)),
            RetryPlan {
                retry: Retry::Shift,
                centre: 80
            }
        );
    }

    /// Frames the sky took past the lowest sample keep the grid,
    /// whatever the fit extrapolated through the samples that are left.
    #[test]
    fn a_vertex_past_frames_the_sky_took_repeats_the_grid() {
        let mut points = rising_wing();
        points.insert(0, point(70, None, 0));
        assert_eq!(
            retry_centre(100, &vertex_outside(65), &points, 20, (None, None)),
            RetryPlan {
                retry: Retry::SameGrid,
                centre: 100
            }
        );
    }

    #[test]
    fn a_vertex_past_a_bound_centres_the_retry_on_the_bound() {
        assert_eq!(
            retry_centre(
                100,
                &vertex_outside(65),
                &rising_wing(),
                20,
                (Some(72), None)
            ),
            RetryPlan {
                retry: Retry::Shift,
                centre: 72
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
        apply_star_floor(&mut points);
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
        apply_star_floor(&mut points);
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
            point(90, Some(1.5), 1),
            point(100, Some(2.0), 100),
            point(110, Some(5.0), 100),
            point(120, None, 0),
        ];
        apply_star_floor(&mut points);
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
        assert_eq!(gate_counts(&starved_above()), (4, 3, 2));
    }

    /// The log line and the record name an outcome and a retry the same
    /// way.
    #[test]
    fn the_logged_names_are_the_recorded_names() {
        for outcome in [
            FitOutcome::NotEnoughStars,
            FitOutcome::MonotonicCurve,
            FitOutcome::PoorFit,
        ] {
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
    fn the_confirmation_holds_the_frame_to_the_floor_and_the_tolerance() {
        assert_eq!(confirmation_verdict(Some(1.1), 100, 1.0, 0.25), None);
        assert_eq!(
            confirmation_verdict(Some(1.1), 2, 1.0, 0.25),
            None,
            "two listed stars are enough"
        );
        assert_eq!(
            confirmation_verdict(Some(1.1), 1, 1.0, 0.25),
            Some(ConfirmationRejection::Sparse),
            "one listed star is not a confirmation"
        );
        assert_eq!(
            confirmation_verdict(Some(1.5), 100, 1.0, 0.25),
            Some(ConfirmationRejection::Worse),
            "1.5 is more than 25 % worse than 1.0"
        );
        assert_eq!(
            confirmation_verdict(None, 0, 1.0, 0.25),
            Some(ConfirmationRejection::Starless)
        );
    }

    #[test]
    fn the_confirmations_reason_reads_as_the_contract_names_it() {
        for (rejected, name) in [
            (ConfirmationRejection::Starless, "starless"),
            (ConfirmationRejection::Sparse, "sparse"),
            (ConfirmationRejection::Worse, "worse"),
        ] {
            assert_eq!(
                serde_json::to_value(rejected).unwrap(),
                serde_json::json!(name)
            );
        }
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
        let rig = ScriptedRig::hyperbola(100);
        let outcome = sweep(&rig, 100, params()).await.unwrap();
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.best_position, 100);
        assert!(outcome.confirmed);
        assert_eq!(outcome.position, 100);
        assert_eq!(outcome.samples_used, 9);
        assert_eq!(outcome.curve_points.len(), 9);
        assert!(outcome.wing_slope.is_some());
        // Nine grid moves, then the move to the vertex: the reference
        // frame is taken where the focuser already is, at the centre.
        assert_eq!(rig.moves().len(), 10);
        assert_eq!(rig.references(), [100]);
        assert_eq!(
            outcome.reference,
            Reference {
                position: 100,
                document_id: "ref-100".to_owned(),
                listed: 100,
            }
        );
        assert_eq!(outcome.confirmation.rejected, None);
        assert!(outcome.curve_points.iter().all(|p| p.star_count == 100));
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
            start: Mutex::new(0),
            references: Mutex::new(Vec::new()),
        };
        let outcome = sweep(&rig, 100, params()).await.unwrap();
        assert!(!outcome.confirmed);
        assert_eq!(outcome.position, 90, "the lowest accepted sample");
        assert_eq!(outcome.hfr, 1.1);
        assert_eq!(rig.moves().last(), Some(&90));
        assert_eq!(
            outcome.confirmation.rejected,
            Some(ConfirmationRejection::Worse)
        );
    }

    #[tokio::test]
    async fn a_starless_sweep_is_repeated_and_carries_every_attempt() {
        let rig = ScriptedRig::starless();
        let failure = sweep(&rig, 100, params()).await.unwrap_err();
        let SweepFailure::Fit {
            error,
            attempts,
            attempts_log,
            reference,
            curve_points,
        } = failure
        else {
            panic!("expected a fit failure, got {failure:?}");
        };
        assert_eq!(attempts, 2, "max_attempts is 2");
        assert_eq!(error.outcome(), FitOutcome::NotEnoughStars);
        assert_eq!(curve_points.len(), 18, "both attempts, nine points each");
        assert!(curve_points.iter().all(|p| p.hfr.is_none()));
        // Two full walks, the move back to the centre for the second
        // reference frame, and no move to a vertex that never fitted.
        assert_eq!(rig.moves().len(), 19);
        assert_eq!(
            rig.references(),
            [100, 100],
            "one reference frame per attempt, at its centre"
        );
        assert_eq!(
            reference,
            Some(Reference {
                position: 100,
                document_id: "ref-100".to_owned(),
                listed: 0,
            }),
            "a reference frame with no stars lists none"
        );
        assert_eq!(
            attempts_log,
            [
                FailedAttempt {
                    attempt: 1,
                    outcome: FitOutcome::NotEnoughStars,
                    error: error.to_string(),
                    centre: 100,
                    listed: 0,
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
                    listed: 0,
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
        let failure = sweep(&rig, 100, single).await.unwrap_err();
        assert!(
            matches!(failure, SweepFailure::Fit { attempts: 1, .. }),
            "{failure:?}"
        );
        assert_eq!(rig.moves().len(), 9);
    }

    #[tokio::test]
    async fn a_cancellation_stops_the_walk_where_it_is() {
        let rig = ScriptedRig::hyperbola(100);
        // Two checks around the reference frame, two around the first
        // point, one before the second point's move.
        *rig.cancel_after.lock().unwrap() = Some(5);
        let failure = sweep(&rig, 100, params()).await.unwrap_err();
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
            lowest_position: 90,
            lowest_hfr: 1.1,
            samples_used: 5,
        };
        let mut list = vec![ListedStar {
            x: 1.0,
            y: 1.0,
            radius: 3.0,
        }];
        let err = confirm(&rig, params(), stage, &mut list).await.unwrap_err();
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
        let rig = ScriptedRig::hyperbola(58);
        // Every move lands two steps short, so the samples run
        // [58, 138] while the requested grid ran [60, 140].
        *rig.short_by.lock().unwrap() = 2;
        let outcome = sweep(&rig, 100, params()).await.unwrap();
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
        let failure = sweep(&rig, 100, single).await.unwrap_err();
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
        let rig = ScriptedRig::hyperbola(100);
        // Nine grid moves land; the move to the vertex does not.
        *rig.fail_move_after.lock().unwrap() = Some(9);
        let failure = sweep(&rig, 100, params()).await.unwrap_err();
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
        let rig = ScriptedRig::hyperbola(vertex);
        ScriptedRig {
            curve: Box::new(move |position| {
                let dx = f64::from(position - vertex);
                (position < edge).then_some((dx / 10.0).hypot(1.0))
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
        let outcome = sweep(&rig, 100, params()).await.unwrap();
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
                listed: 100,
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
            Some(&60),
            "the move to the shifted centre"
        );
        assert_eq!(
            rig.references(),
            [100, 60],
            "a reference frame at each attempt's centre"
        );
        assert_eq!(
            rig.moves().get(10),
            Some(&20),
            "the second walk starts half a width lower"
        );
    }

    /// A sweep started two and a half half widths above focus walks one
    /// wing, and its fit extrapolates the vertex to focus: the retry
    /// centres there and fits, where half a width would have left focus
    /// below the second grid too.
    #[tokio::test]
    async fn a_sweep_started_off_focus_centres_its_retry_on_the_vertex_and_fits() {
        let rig = ScriptedRig::hyperbola(0);
        let outcome = sweep(&rig, 100, params()).await.unwrap();
        assert_eq!(outcome.attempts, 2);
        assert_eq!(outcome.best_position, 0);
        assert!(outcome.confirmed);
        assert_eq!(
            outcome.attempts_log,
            [FailedAttempt {
                attempt: 1,
                outcome: FitOutcome::MonotonicCurve,
                error: "monotonic curve: fitted vertex 0 is outside sampled grid [60, 140]"
                    .to_owned(),
                centre: 100,
                listed: 100,
                points: 9,
                accepted: 9,
                sparse: 0,
                starless: 0,
                retry: Retry::Shift,
                next_centre: Some(0),
            }]
        );
        assert_eq!(
            rig.references(),
            [100, 0],
            "the second reference frame is taken at the vertex"
        );
    }

    /// A vertex more than three half widths out moves the centre three
    /// half widths; the walk there extrapolates it again, now within
    /// reach, and the third attempt centres on it and fits.
    #[tokio::test]
    async fn a_vertex_far_past_the_samples_is_reached_three_half_widths_at_a_time() {
        let rig = ScriptedRig::hyperbola(-100);
        let three = SweepParams {
            max_attempts: 3,
            ..params()
        };
        let outcome = sweep(&rig, 100, three).await.unwrap();
        assert_eq!(outcome.attempts, 3);
        assert_eq!(outcome.best_position, -100);
        assert!(outcome.confirmed);
        let retries: Vec<(Retry, Option<i32>)> = outcome
            .attempts_log
            .iter()
            .map(|attempt| (attempt.retry, attempt.next_centre))
            .collect();
        assert_eq!(
            retries,
            [(Retry::Shift, Some(-20)), (Retry::Shift, Some(-100))]
        );
    }

    /// rig2's R sizing, step 22 and half width 86, walks a grid that
    /// ends 18 steps short of its half width above the centre: a sweep
    /// started 300 steps from focus on either side still centres its
    /// retry on a grid that holds focus, and fits on its second attempt.
    #[tokio::test]
    async fn a_sweep_started_300_steps_off_focus_at_rig2s_r_sizing_fits_on_its_second_attempt() {
        let r = SweepParams {
            step_size: 22,
            half_width: 86,
            ..params()
        };
        for start in [29_272, 29_872] {
            let rig = ScriptedRig::hyperbola(29_572);
            let outcome = sweep(&rig, start, r).await.unwrap();
            assert_eq!(
                (outcome.attempts, outcome.best_position, outcome.confirmed),
                (2, 29_572, true),
                "the sweep started at {start}: {:?}",
                outcome.attempts_log
            );
        }
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
            ..ScriptedRig::hyperbola(0)
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
                (dx / 10.0).hypot(1.0)
            },
            |position| position >= 100,
            17,
        );
        let wide = SweepParams {
            half_width: 80,
            ..params()
        };
        let outcome = sweep(&rig, 100, wide).await.unwrap();
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
            Some(&100),
            "the move to the retry's centre"
        );
        assert_eq!(
            rig.moves().get(18),
            Some(&20),
            "the second walk is the first one again"
        );
    }

    /// The roster grid: ±400 in steps of 100.
    fn roster() -> SweepParams {
        SweepParams {
            step_size: 100,
            half_width: 400,
            ..params()
        }
    }

    /// Focus where the prediction put it, on the centre, and a cloud
    /// over it and the frames above: the fit through the one wing left
    /// puts its centre past the samples, so the attempt fails as a
    /// monotonic curve. A shift would start the next grid at focus, and
    /// the same cloud would leave it the same wing; the same grid, under
    /// a cleared sky, fits. The four samples the cloud leaves are let
    /// fit, so the attempt reaches the curve.
    #[tokio::test]
    async fn a_monotonic_curve_a_cloud_cut_short_at_focus_fits_on_the_same_grid() {
        let rig = clouded(roster_v, |position| position >= 30_000, 9);
        let four = SweepParams {
            min_fit_points: 4,
            ..roster()
        };
        let outcome = sweep(&rig, 30_000, four).await.unwrap();
        assert_eq!(outcome.attempts, 2);
        assert_eq!(outcome.best_position, 30_000);
        assert!(outcome.confirmed);
        let first = outcome.attempts_log.first().unwrap();
        assert_eq!(
            (first.outcome, first.accepted, first.retry),
            (FitOutcome::MonotonicCurve, 4, Retry::SameGrid)
        );
    }

    /// The same sweep walked downward, as an inward backlash approach
    /// walks it: the cloud takes focus and the frames below it, the
    /// curve falls toward the bottom of what is left, and the same grid
    /// fits.
    #[tokio::test]
    async fn a_descending_monotonic_curve_a_cloud_cut_short_at_focus_fits_on_the_same_grid() {
        let rig = clouded(roster_v, |position| position <= 30_000, 9);
        let descending = SweepParams {
            min_fit_points: 4,
            direction: Direction::Descending,
            ..roster()
        };
        let outcome = sweep(&rig, 30_000, descending).await.unwrap();
        assert_eq!(outcome.attempts, 2);
        assert_eq!(outcome.best_position, 30_000);
        assert!(outcome.confirmed);
        let first = outcome.attempts_log.first().unwrap();
        assert_eq!(
            (first.outcome, first.accepted, first.retry),
            (FitOutcome::MonotonicCurve, 4, Retry::SameGrid)
        );
    }

    /// A cloud over the frames above focus leaves the wing below it,
    /// focus included: the hyperbola through that wing bottoms out at
    /// focus, so the first walk places it.
    #[tokio::test]
    async fn a_wing_that_reaches_focus_places_it_on_the_first_walk() {
        let rig = clouded(roster_v, |position| position >= 30_100, 9);
        let outcome = sweep(&rig, 30_000, roster()).await.unwrap();
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.best_position, 30_000);
        assert!(outcome.confirmed);
        assert_eq!(outcome.samples_used, 5);
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
        let failure = sweep(&rig, 100, tight).await.unwrap_err();
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
        let failure = sweep(&rig, 100, pinned).await.unwrap_err();
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
        assert_eq!(
            rig.moves().len(),
            11,
            "two walks of 100..140, and the move back to the centre for the second reference frame"
        );
    }

    /// A device failure on a later attempt keeps the account of the
    /// attempts that failed to fit before it.
    #[tokio::test]
    async fn a_rig_failure_keeps_the_attempts_that_failed_before_it() {
        let rig = ScriptedRig::starless();
        // The first walk's nine moves, the move to the retry's centre and
        // two of the second walk's land.
        *rig.fail_move_after.lock().unwrap() = Some(12);
        let failure = sweep(&rig, 100, params()).await.unwrap_err();
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
        let rig = ScriptedRig::hyperbola(100);
        let outcome = sweep(&rig, 100, params()).await.unwrap();
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

    // --- the hyperbola: its threshold, across trains, on the rig ---

    /// The curve's own refusal keeps the name and the shape the sweep
    /// records it under.
    #[test]
    fn the_curves_failures_keep_the_sweeps_names() {
        let err = fit(&[(0, 1.0, 10), (10, 2.0, 0), (20, 3.0, 0)]).unwrap_err();
        assert_eq!(err, FitError::NotEnoughStars { got: 1, needed: 3 });
        let err = fit(&[(0, 2.0, 10), (10, 2.0, 10), (20, 2.0, 10)]).unwrap_err();
        assert_eq!(err.outcome(), FitOutcome::MonotonicCurve);
    }

    /// A V around 100 with every other sample 20 % high: a fit with its
    /// centre among the samples and an R² short of 1.
    fn ragged(position: i32) -> f64 {
        let value = (f64::from(position - 100) / 10.0).hypot(1.0);
        if (position / 10) % 2 == 0 {
            value * 1.2
        } else {
            value
        }
    }

    fn ragged_points() -> Vec<CurvePoint> {
        (60..=140)
            .step_by(10)
            .map(|position| point(position, Some(ragged(position)), 100))
            .collect()
    }

    fn with_threshold(threshold: Option<f64>) -> SweepParams {
        SweepParams {
            min_fit_r_squared: threshold,
            ..params()
        }
    }

    #[test]
    fn without_a_threshold_a_ragged_fit_is_reported_not_refused() {
        let stage = gate_and_fit(&mut ragged_points(), params()).unwrap();
        assert_eq!(stage.best_position, 100);
        assert!(stage.r_squared < 0.99, "{stage:?}");
    }

    /// The threshold holds a fit to what it asks and no further: an R²
    /// equal to it passes, and 0 accepts every fit.
    #[test]
    fn a_fit_at_the_threshold_passes_and_zero_accepts_every_fit() {
        let r_squared = gate_and_fit(&mut ragged_points(), params())
            .unwrap()
            .r_squared;
        gate_and_fit(&mut ragged_points(), with_threshold(Some(r_squared))).unwrap();
        gate_and_fit(&mut ragged_points(), with_threshold(Some(0.0))).unwrap();
    }

    /// Samples on one wing fit well with their centre extrapolated past
    /// them: that is a monotonic curve, which shifts, whatever the
    /// threshold would have said.
    #[test]
    fn a_centre_outside_the_samples_is_a_monotonic_curve_whatever_the_threshold() {
        let mut points: Vec<CurvePoint> = (60..=140)
            .step_by(10)
            .map(|position| point(position, Some((f64::from(position) / 10.0).hypot(1.0)), 100))
            .collect();
        let err = gate_and_fit(&mut points, with_threshold(Some(1.0))).unwrap_err();
        assert_eq!(err.outcome(), FitOutcome::MonotonicCurve, "{err}");
    }

    /// A fit below the threshold is `poor_fit`: retried on the same grid,
    /// since the curve has its minimum among the samples, then the run
    /// fails naming the R² and the threshold, and carries the R².
    #[tokio::test]
    async fn a_fit_below_the_threshold_is_a_poor_fit_retried_on_the_same_grid() {
        let rig = ScriptedRig {
            curve: Box::new(|position| Some(ragged(position))),
            ..ScriptedRig::hyperbola(0)
        };
        let failure = sweep(&rig, 100, with_threshold(Some(0.99)))
            .await
            .unwrap_err();
        let SweepFailure::Fit {
            error,
            attempts,
            attempts_log,
            curve_points,
            ..
        } = &failure
        else {
            panic!("expected a fit failure, got {failure:?}");
        };
        let FitError::PoorFit {
            r_squared,
            threshold,
        } = error
        else {
            panic!("expected a poor fit, got {error:?}");
        };
        assert!(*r_squared < 0.99, "{error:?}");
        assert_eq!(*threshold, 0.99);
        assert_eq!(error.fit_r_squared(), Some(*r_squared));
        assert!(
            error.to_string().starts_with("poor fit: fit_r_squared 0.9"),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .ends_with("is below min_fit_r_squared 0.99"),
            "{error}"
        );
        assert_eq!(*attempts, 2);
        assert_eq!(curve_points.len(), 18, "both walks are kept");
        assert_eq!(
            attempts_log
                .iter()
                .map(|attempt| (attempt.outcome, attempt.retry, attempt.next_centre))
                .collect::<Vec<_>>(),
            [
                (FitOutcome::PoorFit, Retry::SameGrid, Some(100)),
                (FitOutcome::PoorFit, Retry::NoAttemptsLeft, None),
            ]
        );
        assert_eq!(
            rig.moves().get(10),
            Some(&60),
            "the second walk is the first one again, after the move to its centre"
        );
    }

    /// A deterministic xorshift, so the noisy curves are the same on
    /// every run.
    struct Noise(u64);

    impl Noise {
        fn unit(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1_u64 << 53) as f64
        }
    }

    /// Curves generated from the optics of a spread of trains — f/3.9 to
    /// f/10, 0.4 to 10 µm a step, seeing of 1.2" to 4.5", open and
    /// obstructed, the grid centred up to 0.3 half widths off focus —
    /// each sized as the provider sizes it, measured with 5 % noise and
    /// star counts that fall with defocus: the fit places focus within
    /// half a critical focus zone, the span the prediction treats as the
    /// same focus. The hyperbola is the defocus curve itself, so it must
    /// hold for every train, not one rig. A grid the critical focus zone
    /// leaves too few positions to fit is refused before any motion, so
    /// it is no case of the fit's.
    #[test]
    fn the_hyperbola_places_focus_on_curves_generated_across_trains() {
        const FOCUS: i32 = 30_000;
        let mut noise = Noise(0x0005_9000_1187);
        let mut fitted = 0;
        let (aperture_mm, pixel_um) = (100.0, 3.76);
        for focal_ratio in [3.9, 5.0, 7.0, 10.0] {
            for microns_per_step in [0.4, 1.0, 2.5, 10.0] {
                for seeing_arcsec in [1.2, 2.5, 4.5] {
                    for obstruction_mm in [None, Some(34.0)] {
                        let focal_length_mm = focal_ratio * aperture_mm;
                        let obstruction_ratio = obstruction_mm.unwrap_or(0.0) / aperture_mm;
                        let optics = Optics {
                            focal_length_mm: Some(focal_length_mm),
                            aperture_mm: Some(aperture_mm),
                            obstruction_mm,
                            obstruction_ratio: Some(obstruction_ratio),
                            focal_ratio: None,
                            pixel_size_um: Some(pixel_um),
                            pixel_scale_arcsec_per_pixel: None,
                            microns_per_step: Some(microns_per_step),
                        };
                        let pixel_scale = 206.265 * pixel_um / focal_length_mm;
                        let truth = Hyperbola {
                            centre: f64::from(FOCUS),
                            minimum: 0.5 * seeing_arcsec / pixel_scale,
                            slope: predicted_slope_per_step(
                                blur_constant(obstruction_ratio),
                                microns_per_step,
                                focal_ratio,
                                pixel_um,
                            ),
                        };
                        let plan = plan_sweep(
                            "t",
                            &optics,
                            &TrainConfig::default(),
                            &SweepConfig::default(),
                            None,
                            Some(truth.minimum),
                        )
                        .unwrap();
                        let half_cfz = plan.cfz_steps.unwrap() / 2.0;
                        for offset in [-0.3, 0.0, 0.3] {
                            let centre =
                                FOCUS + (offset * f64::from(plan.half_width)).round() as i32;
                            let grid =
                                build_grid(centre, plan.step_size, plan.half_width, (None, None));
                            if grid.len() < params().min_fit_points {
                                continue;
                            }
                            let mut points: Vec<CurvePoint> = grid
                                .into_iter()
                                .map(|position| {
                                    let value = truth.value_at(f64::from(position));
                                    let measured =
                                        value * 0.05_f64.mul_add(2.0 * noise.unit() - 1.0, 1.0);
                                    let stars = ((300.0 * truth.minimum / value) as u32).max(1);
                                    point(position, Some(measured), stars)
                                })
                                .collect();
                            let stage = gate_and_fit(&mut points, params()).unwrap();
                            let miss = f64::from(stage.best_position - FOCUS).abs();
                            assert!(
                                miss <= half_cfz,
                                "f/{focal_ratio}, {microns_per_step} µm/step, {seeing_arcsec}\", \
                                 obstruction {obstruction_mm:?}, offset {offset}: focus placed \
                                 {miss} steps off, half a CFZ is {half_cfz}; {plan:?} {stage:?}"
                            );
                            fitted += 1;
                        }
                    }
                }
            }
        }
        assert!(fitted >= 250, "only {fitted} of 288 cases reached a fit");
    }

    /// A sweep of rig2's recorded frames as the star list measures them:
    /// `(position, hfr, listed stars measured)`, `None` where none was.
    /// Each was replayed through `rp`'s `measure_stars` with the list its
    /// centre frame gave, at `threshold_sigma` 10, `min_area` 40 and
    /// `max_area` 5000 — the settings the rig ran.
    fn recorded(samples: &[(i32, Option<f64>, u32)]) -> Vec<CurvePoint> {
        samples
            .iter()
            .map(|&(position, hfr, star_count)| point(position, hfr, star_count))
            .collect()
    }

    /// rig2's fine sweep of 2026-09-08: ±400 by 100 around 29666, 359
    /// stars listed.
    fn recorded_fine_sweep() -> Vec<CurvePoint> {
        recorded(&[
            (29_266, Some(20.861), 298),
            (29_366, Some(19.210), 316),
            (29_466, Some(14.584), 337),
            (29_566, Some(9.758), 340),
            (29_666, Some(4.728), 342),
            (29_766, Some(0.969), 339),
            (29_866, Some(4.234), 342),
            (29_966, Some(9.063), 339),
            (30_066, Some(13.934), 330),
        ])
    }

    /// rig2's coarse sweep of 2026-09-08: ±1200 by 300 around 29966, 152
    /// stars listed. The two frames 900 and 1200 steps above focus are
    /// beyond the size cap, and the one 1000 steps below keeps three
    /// stars.
    fn recorded_coarse_sweep() -> Vec<CurvePoint> {
        recorded(&[
            (28_766, Some(37.435), 3),
            (29_066, Some(33.421), 122),
            (29_366, Some(19.352), 144),
            (29_666, Some(4.945), 150),
            (29_966, Some(8.854), 151),
            (30_266, Some(23.405), 142),
            (30_566, Some(37.646), 102),
            (30_866, None, 0),
            (31_166, None, 0),
        ])
    }

    /// The 2026-09-26 sizing sweep: ±100 by 25 around 29737, six stars
    /// listed, `min_area` 40 leaving the compact ones out.
    fn recorded_sizing_sweep() -> Vec<CurvePoint> {
        recorded(&[
            (29_637, Some(4.887), 6),
            (29_662, Some(3.792), 6),
            (29_687, Some(2.787), 6),
            (29_712, Some(1.668), 6),
            (29_737, Some(1.027), 6),
            (29_762, Some(1.153), 6),
            (29_787, Some(1.822), 6),
            (29_812, Some(2.902), 6),
            (29_837, Some(4.051), 6),
        ])
    }

    /// The 2026-09-26 sized sweep's second attempt: ±80 by 20 around
    /// 29780, 35 stars listed.
    fn recorded_sized_sweep() -> Vec<CurvePoint> {
        recorded(&[
            (29_700, Some(1.736), 35),
            (29_720, Some(1.268), 35),
            (29_740, Some(0.972), 35),
            (29_760, Some(1.214), 35),
            (29_780, Some(1.837), 35),
            (29_800, Some(2.597), 35),
            (29_820, Some(3.505), 35),
            (29_840, Some(4.421), 35),
            (29_860, Some(5.327), 35),
        ])
    }

    #[test]
    fn the_recorded_fine_sweep_places_focus_beside_its_lowest_sample() {
        let stage = gate_and_fit(&mut recorded_fine_sweep(), params()).unwrap();
        assert_eq!(stage.best_position, 29_770, "{stage:?}");
        assert!(stage.r_squared > 0.98, "{stage:?}");
    }

    /// Measured on its listed stars, the coarse sweep keeps seven
    /// samples, the one of three stars among them, and places focus
    /// within three steps of the fine sweep's vertex.
    #[test]
    fn the_recorded_coarse_sweep_places_focus_beside_the_fine_sweeps() {
        let mut points = recorded_coarse_sweep();
        let stage = gate_and_fit(&mut points, params()).unwrap();
        assert_eq!(stage.samples_used, 7, "{stage:?}");
        assert_eq!(stage.best_position, 29_772, "{stage:?}");
        assert!(stage.r_squared > 0.99, "{stage:?}");
    }

    #[test]
    fn the_recorded_sized_sweeps_place_focus_where_the_replay_did() {
        let sizing = gate_and_fit(&mut recorded_sizing_sweep(), params()).unwrap();
        assert_eq!(sizing.best_position, 29_747, "{sizing:?}");
        let sized = gate_and_fit(&mut recorded_sized_sweep(), params()).unwrap();
        assert_eq!(sized.best_position, 29_739, "{sized:?}");
    }

    /// rig2's R sweep of 2026-11-05, first attempt: ±86 by 22 around
    /// 29770, 198 steps above R's focus at 29572, 114 stars listed.
    fn recorded_r_sweep_above_focus() -> Vec<CurvePoint> {
        recorded(&[
            (29_684, Some(5.427), 114),
            (29_706, Some(6.479), 114),
            (29_728, Some(7.432), 114),
            (29_750, Some(8.588), 114),
            (29_772, Some(9.658), 114),
            (29_794, Some(10.679), 114),
            (29_816, Some(11.735), 114),
            (29_838, Some(12.755), 114),
        ])
    }

    /// The same night's R sweep around 29371, 201 steps below focus, 134
    /// stars listed.
    fn recorded_r_sweep_below_focus() -> Vec<CurvePoint> {
        recorded(&[
            (29_285, Some(12.961), 134),
            (29_307, Some(11.877), 134),
            (29_329, Some(10.809), 134),
            (29_351, Some(9.762), 134),
            (29_373, Some(8.621), 134),
            (29_395, Some(7.692), 134),
            (29_417, Some(6.566), 134),
            (29_439, Some(5.633), 134),
        ])
    }

    /// The same night's R sweep around 29721, 149 steps above focus, 187
    /// stars listed.
    fn recorded_r_sweep_nearer_focus() -> Vec<CurvePoint> {
        recorded(&[
            (29_635, Some(3.167), 187),
            (29_657, Some(4.104), 187),
            (29_679, Some(5.248), 187),
            (29_701, Some(6.237), 187),
            (29_723, Some(7.271), 187),
            (29_745, Some(8.381), 187),
            (29_767, Some(9.515), 187),
            (29_789, Some(10.636), 187),
        ])
    }

    /// Each sweep walked one wing and fitted a vertex outside its
    /// samples, where the rig's own run put it; the retry centres there,
    /// and the grid it walks holds R's focus, 29572, which a shift of
    /// half a width leaves outside the second grids of the two sweeps
    /// started 200 steps off.
    #[test]
    fn rig2s_sweeps_started_off_focus_centre_their_retry_on_a_grid_holding_focus() {
        const FOCUS: i32 = 29_572;
        let r = SweepParams {
            step_size: 22,
            half_width: 86,
            ..params()
        };
        for (mut points, centre, vertex) in [
            (recorded_r_sweep_above_focus(), 29_770, 29_572),
            (recorded_r_sweep_below_focus(), 29_371, 29_536),
            (recorded_r_sweep_nearer_focus(), 29_721, 29_584),
        ] {
            let error = gate_and_fit(&mut points, r).unwrap_err();
            let FitError::VertexOutside { vertex: fitted, .. } = error else {
                panic!("the sweep around {centre} failed as {error:?}");
            };
            assert_eq!(fitted, vertex, "the sweep around {centre}");
            let plan = retry_centre(centre, &error, &points, r.half_width, r.bounds());
            assert_eq!(
                plan,
                RetryPlan {
                    retry: Retry::Shift,
                    centre: vertex
                },
                "the sweep around {centre}"
            );
            let (low, high) = grid_span(&build_grid(
                plan.centre,
                r.step_size,
                r.half_width,
                r.bounds(),
            ));
            assert!(
                low < Some(FOCUS) && Some(FOCUS) < high,
                "the retry of the sweep around {centre} walks {low:?}..{high:?}"
            );
        }
    }

    /// The sizing sweep's confirmation frame at 29744 detects two stars
    /// at `min_area` 40, 1.66 px between them; its six listed stars
    /// measure 0.927 px against the lowest sample's 1.027, and it is
    /// accepted.
    #[test]
    fn the_recorded_sizing_confirmation_is_accepted_on_its_listed_stars() {
        let sizing = gate_and_fit(&mut recorded_sizing_sweep(), params()).unwrap();
        assert_eq!(sizing.lowest_hfr, 1.027);
        assert_eq!(
            confirmation_verdict(Some(0.927), 6, sizing.lowest_hfr, 0.25),
            None
        );
        assert_eq!(
            confirmation_verdict(Some(1.664), 2, 1.05, 0.25),
            Some(ConfirmationRejection::Worse),
            "the two stars detection finds there read worse than the lowest detected sample"
        );
    }
}
