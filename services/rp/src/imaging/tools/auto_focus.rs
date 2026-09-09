//! `auto_focus`: V-curve focus sweep compound tool.
//!
//! The driving logic — sweep grid construction, the move/capture/measure
//! loop, the sparse-sample gate, the parabolic least-squares fit with
//! vertex-in-range validation, and the confirmation frame at the
//! fitted position — is pure Rust and fully unit-testable via the
//! [`FocuserOps`], [`CaptureOps`], [`MeasureOps`] traits. The MCP
//! wrapper in `mcp.rs` provides concrete adapters that bind to the
//! real Alpaca focuser / camera and the image cache; tests substitute
//! synthetic adapters that drive the loop with deterministic
//! per-position HFR data.
//!
//! Behavioral contract: `docs/services/rp.md` → Compound Tools →
//! `auto_focus` Contract.

use async_trait::async_trait;
use serde::Serialize;
use std::cmp::Ordering;
use std::time::Duration;
use thiserror::Error;
use tracing::debug;

/// Default for [`AutoFocusParams::min_star_fraction`].
///
/// A sample with fewer than a tenth of the sweep's densest frame's
/// stars is not a measurement of the star field, it is whatever
/// fragments survived the area filter.
pub const DEFAULT_MIN_STAR_FRACTION: f64 = 0.1;

/// Default for [`AutoFocusParams::confirmation_tolerance`].
///
/// Wide enough for seeing jitter and the parabola's vertex bias,
/// narrow enough that a fit landing off the measured minimum (about
/// twice the best sample) is rejected.
pub const DEFAULT_CONFIRMATION_TOLERANCE: f64 = 0.25;

#[derive(Debug, Clone)]
pub struct AutoFocusParams {
    pub duration: Duration,
    pub step_size: i32,
    pub half_width: i32,
    pub min_area: usize,
    pub max_area: usize,
    pub threshold_sigma: f64,
    pub min_fit_points: usize,
    /// The order the grid is walked in. Follows the focuser's backlash
    /// `approach` so every sample is reached from the same side as the
    /// final move (rp.md § Focuser Tool Details); ascending otherwise.
    pub direction: SweepDirection,
    /// Sparse-sample gate: a sweep sample whose `star_count` is below
    /// this fraction of the sweep's largest `star_count` is rejected
    /// before the fit. Finite, in `[0, 1)`; `0.0` disables the gate.
    pub min_star_fraction: f64,
    /// How much worse (as a fraction) than the lowest accepted sweep
    /// sample the confirmation frame may measure before the fitted
    /// position is rejected in favour of that sample's position.
    /// Finite, `≥ 0`.
    pub confirmation_tolerance: f64,
}

/// Walk order of the sweep grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SweepDirection {
    /// Smallest position first — the default, and the order for a
    /// focuser whose backlash `approach` is `out`.
    #[default]
    Ascending,
    /// Largest position first — the order for a focuser whose backlash
    /// `approach` is `in`.
    Descending,
}

/// Why a sweep sample was excluded from the fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    /// The sample's `star_count` fell below the sparse gate.
    Sparse,
}

#[derive(Debug, Clone, Serialize)]
pub struct CurvePoint {
    pub position: i32,
    pub hfr: Option<f64>,
    pub star_count: u32,
    pub document_id: String,
    /// `None` for a sample that entered the fit (or a starless one,
    /// which never could); `Some` names why an otherwise measurable
    /// sample was left out.
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

/// The frame captured at the fitted position after the final move,
/// and whether it vouched for the fit.
#[derive(Debug, Clone, Serialize)]
pub struct Confirmation {
    pub document_id: String,
    pub hfr: Option<f64>,
    pub star_count: u32,
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AutoFocusResult {
    /// The fitted vertex.
    pub best_position: i32,
    /// The fitted HFR at the vertex — a fit value, not a measurement.
    pub best_hfr: f64,
    /// Weighted coefficient of determination of the parabola over the
    /// accepted samples.
    pub fit_r_squared: f64,
    pub confirmation: Confirmation,
    /// Where the focuser was left: `best_position` when the
    /// confirmation was accepted, the lowest accepted sweep sample's
    /// position otherwise.
    pub final_position: i32,
    /// The HFR measured at `final_position`.
    pub final_hfr: f64,
    /// Accepted samples that entered the fit.
    pub samples_used: usize,
    pub curve_points: Vec<CurvePoint>,
    pub temperature_c: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct HfrSample {
    pub hfr: Option<f64>,
    pub star_count: u32,
}

#[derive(Debug, Error)]
pub enum AutoFocusError {
    #[error("step_size must be positive (got {0})")]
    InvalidStepSize(i32),
    #[error("half_width must be positive (got {0})")]
    InvalidHalfWidth(i32),
    #[error("min_fit_points must be at least 3 (got {0})")]
    InvalidMinFitPoints(usize),
    #[error("min_star_fraction must be a finite number in [0, 1) (got {0})")]
    InvalidMinStarFraction(f64),
    #[error("confirmation_tolerance must be a finite number of at least 0 (got {0})")]
    InvalidConfirmationTolerance(f64),
    #[error(
        "sweep grid has {available} positions after clamping to focuser bounds; \
         min_fit_points={requested}"
    )]
    GridTooSmall { available: usize, requested: usize },
    #[error(
        "sweep grid would contain {requested} positions, exceeds the safety cap of \
         {max} (raise step_size or lower half_width)"
    )]
    GridTooLarge { requested: usize, max: usize },
    #[error(
        "not enough stars: only {got} of {needed} required samples are accepted \
         (non-null HFR, past the sparse gate)"
    )]
    NotEnoughStars { got: usize, needed: usize },
    #[error("monotonic curve: {0}")]
    MonotonicCurve(String),
    #[error("equipment error during sweep: {0}")]
    Equipment(String),
}

#[async_trait]
pub trait FocuserOps {
    async fn move_to(&self, position: i32) -> Result<i32, String>;
}

pub use super::ops::CaptureOps;

#[async_trait]
pub trait MeasureOps {
    async fn measure(
        &self,
        document_id: &str,
        min_area: usize,
        max_area: usize,
        threshold_sigma: f64,
    ) -> Result<HfrSample, String>;
}

/// Maximum number of grid points an `auto_focus` sweep is allowed to
/// build.
///
/// Generous enough that any plausible auto-focus run fits well
/// inside the cap (typical 10–30 points; even an aggressively-fine
/// sweep over a wide range stays under 200), so the cap is purely a
/// guardrail against operator misconfiguration that would otherwise
/// produce thousands of captures and tie up the rig for hours.
pub const MAX_GRID_POINTS: usize = 1000;

/// Reject sweep parameters that cannot produce a usable grid or a
/// meaningful gate.
///
/// # Errors
///
/// Returns the `Invalid*` variant naming a non-positive `step_size` or
/// `half_width`, a `min_fit_points` below 3, a `min_star_fraction`
/// outside `[0, 1)`, or a negative `confirmation_tolerance` (either
/// fraction non-finite counts as out of range), or
/// [`AutoFocusError::GridTooLarge`] if the unclamped grid would exceed
/// [`MAX_GRID_POINTS`].
pub fn validate_params(params: &AutoFocusParams) -> Result<(), AutoFocusError> {
    if params.step_size <= 0 {
        return Err(AutoFocusError::InvalidStepSize(params.step_size));
    }
    if params.half_width <= 0 {
        return Err(AutoFocusError::InvalidHalfWidth(params.half_width));
    }
    if params.min_fit_points < 3 {
        return Err(AutoFocusError::InvalidMinFitPoints(params.min_fit_points));
    }
    if !valid_min_star_fraction(params.min_star_fraction) {
        return Err(AutoFocusError::InvalidMinStarFraction(
            params.min_star_fraction,
        ));
    }
    if !valid_confirmation_tolerance(params.confirmation_tolerance) {
        return Err(AutoFocusError::InvalidConfirmationTolerance(
            params.confirmation_tolerance,
        ));
    }
    // Upper bound on the unclamped grid size: 2·half_width steps from
    // start to end, plus the start point itself. Computed in i64 so
    // an extreme half_width can't overflow before the cap check fires.
    // `step_size > 0` was validated above, so the division cannot trap;
    // the fold to `i64::MAX` on the impossible zero lands in the
    // GridTooLarge error either way.
    let estimated = i64::from(params.half_width)
        .saturating_mul(2)
        .checked_div(i64::from(params.step_size))
        .unwrap_or(i64::MAX)
        .saturating_add(1);
    if estimated > i64::try_from(MAX_GRID_POINTS).unwrap_or(i64::MAX) {
        return Err(AutoFocusError::GridTooLarge {
            requested: usize::try_from(estimated).unwrap_or(usize::MAX),
            max: MAX_GRID_POINTS,
        });
    }
    Ok(())
}

/// Whether `value` is a usable sparse-gate fraction: finite and in
/// `[0, 1)`. Shared by the per-call and config-block validations.
#[must_use]
pub fn valid_min_star_fraction(value: f64) -> bool {
    value.is_finite() && (0.0..1.0).contains(&value)
}

/// Whether `value` is a usable confirmation tolerance: finite and
/// non-negative. Shared by the per-call and config-block validations.
#[must_use]
pub fn valid_confirmation_tolerance(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

/// The sparse gate's threshold for a sweep whose densest frame counted
/// `max_stars` stars: a sample with fewer stars than this is rejected.
#[must_use]
pub fn sparse_threshold(max_stars: u32, min_star_fraction: f64) -> f64 {
    min_star_fraction * f64::from(max_stars)
}

/// Mark every measurable sample whose `star_count` falls below
/// `min_star_fraction` of the sweep's largest count as
/// [`Rejection::Sparse`].
///
/// Starless samples (`hfr: None`) are left alone — they never enter
/// the fit anyway — and do not set the maximum. Returns the threshold
/// applied, which the confirmation frame is held to as well.
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

/// The confirmation verdict on the frame measured at the fitted
/// position.
///
/// Accepted when the frame has stars, passes the sweep's sparse gate,
/// and its HFR is no worse than `lowest_hfr` (the best accepted sweep
/// sample) by more than `tolerance`.
#[must_use]
pub fn confirmation_accepted(
    sample: &HfrSample,
    gate_threshold: f64,
    lowest_hfr: f64,
    tolerance: f64,
) -> bool {
    sample.hfr.is_some_and(|hfr| {
        f64::from(sample.star_count) >= gate_threshold && hfr <= lowest_hfr * (1.0 + tolerance)
    })
}

/// The lowest-HFR sample; ties go to the denser frame so the choice
/// is deterministic and favours the better measurement.
#[must_use]
pub fn lowest_sample(samples: &[(i32, f64, u32)]) -> Option<(i32, f64, u32)> {
    samples.iter().copied().min_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.2.cmp(&a.2))
    })
}

/// Build the sweep grid `[start, start+step, …]` continuing while the
/// position stays `≤ end`, then clamped to `[min_bound, max_bound]`.
///
/// `start` is `current − half_width` and `end` is
/// `current + half_width`, so `end` only appears as a grid point when
/// `(end − start)` is an exact multiple of `step`; otherwise the
/// last grid point is the largest `start + k·step` that's still
/// `≤ end`. Out-of-range points (those failing the `[min_bound,
/// max_bound]` clamp) are dropped, not coerced — coercion would
/// produce duplicate samples at a bound and distort the parabola fit.
#[must_use]
pub fn build_grid(
    current: i32,
    step: i32,
    half_width: i32,
    bounds: (Option<i32>, Option<i32>),
) -> Vec<i32> {
    let start = current.saturating_sub(half_width);
    let end = current.saturating_add(half_width);
    let mut grid = Vec::new();
    let mut p = start;
    loop {
        let in_min = bounds.0.is_none_or(|min| p >= min);
        let in_max = bounds.1.is_none_or(|max| p <= max);
        if in_min && in_max {
            grid.push(p);
        }
        let next = p.saturating_add(step);
        if p == end || next <= p {
            break;
        }
        if next > end {
            break;
        }
        p = next;
    }
    grid
}

/// Result of fitting `hfr = a·x'² + b·x' + c` where `x' = x − offset_x`.
///
/// The fit is performed in the recentered frame so the normal-equations
/// determinant doesn't lose precision at real-world focuser positions
/// (~`5_000–50_000` steps); see `fit_parabola` for the rationale. The
/// vertex sits at `(round(−b/2a + offset_x), c − b²/(4a))` in the
/// original frame — `vertex_position` rounds the recovered original-x
/// after adding `offset_x` (which is generally non-integer because it's
/// the weighted mean of the sample positions).
#[derive(Debug, Clone, Copy)]
pub struct ParabolaFit {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub offset_x: f64,
    /// Weighted coefficient of determination over the fitted samples,
    /// clamped to `[0, 1]`: how much of the samples' spread the
    /// parabola explains.
    pub r_squared: f64,
}

impl ParabolaFit {
    #[must_use]
    pub fn vertex_position(&self) -> i32 {
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            reason = "`f64` to `i32` has no total spelling; `as` saturates at the i32 rails and maps NaN to 0, and the caller's grid-range check rejects rail-hitting vertices (and 0, unless the sampled grid straddles it — no in-tree path produces a NaN fit)"
        )]
        let vertex = (-self.b / (2.0 * self.a) + self.offset_x).round() as i32;
        vertex
    }

    #[must_use]
    pub fn vertex_value(&self) -> f64 {
        self.c - (self.b * self.b) / (4.0 * self.a)
    }
}

/// Determinant of a 3×3 matrix given by rows (first-row cofactor
/// expansion).
const fn det3(r0: [f64; 3], r1: [f64; 3], r2: [f64; 3]) -> f64 {
    r0[0] * (r1[1] * r2[2] - r1[2] * r2[1]) - r0[1] * (r1[0] * r2[2] - r1[2] * r2[0])
        + r0[2] * (r1[0] * r2[1] - r1[1] * r2[0])
}

/// Weighted least-squares fit of a parabola to `(position, hfr, weight)`
/// samples. `weight` is typically the per-frame `star_count`; samples
/// with `weight == 0` are dropped.
///
/// **Returned coefficients are in the recentered frame.** The fit
/// solves `y = a·x'² + b·x' + c` where `x' = position − offset_x`;
/// `offset_x` is the weighted mean of the input positions and is
/// stored on `ParabolaFit`. Use `vertex_position()` and
/// `vertex_value()` to recover the original-frame minimum without
/// re-expanding the polynomial (re-expanding would re-introduce the
/// precision loss this recentering exists to avoid). Callers that
/// genuinely need to evaluate `y` at an original-frame `x` must
/// subtract `offset_x` before substituting.
///
/// # Errors
///
/// Returns [`AutoFocusError::NotEnoughStars`] if fewer than 3 samples
/// carry a non-zero weight, or [`AutoFocusError::MonotonicCurve`] if
/// `a ≤ 0` (the curve has no minimum) or the design matrix is too
/// ill-conditioned to invert (essentially flat input, where the vertex
/// is undefined).
pub fn fit_parabola(samples: &[(i32, f64, u32)]) -> Result<ParabolaFit, AutoFocusError> {
    let filtered: Vec<(f64, f64, f64)> = samples
        .iter()
        .filter(|(_, _, w)| *w > 0)
        .map(|(x, y, w)| (f64::from(*x), *y, f64::from(*w)))
        .collect();
    if filtered.len() < 3 {
        return Err(AutoFocusError::NotEnoughStars {
            got: filtered.len(),
            needed: 3,
        });
    }
    // Recenter x by the weighted mean before forming the normal
    // equations. Without this, `m4 ~ N·p⁴` overflows f64's working
    // precision (~16 digits) at real-world focuser positions
    // (p ≳ 5_000 steps): the m4·m2·m0 product hits ~1e30, the actual
    // determinant of a parabolic fit is much smaller, and roundoff
    // swamps the cancellation — perfectly fittable V-curves get
    // rejected as "design matrix is singular". The fit is
    // translation-invariant in y, so we recover the original-frame
    // vertex by adding `offset_x` back in `vertex_position`.
    let total_w: f64 = filtered.iter().map(|(_, _, w)| *w).sum();
    let offset_x: f64 = filtered.iter().map(|(x, _, w)| w * x).sum::<f64>() / total_w;

    // Normal equations Aᵀ W A · [a, b, c]ᵀ = Aᵀ W y, with
    // A = [x'², x', 1] per row (x' = x − offset_x), W = diag(weights).
    // Solved via Cramer's rule on the 3×3 system below.
    let mut m4 = 0.0;
    let mut m3 = 0.0;
    let mut m2 = 0.0;
    let mut m1 = 0.0;
    let mut m0 = 0.0;
    let mut t2 = 0.0;
    let mut t1 = 0.0;
    let mut t0 = 0.0;
    for (x, y, w) in &filtered {
        let xc = x - offset_x;
        let xc2 = xc * xc;
        let xc3 = xc2 * xc;
        let xc4 = xc3 * xc;
        m4 += w * xc4;
        m3 += w * xc3;
        m2 += w * xc2;
        m1 += w * xc;
        m0 += w;
        t2 += w * xc2 * y;
        t1 += w * xc * y;
        t0 += w * y;
    }
    // Cramer's rule on the symmetric system M·[a, b, c]ᵀ = t, with
    // M = [[m4, m3, m2], [m3, m2, m1], [m2, m1, m0]] and t = [t2, t1, t0];
    // each det_* replaces one column of M with t.
    let det = det3([m4, m3, m2], [m3, m2, m1], [m2, m1, m0]);
    // Scale-invariant ill-conditioning check: the normal-equation
    // determinant is roughly O(m4·m2·m0) for the problem; require the
    // actual det to be at least ~1e-12 of that ceiling. Below this, the
    // input is effectively flat and the vertex is meaningless.
    let det_scale = (m4.abs() * m2.abs() * m0.abs()).max(1.0);
    if det.abs() < det_scale * 1e-12 {
        return Err(AutoFocusError::MonotonicCurve(format!(
            "design matrix is singular (det={det:.3e}, scale={det_scale:.3e})"
        )));
    }
    let det_a = det3([t2, m3, m2], [t1, m2, m1], [t0, m1, m0]);
    let det_b = det3([m4, t2, m2], [m3, t1, m1], [m2, t0, m0]);
    let det_c = det3([m4, m3, t2], [m3, m2, t1], [m2, m1, t0]);
    let a = det_a / det;
    let b = det_b / det;
    let c = det_c / det;
    if a <= 0.0 {
        return Err(AutoFocusError::MonotonicCurve(format!(
            "non-positive leading coefficient (a={a:.3e})"
        )));
    }
    // Weighted R²: residual spread around the fit against the spread
    // around the weighted mean. `m0 > 0` because every retained sample
    // carries a positive weight.
    let y_mean = t0 / m0;
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
    /// The sparse gate's threshold, applied to the confirmation frame too.
    gate_threshold: f64,
    /// The lowest accepted sweep sample — the fallback position and
    /// the bar the confirmation frame is held to.
    lowest_position: i32,
    lowest_hfr: f64,
    samples_used: usize,
}

/// Gate the sweep, fit the accepted samples, and validate the vertex
/// against the grid. Pure: the caller decides what to do with the
/// focuser on failure.
fn gate_and_fit(
    curve_points: &mut [CurvePoint],
    grid: &[i32],
    params: &AutoFocusParams,
) -> Result<FitStage, AutoFocusError> {
    let gate_threshold = apply_sparse_gate(curve_points, params.min_star_fraction);
    let accepted: Vec<(i32, f64, u32)> = curve_points
        .iter()
        .filter_map(CurvePoint::accepted_sample)
        .collect();
    let rejected_sparse = curve_points.iter().filter(|p| p.rejected.is_some()).count();
    debug!(
        gate_threshold,
        accepted = accepted.len(),
        rejected_sparse,
        "auto_focus sparse gate applied"
    );
    if accepted.len() < params.min_fit_points {
        return Err(AutoFocusError::NotEnoughStars {
            got: accepted.len(),
            needed: params.min_fit_points,
        });
    }

    let fit = fit_parabola(&accepted)?;
    let best_position = fit.vertex_position();
    // `accepted` was filtered from `grid`, and we just returned above
    // unless `accepted.len() >= min_fit_points >= 3`, so `grid` is
    // non-empty here. Pattern-match the `Option`s instead of panicking
    // to satisfy the workspace's no-panic policy.
    let (Some(&grid_min), Some(&grid_max)) = (grid.iter().min(), grid.iter().max()) else {
        return Err(AutoFocusError::MonotonicCurve(
            "grid is empty despite having accepted samples".into(),
        ));
    };
    if best_position < grid_min || best_position > grid_max {
        return Err(AutoFocusError::MonotonicCurve(format!(
            "fitted vertex {best_position} is outside sampled grid [{grid_min}, {grid_max}]"
        )));
    }
    let Some((lowest_position, lowest_hfr, _)) = lowest_sample(&accepted) else {
        return Err(AutoFocusError::MonotonicCurve(
            "no accepted sample to hold the confirmation against".into(),
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

/// Best-effort return to the starting position after a fit failure.
/// A failed restore is logged, never surfaced — the fit error is the
/// one the caller needs.
async fn restore_start<F: FocuserOps + Sync>(focuser: &F, starting_position: i32) {
    if let Err(e) = focuser.move_to(starting_position).await {
        debug!(
            error = %e,
            starting_position,
            "auto_focus could not restore the starting position after a failed fit"
        );
    }
}

/// Drive the V-curve sweep against the supplied focuser/capturer/measurer
/// adapters.
///
/// See `docs/services/rp.md` → `auto_focus` Contract for the
/// behavioral spec; this function is the reference implementation.
///
/// `starting_position` and `starting_temperature_c` must be the values the
/// caller already read from the focuser for the `focus_started` event.
/// The contract guarantees a single read of each — passing them in keeps
/// the event payload and the result strictly consistent and avoids extra
/// Alpaca round-trips inside the loop.
///
/// # Errors
///
/// Returns [`validate_params`]'s rejection, [`AutoFocusError::GridTooSmall`]
/// if the bounds-clamped grid holds fewer than `min_fit_points` positions,
/// [`AutoFocusError::Equipment`] if any focuser move, capture, or
/// measurement fails (the focuser stays where it is),
/// [`AutoFocusError::NotEnoughStars`] if fewer than `min_fit_points`
/// positions yielded an accepted sample, or
/// [`AutoFocusError::MonotonicCurve`] if the fit fails or its vertex
/// falls outside the sampled grid — for those last two the focuser is
/// first moved back to `starting_position`, best effort.
pub async fn run_auto_focus<F: FocuserOps + Sync, C: CaptureOps + Sync, M: MeasureOps + Sync>(
    focuser: &F,
    capturer: &C,
    measurer: &M,
    bounds: (Option<i32>, Option<i32>),
    starting_position: i32,
    starting_temperature_c: Option<f64>,
    params: AutoFocusParams,
) -> Result<AutoFocusResult, AutoFocusError> {
    validate_params(&params)?;

    let mut grid = build_grid(
        starting_position,
        params.step_size,
        params.half_width,
        bounds,
    );
    if grid.len() < params.min_fit_points {
        return Err(AutoFocusError::GridTooSmall {
            available: grid.len(),
            requested: params.min_fit_points,
        });
    }
    // `build_grid` yields ascending positions; the walk order follows
    // the focuser's backlash approach so every sample is reached from
    // the same side as the final move.
    if params.direction == SweepDirection::Descending {
        grid.reverse();
    }
    let grid = grid;

    let temperature_c = starting_temperature_c;
    debug!(
        current_position = starting_position,
        grid_len = grid.len(),
        direction = ?params.direction,
        temperature_c = ?temperature_c,
        "auto_focus sweep starting"
    );

    let mut curve_points = Vec::with_capacity(grid.len());
    for position in &grid {
        focuser
            .move_to(*position)
            .await
            .map_err(AutoFocusError::Equipment)?;
        let document_id = capturer
            .capture(params.duration)
            .await
            .map_err(AutoFocusError::Equipment)?;
        let sample = measurer
            .measure(
                &document_id,
                params.min_area,
                params.max_area,
                params.threshold_sigma,
            )
            .await
            .map_err(AutoFocusError::Equipment)?;
        curve_points.push(CurvePoint {
            position: *position,
            hfr: sample.hfr,
            star_count: sample.star_count,
            document_id,
            rejected: None,
        });
    }

    let stage = match gate_and_fit(&mut curve_points, &grid, &params) {
        Ok(stage) => stage,
        Err(e) => {
            restore_start(focuser, starting_position).await;
            return Err(e);
        }
    };

    let settled = confirm_and_settle(focuser, capturer, measurer, &params, stage).await?;

    Ok(AutoFocusResult {
        best_position: stage.best_position,
        best_hfr: stage.best_hfr,
        fit_r_squared: stage.r_squared,
        confirmation: settled.confirmation,
        final_position: settled.final_position,
        final_hfr: settled.final_hfr,
        samples_used: stage.samples_used,
        curve_points,
        temperature_c,
    })
}

/// Where a run ended and what vouched for it.
struct Settled {
    confirmation: Confirmation,
    final_position: i32,
    final_hfr: f64,
}

/// Move to the fitted vertex, measure a confirmation frame there, and
/// settle on the better of the two measured positions: the vertex when
/// the confirmation is accepted, the lowest accepted sweep sample
/// otherwise.
async fn confirm_and_settle<F: FocuserOps + Sync, C: CaptureOps + Sync, M: MeasureOps + Sync>(
    focuser: &F,
    capturer: &C,
    measurer: &M,
    params: &AutoFocusParams,
    stage: FitStage,
) -> Result<Settled, AutoFocusError> {
    let moved_to = focuser
        .move_to(stage.best_position)
        .await
        .map_err(AutoFocusError::Equipment)?;
    let document_id = capturer
        .capture(params.duration)
        .await
        .map_err(AutoFocusError::Equipment)?;
    let sample = measurer
        .measure(
            &document_id,
            params.min_area,
            params.max_area,
            params.threshold_sigma,
        )
        .await
        .map_err(AutoFocusError::Equipment)?;
    let accepted = confirmation_accepted(
        &sample,
        stage.gate_threshold,
        stage.lowest_hfr,
        params.confirmation_tolerance,
    );
    debug!(
        best_position = stage.best_position,
        confirmation_hfr = ?sample.hfr,
        confirmation_stars = sample.star_count,
        lowest_position = stage.lowest_position,
        lowest_hfr = stage.lowest_hfr,
        accepted,
        "auto_focus confirmation frame measured"
    );
    // Two positions are measured now; the better one wins. The
    // pattern pairs the verdict with the HFR so a rejected verdict
    // never reads a missing one.
    let (final_position, final_hfr) = if let (true, Some(hfr)) = (accepted, sample.hfr) {
        (moved_to, hfr)
    } else {
        let position = focuser
            .move_to(stage.lowest_position)
            .await
            .map_err(AutoFocusError::Equipment)?;
        (position, stage.lowest_hfr)
    };
    Ok(Settled {
        confirmation: Confirmation {
            document_id,
            hfr: sample.hfr,
            star_count: sample.star_count,
            accepted,
        },
        final_position,
        final_hfr,
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ---- pure helpers ----

    #[test]
    fn validate_params_accepts_minimum_valid_input() {
        let p = AutoFocusParams {
            duration: Duration::from_millis(100),
            step_size: 1,
            half_width: 1,
            min_area: 1,
            max_area: 1,
            threshold_sigma: 5.0,
            min_fit_points: 3,
            min_star_fraction: DEFAULT_MIN_STAR_FRACTION,
            confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
            direction: SweepDirection::Ascending,
        };
        validate_params(&p).unwrap();
    }

    #[test]
    fn validate_params_rejects_zero_step_size() {
        let p = AutoFocusParams {
            duration: Duration::from_millis(100),
            step_size: 0,
            half_width: 100,
            min_area: 5,
            max_area: 1000,
            threshold_sigma: 5.0,
            min_fit_points: 5,
            min_star_fraction: DEFAULT_MIN_STAR_FRACTION,
            confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
            direction: SweepDirection::Ascending,
        };
        assert!(matches!(
            validate_params(&p),
            Err(AutoFocusError::InvalidStepSize(0))
        ));
    }

    #[test]
    fn validate_params_rejects_negative_half_width() {
        let p = AutoFocusParams {
            duration: Duration::from_millis(100),
            step_size: 50,
            half_width: -1,
            min_area: 5,
            max_area: 1000,
            threshold_sigma: 5.0,
            min_fit_points: 5,
            min_star_fraction: DEFAULT_MIN_STAR_FRACTION,
            confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
            direction: SweepDirection::Ascending,
        };
        assert!(matches!(
            validate_params(&p),
            Err(AutoFocusError::InvalidHalfWidth(-1))
        ));
    }

    #[test]
    fn validate_params_rejects_min_fit_points_below_3() {
        let p = AutoFocusParams {
            duration: Duration::from_millis(100),
            step_size: 50,
            half_width: 100,
            min_area: 5,
            max_area: 1000,
            threshold_sigma: 5.0,
            min_fit_points: 2,
            min_star_fraction: DEFAULT_MIN_STAR_FRACTION,
            confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
            direction: SweepDirection::Ascending,
        };
        assert!(matches!(
            validate_params(&p),
            Err(AutoFocusError::InvalidMinFitPoints(2))
        ));
    }

    #[test]
    fn validate_params_rejects_grid_size_above_safety_cap() {
        // step_size=1, half_width=1_000_000 → 2_000_001 points,
        // far beyond the 1000-point safety cap.
        let p = AutoFocusParams {
            duration: Duration::from_millis(100),
            step_size: 1,
            half_width: 1_000_000,
            min_area: 5,
            max_area: 1000,
            threshold_sigma: 5.0,
            min_fit_points: 5,
            min_star_fraction: DEFAULT_MIN_STAR_FRACTION,
            confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
            direction: SweepDirection::Ascending,
        };
        match validate_params(&p) {
            Err(AutoFocusError::GridTooLarge { requested, max }) => {
                assert!(requested > max, "expected requested > max");
                assert_eq!(max, MAX_GRID_POINTS);
            }
            other => panic!("expected GridTooLarge, got {other:?}"),
        }
    }

    // ---- grid construction ----

    #[test]
    fn build_grid_unbounded_is_symmetric() {
        let g = build_grid(1000, 100, 200, (None, None));
        assert_eq!(g, vec![800, 900, 1000, 1100, 1200]);
    }

    #[test]
    fn build_grid_clamps_below_min_position() {
        let g = build_grid(5000, 100, 500, (Some(4900), None));
        assert_eq!(g, vec![4900, 5000, 5100, 5200, 5300, 5400, 5500]);
    }

    #[test]
    fn build_grid_clamps_above_max_position() {
        let g = build_grid(5000, 100, 500, (None, Some(5100)));
        assert_eq!(g, vec![4500, 4600, 4700, 4800, 4900, 5000, 5100]);
    }

    #[test]
    fn build_grid_clamps_both_sides() {
        let g = build_grid(5000, 100, 500, (Some(4900), Some(5100)));
        assert_eq!(g, vec![4900, 5000, 5100]);
    }

    #[test]
    fn build_grid_step_larger_than_half_width_yields_singleton() {
        let g = build_grid(1000, 1000, 100, (None, None));
        assert_eq!(g, vec![900]);
    }

    // ---- parabola fit ----

    fn make_v_samples(vertex_x: i32, vertex_y: f64, curvature: f64) -> Vec<(i32, f64, u32)> {
        (vertex_x - 200..=vertex_x + 200)
            .step_by(50)
            .map(|x| {
                let dx = f64::from(x - vertex_x);
                let y = curvature * dx * dx + vertex_y;
                (x, y, 100)
            })
            .collect()
    }

    #[test]
    fn fit_parabola_recovers_known_minimum_to_within_one_step() {
        // Cover the small-position case plus realistic focuser ranges
        // (5_000–50_000 steps) where the un-recentered normal equations
        // would lose precision and reject the fit as singular (#174).
        for vertex_x in [1234, 11_000, 42_000] {
            let samples = make_v_samples(vertex_x, 1.5, 1e-4);
            let fit = fit_parabola(&samples).unwrap();
            let vx = fit.vertex_position();
            assert!(
                (vx - vertex_x).abs() <= 1,
                "expected vertex_position within ±1 of {vertex_x}, got {vx}"
            );
            let vy = fit.vertex_value();
            assert!(
                (vy - 1.5).abs() < 1e-6,
                "expected vertex_value ≈ 1.5 at vertex_x={vertex_x}, got {vy}"
            );
        }
    }

    #[test]
    fn fit_parabola_rejects_flat_input() {
        // Realistic focuser-position range — the rejection must
        // still trigger at the scale where un-recentered moments
        // would have overflowed f64 precision.
        let samples: Vec<_> = (0..10).map(|i| (40_000 + i * 100, 5.0, 100)).collect();
        match fit_parabola(&samples) {
            Err(AutoFocusError::MonotonicCurve(_)) => {}
            other => panic!("expected MonotonicCurve, got {other:?}"),
        }
    }

    #[test]
    fn fit_parabola_rejects_concave_down_curve() {
        let samples: Vec<_> = (0..10)
            .map(|i| {
                let x = 40_000 + i * 50;
                let dx = f64::from(x - 40_100);
                (x, 5.0 - 1e-4 * dx * dx, 100)
            })
            .collect();
        match fit_parabola(&samples) {
            Err(AutoFocusError::MonotonicCurve(msg)) => {
                assert!(msg.contains("non-positive"), "got msg: {msg}");
            }
            other => panic!("expected MonotonicCurve, got {other:?}"),
        }
    }

    #[test]
    fn fit_parabola_rejects_too_few_samples() {
        let samples = vec![(0, 1.0, 100), (10, 2.0, 100)];
        match fit_parabola(&samples) {
            Err(AutoFocusError::NotEnoughStars { got: 2, needed: 3 }) => {}
            other => panic!("expected NotEnoughStars, got {other:?}"),
        }
    }

    #[test]
    fn fit_parabola_drops_zero_weight_samples() {
        let mut samples = make_v_samples(1000, 2.0, 1e-4);
        samples.push((100_000, 999.0, 0));
        samples.push((-100_000, 999.0, 0));
        let fit = fit_parabola(&samples).unwrap();
        assert!((fit.vertex_position() - 1000).abs() <= 1);
    }

    // ---- end-to-end run_auto_focus over synthetic adapters ----

    struct StubFocuser {
        position: Mutex<i32>,
    }

    #[async_trait]
    impl FocuserOps for StubFocuser {
        async fn move_to(&self, position: i32) -> Result<i32, String> {
            *self.position.lock().unwrap() = position;
            Ok(position)
        }
    }

    /// The capturer reads the focuser's current position and stamps it
    /// into the synthetic `document_id`, so the measurer can recover
    /// per-position HFR values without any shared state beyond the id.
    struct StubCapturer<'a> {
        focuser: &'a StubFocuser,
        counter: Mutex<u64>,
    }

    #[async_trait]
    impl CaptureOps for StubCapturer<'_> {
        async fn capture(&self, _duration: Duration) -> Result<String, String> {
            let pos = *self.focuser.position.lock().unwrap();
            let mut c = self.counter.lock().unwrap();
            *c += 1;
            Ok(format!("doc-{:05}-pos{}", *c, pos))
        }
    }

    /// Synthetic V-curve: `hfr = curvature·(pos − vertex)² + vertex_y`.
    /// Recovers the position from the document id stamped by [`StubCapturer`].
    struct StubMeasurer {
        vertex: i32,
        vertex_y: f64,
        curvature: f64,
        star_count: u32,
    }

    #[async_trait]
    impl MeasureOps for StubMeasurer {
        async fn measure(
            &self,
            document_id: &str,
            _min_area: usize,
            _max_area: usize,
            _threshold_sigma: f64,
        ) -> Result<HfrSample, String> {
            let pos: i32 = document_id
                .rsplit_once("pos")
                .and_then(|(_, s)| s.parse().ok())
                .ok_or_else(|| format!("bad document_id: {document_id}"))?;
            let dx = f64::from(pos - self.vertex);
            let hfr = self.curvature * dx * dx + self.vertex_y;
            Ok(HfrSample {
                hfr: Some(hfr),
                star_count: self.star_count,
            })
        }
    }

    #[tokio::test]
    async fn run_auto_focus_recovers_known_vertex() {
        let foc = StubFocuser {
            position: Mutex::new(1234),
        };
        let cap = StubCapturer {
            focuser: &foc,
            counter: Mutex::new(0),
        };
        let meas = StubMeasurer {
            vertex: 1234,
            vertex_y: 2.0,
            curvature: 1e-4,
            star_count: 100,
        };
        let result = run_auto_focus(
            &foc,
            &cap,
            &meas,
            (None, None),
            1234,
            Some(4.5),
            AutoFocusParams {
                duration: Duration::from_millis(100),
                step_size: 100,
                half_width: 400,
                min_area: 5,
                max_area: 1000,
                threshold_sigma: 5.0,
                min_fit_points: 5,
                min_star_fraction: DEFAULT_MIN_STAR_FRACTION,
                confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
                direction: SweepDirection::Ascending,
            },
        )
        .await
        .unwrap();
        assert!(
            (result.best_position - 1234).abs() <= 1,
            "best_position {} not within ±1 of 1234",
            result.best_position
        );
        assert!((result.best_hfr - 2.0).abs() < 1e-6);
        assert_eq!(result.samples_used, 9);
        assert_eq!(result.curve_points.len(), 9);
        assert!(result.curve_points.iter().all(|p| p.rejected.is_none()));
        assert!((result.fit_r_squared - 1.0).abs() < 1e-9);
        // The confirmation frame at the vertex measures the vertex
        // value, well inside the tolerance.
        assert!(result.confirmation.accepted);
        assert_eq!(result.confirmation.star_count, 100);
        assert_eq!(result.final_position, result.best_position);
        assert!((result.final_hfr - 2.0).abs() < 1e-6);
        assert_eq!(result.temperature_c, Some(4.5));
    }

    /// A descending walk (the order for a focuser whose backlash approach
    /// is `in`) visits the same grid largest-first, records the curve in
    /// that order, and still recovers the vertex — the range check must
    /// use the grid's extremes, not its first and last entries.
    #[tokio::test]
    async fn run_auto_focus_walks_the_grid_descending_when_asked() {
        let foc = StubFocuser {
            position: Mutex::new(1234),
        };
        let cap = StubCapturer {
            focuser: &foc,
            counter: Mutex::new(0),
        };
        let meas = StubMeasurer {
            vertex: 1234,
            vertex_y: 2.0,
            curvature: 1e-4,
            star_count: 100,
        };
        let result = run_auto_focus(
            &foc,
            &cap,
            &meas,
            (None, None),
            1234,
            None,
            AutoFocusParams {
                duration: Duration::from_millis(100),
                step_size: 100,
                half_width: 400,
                min_area: 5,
                max_area: 1000,
                threshold_sigma: 5.0,
                min_fit_points: 5,
                min_star_fraction: DEFAULT_MIN_STAR_FRACTION,
                confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
                direction: SweepDirection::Descending,
            },
        )
        .await
        .unwrap();
        let positions: Vec<i32> = result.curve_points.iter().map(|p| p.position).collect();
        assert_eq!(
            positions,
            vec![1634, 1534, 1434, 1334, 1234, 1134, 1034, 934, 834],
            "the sweep must visit the grid largest-first"
        );
        assert!(
            (result.best_position - 1234).abs() <= 1,
            "best_position {} not within ±1 of 1234",
            result.best_position
        );
        assert_eq!(result.final_position, result.best_position);
        assert_eq!(*foc.position.lock().unwrap(), result.best_position);
    }

    #[tokio::test]
    async fn run_auto_focus_errors_on_grid_too_small_after_clamp() {
        let foc = StubFocuser {
            position: Mutex::new(5000),
        };
        let cap = StubCapturer {
            focuser: &foc,
            counter: Mutex::new(0),
        };
        let meas = StubMeasurer {
            vertex: 5000,
            vertex_y: 2.0,
            curvature: 1e-4,
            star_count: 100,
        };
        let err = run_auto_focus(
            &foc,
            &cap,
            &meas,
            (Some(4900), Some(5100)),
            5000,
            Some(4.5),
            AutoFocusParams {
                duration: Duration::from_millis(100),
                step_size: 100,
                half_width: 500,
                min_area: 5,
                max_area: 1000,
                threshold_sigma: 5.0,
                min_fit_points: 5,
                min_star_fraction: DEFAULT_MIN_STAR_FRACTION,
                confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
                direction: SweepDirection::Ascending,
            },
        )
        .await;
        assert!(matches!(
            err,
            Err(AutoFocusError::GridTooSmall {
                available: 3,
                requested: 5
            })
        ));
    }

    /// Sparse stars: only the central pair has detections. With 9 grid
    /// points and only 2 useful samples, the run must error
    /// `NotEnoughStars`.
    #[tokio::test]
    async fn run_auto_focus_errors_on_not_enough_stars_after_skips() {
        struct Sparse;
        #[async_trait]
        impl MeasureOps for Sparse {
            async fn measure(
                &self,
                document_id: &str,
                _min_area: usize,
                _max_area: usize,
                _threshold_sigma: f64,
            ) -> Result<HfrSample, String> {
                let pos: i32 = document_id
                    .rsplit_once("pos")
                    .and_then(|(_, s)| s.parse().ok())
                    .unwrap();
                if (pos - 1234).abs() <= 50 {
                    Ok(HfrSample {
                        hfr: Some(2.5),
                        star_count: 50,
                    })
                } else {
                    Ok(HfrSample {
                        hfr: None,
                        star_count: 0,
                    })
                }
            }
        }
        let foc = StubFocuser {
            position: Mutex::new(1234),
        };
        let cap = StubCapturer {
            focuser: &foc,
            counter: Mutex::new(0),
        };
        let err = run_auto_focus(
            &foc,
            &cap,
            &Sparse,
            (None, None),
            1234,
            Some(4.5),
            AutoFocusParams {
                duration: Duration::from_millis(100),
                step_size: 100,
                half_width: 400,
                min_area: 5,
                max_area: 1000,
                threshold_sigma: 5.0,
                min_fit_points: 5,
                min_star_fraction: DEFAULT_MIN_STAR_FRACTION,
                confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
                direction: SweepDirection::Ascending,
            },
        )
        .await;
        assert!(matches!(
            err,
            Err(AutoFocusError::NotEnoughStars { needed: 5, .. })
        ));
        // A failed fit leaves the focuser where the sweep started, not
        // at the far end of the grid.
        assert_eq!(*foc.position.lock().unwrap(), 1234);
    }

    /// `capture()` errors mid-sweep on the second grid point — the
    /// run aborts and propagates the underlying message.
    #[tokio::test]
    async fn run_auto_focus_propagates_capture_error() {
        struct FailingCapturer {
            counter: Mutex<u64>,
        }
        #[async_trait]
        impl CaptureOps for FailingCapturer {
            async fn capture(&self, _: Duration) -> Result<String, String> {
                let mut c = self.counter.lock().unwrap();
                *c += 1;
                if *c == 2 {
                    Err("readout aborted".to_string())
                } else {
                    Ok(format!("doc-{:05}-pos9999", *c))
                }
            }
        }
        let foc = StubFocuser {
            position: Mutex::new(1234),
        };
        let cap = FailingCapturer {
            counter: Mutex::new(0),
        };
        let meas = StubMeasurer {
            vertex: 1234,
            vertex_y: 2.0,
            curvature: 1e-4,
            star_count: 100,
        };
        let err = run_auto_focus(
            &foc,
            &cap,
            &meas,
            (None, None),
            1234,
            Some(4.5),
            AutoFocusParams {
                duration: Duration::from_millis(10),
                step_size: 100,
                half_width: 400,
                min_area: 5,
                max_area: 1000,
                threshold_sigma: 5.0,
                min_fit_points: 5,
                min_star_fraction: DEFAULT_MIN_STAR_FRACTION,
                confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
                direction: SweepDirection::Ascending,
            },
        )
        .await;
        match err {
            Err(AutoFocusError::Equipment(msg)) => assert!(
                msg.contains("readout aborted"),
                "expected propagated message, got: {msg}"
            ),
            other => panic!("expected Equipment, got {other:?}"),
        }
    }

    /// `measure()` errors on the first sample — the run aborts and
    /// propagates the underlying message.
    #[tokio::test]
    async fn run_auto_focus_propagates_measure_error() {
        struct FailingMeasurer;
        #[async_trait]
        impl MeasureOps for FailingMeasurer {
            async fn measure(
                &self,
                _: &str,
                _: usize,
                _: usize,
                _: f64,
            ) -> Result<HfrSample, String> {
                Err("FITS decode failed".to_string())
            }
        }
        let foc = StubFocuser {
            position: Mutex::new(1234),
        };
        let cap = StubCapturer {
            focuser: &foc,
            counter: Mutex::new(0),
        };
        let err = run_auto_focus(
            &foc,
            &cap,
            &FailingMeasurer,
            (None, None),
            1234,
            Some(4.5),
            AutoFocusParams {
                duration: Duration::from_millis(10),
                step_size: 100,
                half_width: 400,
                min_area: 5,
                max_area: 1000,
                threshold_sigma: 5.0,
                min_fit_points: 5,
                min_star_fraction: DEFAULT_MIN_STAR_FRACTION,
                confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
                direction: SweepDirection::Ascending,
            },
        )
        .await;
        match err {
            Err(AutoFocusError::Equipment(msg)) => assert!(
                msg.contains("FITS decode failed"),
                "expected propagated message, got: {msg}"
            ),
            other => panic!("expected Equipment, got {other:?}"),
        }
    }

    /// `a > 0` but the fitted vertex falls outside the sampled grid
    /// — the curve is monotonic over the sampled range, so the run
    /// errors `MonotonicCurve` even though the parabola itself has
    /// a minimum somewhere off-grid. Achieved by sweeping on one
    /// arm of the V-curve only (vertex at 9999, sweep around 1234).
    #[tokio::test]
    async fn run_auto_focus_rejects_vertex_outside_sampled_grid() {
        let foc = StubFocuser {
            position: Mutex::new(1234),
        };
        let cap = StubCapturer {
            focuser: &foc,
            counter: Mutex::new(0),
        };
        let meas = StubMeasurer {
            vertex: 9999,
            vertex_y: 2.0,
            curvature: 1e-4,
            star_count: 100,
        };
        let err = run_auto_focus(
            &foc,
            &cap,
            &meas,
            (None, None),
            1234,
            Some(4.5),
            AutoFocusParams {
                duration: Duration::from_millis(10),
                step_size: 100,
                half_width: 400,
                min_area: 5,
                max_area: 1000,
                threshold_sigma: 5.0,
                min_fit_points: 5,
                min_star_fraction: DEFAULT_MIN_STAR_FRACTION,
                confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
                direction: SweepDirection::Ascending,
            },
        )
        .await;
        match err {
            Err(AutoFocusError::MonotonicCurve(msg)) => {
                assert!(
                    msg.contains("outside sampled grid"),
                    "expected vertex-outside-grid message, got: {msg}"
                );
            }
            other => panic!("expected MonotonicCurve, got {other:?}"),
        }
        assert_eq!(*foc.position.lock().unwrap(), 1234);
    }

    /// When the caller passes `None` for the starting temperature
    /// (e.g. the focuser doesn't implement Temperature, or the read
    /// failed), the result's `temperature_c` is also `None`. The
    /// rest of the sweep proceeds normally.
    #[tokio::test]
    async fn run_auto_focus_records_none_temperature() {
        let foc = StubFocuser {
            position: Mutex::new(1234),
        };
        let cap = StubCapturer {
            focuser: &foc,
            counter: Mutex::new(0),
        };
        let meas = StubMeasurer {
            vertex: 1234,
            vertex_y: 2.0,
            curvature: 1e-4,
            star_count: 100,
        };
        let result = run_auto_focus(
            &foc,
            &cap,
            &meas,
            (None, None),
            1234,
            None,
            AutoFocusParams {
                duration: Duration::from_millis(10),
                step_size: 100,
                half_width: 400,
                min_area: 5,
                max_area: 1000,
                threshold_sigma: 5.0,
                min_fit_points: 5,
                min_star_fraction: DEFAULT_MIN_STAR_FRACTION,
                confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
                direction: SweepDirection::Ascending,
            },
        )
        .await
        .unwrap();
        assert_eq!(result.temperature_c, None);
        assert!((result.best_position - 1234).abs() <= 1);
    }

    // ---- sparse gate, fit quality, confirmation, restore ----

    #[test]
    fn validate_params_rejects_min_star_fraction_outside_the_unit_interval() {
        for bad in [1.0, -0.1, f64::NAN, f64::INFINITY] {
            let p = AutoFocusParams {
                duration: Duration::from_millis(100),
                step_size: 50,
                half_width: 100,
                min_area: 5,
                max_area: 1000,
                threshold_sigma: 5.0,
                min_fit_points: 5,
                min_star_fraction: bad,
                confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
                direction: SweepDirection::Ascending,
            };
            assert!(
                matches!(
                    validate_params(&p),
                    Err(AutoFocusError::InvalidMinStarFraction(_))
                ),
                "{bad} was accepted"
            );
        }
    }

    #[test]
    fn validate_params_rejects_a_negative_or_non_finite_confirmation_tolerance() {
        for bad in [-0.5, f64::NAN, f64::NEG_INFINITY] {
            let p = AutoFocusParams {
                duration: Duration::from_millis(100),
                step_size: 50,
                half_width: 100,
                min_area: 5,
                max_area: 1000,
                threshold_sigma: 5.0,
                min_fit_points: 5,
                min_star_fraction: 0.0,
                confirmation_tolerance: bad,
                direction: SweepDirection::Ascending,
            };
            assert!(
                matches!(
                    validate_params(&p),
                    Err(AutoFocusError::InvalidConfirmationTolerance(_))
                ),
                "{bad} was accepted"
            );
        }
    }

    fn point(position: i32, hfr: Option<f64>, star_count: u32) -> CurvePoint {
        CurvePoint {
            position,
            hfr,
            star_count,
            document_id: format!("doc-pos{position}"),
            rejected: None,
        }
    }

    #[test]
    fn apply_sparse_gate_rejects_below_the_fraction_and_skips_starless_points() {
        let mut points = vec![
            point(0, None, 0),
            point(1, Some(5.0), 12),
            point(2, Some(4.0), 370),
            point(3, Some(8.0), 37),
            point(4, Some(4.5), 36),
        ];
        let threshold = apply_sparse_gate(&mut points, 0.1);
        assert!((threshold - 37.0).abs() < 1e-9, "threshold {threshold}");
        // Starless: untouched, and never a sample.
        assert_eq!(points[0].rejected, None);
        assert_eq!(points[0].accepted_sample(), None);
        assert_eq!(points[1].rejected, Some(Rejection::Sparse));
        assert_eq!(points[1].accepted_sample(), None);
        assert_eq!(points[2].rejected, None);
        assert_eq!(points[2].accepted_sample(), Some((2, 4.0, 370)));
        // Exactly at the threshold passes; one below does not.
        assert_eq!(points[3].rejected, None);
        assert_eq!(points[4].rejected, Some(Rejection::Sparse));
    }

    #[test]
    fn apply_sparse_gate_with_zero_fraction_rejects_nothing() {
        let mut points = vec![point(1, Some(5.0), 1), point(2, Some(4.0), 1000)];
        let threshold = apply_sparse_gate(&mut points, 0.0);
        assert!(threshold.abs() < f64::EPSILON);
        assert!(points.iter().all(|p| p.rejected.is_none()));
    }

    #[test]
    fn confirmation_verdict_needs_stars_the_gate_and_the_tolerance() {
        let sample = |hfr: Option<f64>, star_count: u32| HfrSample { hfr, star_count };
        // Inside the tolerance, dense enough.
        assert!(confirmation_accepted(
            &sample(Some(2.4), 300),
            37.0,
            2.0,
            0.25
        ));
        // Exactly at the bar passes.
        assert!(confirmation_accepted(
            &sample(Some(2.5), 300),
            37.0,
            2.0,
            0.25
        ));
        // Worse than the bar.
        assert!(!confirmation_accepted(
            &sample(Some(2.6), 300),
            37.0,
            2.0,
            0.25
        ));
        // Starless.
        assert!(!confirmation_accepted(&sample(None, 0), 37.0, 2.0, 0.25));
        // A good-looking HFR from a sparse frame is not trusted.
        assert!(!confirmation_accepted(
            &sample(Some(1.0), 5),
            37.0,
            2.0,
            0.25
        ));
    }

    #[test]
    fn lowest_sample_breaks_ties_toward_the_denser_frame() {
        let samples = [(1, 4.0, 10), (2, 3.0, 5), (3, 3.0, 50), (4, 9.0, 400)];
        assert_eq!(lowest_sample(&samples), Some((3, 3.0, 50)));
        assert_eq!(lowest_sample(&[]), None);
    }

    #[test]
    fn fit_parabola_reports_r_squared_one_for_an_exact_parabola() {
        let fit = fit_parabola(&make_v_samples(1234, 1.5, 1e-4)).unwrap();
        assert!((fit.r_squared - 1.0).abs() < 1e-9, "r² {}", fit.r_squared);
    }

    /// A coarse sweep recorded on a real rig: ±1200 steps at 300-step
    /// increments around 29966, with focus near 29766. The wings hold
    /// a handful of detections each — fragments of donuts, whose HFR
    /// is capped by their own area — beside dense frames near focus.
    const RECORDED_COARSE_SWEEP: [(i32, Option<f64>, u32); 9] = [
        (28766, None, 0),
        (29066, Some(5.29), 12),
        (29366, Some(16.66), 41),
        (29666, Some(4.01), 370),
        (29966, Some(7.95), 159),
        (30266, Some(7.59), 33),
        (30566, Some(4.09), 7),
        (30866, Some(4.45), 5),
        (31166, None, 0),
    ];

    #[test]
    fn fit_parabola_scores_a_fragment_dominated_sweep_near_zero() {
        let samples: Vec<(i32, f64, u32)> = RECORDED_COARSE_SWEEP
            .iter()
            .filter_map(|(p, h, s)| h.map(|h| (*p, h, *s)))
            .collect();
        let fit = fit_parabola(&samples).unwrap();
        assert!(fit.r_squared < 0.1, "r² {}", fit.r_squared);
        assert!(
            (fit.vertex_position() - 29926).abs() <= 2,
            "vertex {}",
            fit.vertex_position()
        );
    }

    /// Serves the recorded sweep at its grid positions and the true V
    /// — a hyperbola centred on focus, dense — anywhere else, which is
    /// where the confirmation frame lands.
    struct RecordedSweep;

    impl RecordedSweep {
        fn truth(position: i32) -> f64 {
            let dx = f64::from(position - 29766) / 27.0;
            dx.mul_add(dx, 1.0).sqrt()
        }
    }

    #[async_trait]
    impl MeasureOps for RecordedSweep {
        async fn measure(
            &self,
            document_id: &str,
            _min_area: usize,
            _max_area: usize,
            _threshold_sigma: f64,
        ) -> Result<HfrSample, String> {
            let pos: i32 = document_id
                .rsplit_once("pos")
                .and_then(|(_, s)| s.parse().ok())
                .ok_or_else(|| format!("bad document_id: {document_id}"))?;
            Ok(RECORDED_COARSE_SWEEP
                .iter()
                .find(|(p, _, _)| *p == pos)
                .map_or_else(
                    || HfrSample {
                        hfr: Some(Self::truth(pos)),
                        star_count: 300,
                    },
                    |(_, hfr, star_count)| HfrSample {
                        hfr: *hfr,
                        star_count: *star_count,
                    },
                ))
        }
    }

    fn coarse_params(min_star_fraction: f64, min_fit_points: usize) -> AutoFocusParams {
        AutoFocusParams {
            duration: Duration::from_millis(10),
            step_size: 300,
            half_width: 1200,
            min_area: 40,
            max_area: 5000,
            threshold_sigma: 10.0,
            min_fit_points,
            min_star_fraction,
            confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
            direction: SweepDirection::Ascending,
        }
    }

    #[tokio::test]
    async fn run_auto_focus_gates_the_sparse_wings_and_confirms_the_fit() {
        let foc = StubFocuser {
            position: Mutex::new(29966),
        };
        let cap = StubCapturer {
            focuser: &foc,
            counter: Mutex::new(0),
        };
        let result = run_auto_focus(
            &foc,
            &cap,
            &RecordedSweep,
            (None, None),
            29966,
            None,
            coarse_params(DEFAULT_MIN_STAR_FRACTION, 3),
        )
        .await
        .unwrap();
        let rejected: Vec<i32> = result
            .curve_points
            .iter()
            .filter(|p| p.rejected == Some(Rejection::Sparse))
            .map(|p| p.position)
            .collect();
        assert_eq!(rejected, vec![29066, 30266, 30566, 30866]);
        assert_eq!(result.samples_used, 3);
        assert_eq!(result.curve_points.len(), 9);
        assert!(
            (result.best_position - 29745).abs() <= 5,
            "best_position {}",
            result.best_position
        );
        assert!((result.fit_r_squared - 1.0).abs() < 1e-6);
        assert!(result.confirmation.accepted);
        assert_eq!(result.final_position, result.best_position);
        assert!(result.final_hfr < 1.5, "final_hfr {}", result.final_hfr);
        assert_eq!(*foc.position.lock().unwrap(), result.best_position);
    }

    #[tokio::test]
    async fn run_auto_focus_falls_back_to_the_lowest_sample_when_the_confirmation_fails() {
        // Gate off: the fit trusts the fragment wings and lands 160
        // steps from focus, where the confirmation frame measures
        // twice the best sweep sample.
        let foc = StubFocuser {
            position: Mutex::new(29966),
        };
        let cap = StubCapturer {
            focuser: &foc,
            counter: Mutex::new(0),
        };
        let result = run_auto_focus(
            &foc,
            &cap,
            &RecordedSweep,
            (None, None),
            29966,
            None,
            coarse_params(0.0, 5),
        )
        .await
        .unwrap();
        assert!(result.curve_points.iter().all(|p| p.rejected.is_none()));
        assert_eq!(result.samples_used, 7);
        assert!(
            (result.best_position - 29926).abs() <= 2,
            "best_position {}",
            result.best_position
        );
        assert!(result.fit_r_squared < 0.1);
        assert!(!result.confirmation.accepted);
        assert!(result.confirmation.hfr.is_some_and(|h| h > 5.0));
        assert_eq!(result.final_position, 29666);
        assert!((result.final_hfr - 4.01).abs() < 1e-9);
        assert_eq!(*foc.position.lock().unwrap(), 29666);
    }

    #[tokio::test]
    async fn run_auto_focus_restores_the_start_when_the_gate_leaves_too_few_samples() {
        let foc = StubFocuser {
            position: Mutex::new(29966),
        };
        let cap = StubCapturer {
            focuser: &foc,
            counter: Mutex::new(0),
        };
        let err = run_auto_focus(
            &foc,
            &cap,
            &RecordedSweep,
            (None, None),
            29966,
            None,
            coarse_params(DEFAULT_MIN_STAR_FRACTION, 5),
        )
        .await;
        assert!(matches!(
            err,
            Err(AutoFocusError::NotEnoughStars { got: 3, needed: 5 })
        ));
        assert_eq!(*foc.position.lock().unwrap(), 29966);
    }

    /// A starless confirmation frame is a rejected confirmation: the
    /// focuser falls back to the lowest sweep sample. The vertex sits
    /// between grid points so the confirmation capture is the only
    /// off-grid measurement.
    #[tokio::test]
    async fn run_auto_focus_treats_a_starless_confirmation_as_rejected() {
        struct StarlessOffGrid;
        #[async_trait]
        impl MeasureOps for StarlessOffGrid {
            async fn measure(
                &self,
                document_id: &str,
                _: usize,
                _: usize,
                _: f64,
            ) -> Result<HfrSample, String> {
                let pos: i32 = document_id
                    .rsplit_once("pos")
                    .and_then(|(_, s)| s.parse().ok())
                    .unwrap();
                if (pos - 834) % 100 == 0 {
                    let dx = f64::from(pos - 1250);
                    Ok(HfrSample {
                        hfr: Some(1e-4 * dx * dx + 2.0),
                        star_count: 100,
                    })
                } else {
                    Ok(HfrSample {
                        hfr: None,
                        star_count: 0,
                    })
                }
            }
        }
        let foc = StubFocuser {
            position: Mutex::new(1234),
        };
        let cap = StubCapturer {
            focuser: &foc,
            counter: Mutex::new(0),
        };
        let result = run_auto_focus(
            &foc,
            &cap,
            &StarlessOffGrid,
            (None, None),
            1234,
            None,
            AutoFocusParams {
                duration: Duration::from_millis(10),
                step_size: 100,
                half_width: 400,
                min_area: 5,
                max_area: 1000,
                threshold_sigma: 5.0,
                min_fit_points: 5,
                min_star_fraction: DEFAULT_MIN_STAR_FRACTION,
                confirmation_tolerance: DEFAULT_CONFIRMATION_TOLERANCE,
                direction: SweepDirection::Ascending,
            },
        )
        .await
        .unwrap();
        assert!((result.best_position - 1250).abs() <= 1);
        assert_eq!(result.confirmation.hfr, None);
        assert_eq!(result.confirmation.star_count, 0);
        assert!(!result.confirmation.accepted);
        assert_eq!(result.final_position, 1234);
        assert!((result.final_hfr - 2.0256).abs() < 1e-9);
        assert_eq!(*foc.position.lock().unwrap(), 1234);
    }
}
