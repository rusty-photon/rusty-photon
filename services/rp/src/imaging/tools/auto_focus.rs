//! `auto_focus`: V-curve focus sweep compound tool.
//!
//! The driving logic — sweep grid construction, the move/capture/measure
//! loop, parabolic least-squares fit, vertex-in-range validation — is
//! pure Rust and fully unit-testable via the [`FocuserOps`],
//! [`CaptureOps`], [`MeasureOps`] traits. The MCP wrapper in `mcp.rs`
//! provides concrete adapters that bind to the real Alpaca focuser /
//! camera and the image cache; tests substitute synthetic adapters
//! that drive the loop with deterministic per-position HFR data.
//!
//! Behavioral contract: `docs/services/rp.md` → Compound Tools →
//! `auto_focus` Contract.

use async_trait::async_trait;
use serde::Serialize;
use std::time::Duration;
use thiserror::Error;
use tracing::debug;

#[derive(Debug, Clone)]
pub struct AutoFocusParams {
    pub duration: Duration,
    pub step_size: i32,
    pub half_width: i32,
    pub min_area: usize,
    pub max_area: usize,
    pub threshold_sigma: f64,
    pub min_fit_points: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CurvePoint {
    pub position: i32,
    pub hfr: Option<f64>,
    pub star_count: u32,
    pub document_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AutoFocusResult {
    pub best_position: i32,
    pub best_hfr: f64,
    pub final_position: i32,
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
    #[error("not enough stars: only {got} of {needed} required samples have non-null HFR")]
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
/// build. Generous enough that any plausible auto-focus run fits well
/// inside the cap (typical 10–30 points; even an aggressively-fine
/// sweep over a wide range stays under 200), so the cap is purely a
/// guardrail against operator misconfiguration that would otherwise
/// produce thousands of captures and tie up the rig for hours.
pub const MAX_GRID_POINTS: usize = 1000;

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

/// Build the sweep grid `[start, start+step, …]` continuing while the
/// position stays `≤ end`, then clamped to `[min_bound, max_bound]`.
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
}

impl ParabolaFit {
    #[must_use]
    pub fn vertex_position(&self) -> i32 {
        #[expect(
            clippy::as_conversions,
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
/// Returns `MonotonicCurve` if `a ≤ 0` (the curve has no minimum) or if
/// the design matrix is too ill-conditioned to invert (essentially
/// flat input, where the vertex is undefined).
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
    let det = m4 * (m2 * m0 - m1 * m1) - m3 * (m3 * m0 - m1 * m2) + m2 * (m3 * m1 - m2 * m2);
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
    let det_a = t2 * (m2 * m0 - m1 * m1) - m3 * (t1 * m0 - m1 * t0) + m2 * (t1 * m1 - m2 * t0);
    let det_b = m4 * (t1 * m0 - m1 * t0) - t2 * (m3 * m0 - m1 * m2) + m2 * (m3 * t0 - t1 * m2);
    let det_c = m4 * (m2 * t0 - t1 * m1) - m3 * (m3 * t0 - t1 * m2) + t2 * (m3 * m1 - m2 * m2);
    let a = det_a / det;
    let b = det_b / det;
    let c = det_c / det;
    if a <= 0.0 {
        return Err(AutoFocusError::MonotonicCurve(format!(
            "non-positive leading coefficient (a={a:.3e})"
        )));
    }
    Ok(ParabolaFit { a, b, c, offset_x })
}

/// Drive the V-curve sweep against the supplied focuser/capturer/measurer
/// adapters. See `docs/services/rp.md` → `auto_focus` Contract for the
/// behavioral spec; this function is the reference implementation.
///
/// `starting_position` and `starting_temperature_c` must be the values the
/// caller already read from the focuser for the `focus_started` event.
/// The contract guarantees a single read of each — passing them in keeps
/// the event payload and the result strictly consistent and avoids extra
/// Alpaca round-trips inside the loop.
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

    let grid = build_grid(
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

    let temperature_c = starting_temperature_c;
    debug!(
        current_position = starting_position,
        grid_len = grid.len(),
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
        });
    }

    let valid_samples: Vec<(i32, f64, u32)> = curve_points
        .iter()
        .filter_map(|p| p.hfr.map(|h| (p.position, h, p.star_count)))
        .collect();
    if valid_samples.len() < params.min_fit_points {
        return Err(AutoFocusError::NotEnoughStars {
            got: valid_samples.len(),
            needed: params.min_fit_points,
        });
    }

    let fit = fit_parabola(&valid_samples)?;
    let best_position = fit.vertex_position();
    // `valid_samples` was filtered from `grid`, and we just returned
    // above unless `valid_samples.len() >= min_fit_points >= 1`, so
    // `grid` is non-empty here. Pattern-match the `Option`s instead of
    // panicking to satisfy the workspace's no-panic policy.
    let (Some(&grid_min), Some(&grid_max)) = (grid.first(), grid.last()) else {
        return Err(AutoFocusError::MonotonicCurve(
            "grid is empty despite having valid samples".into(),
        ));
    };
    if best_position < grid_min || best_position > grid_max {
        return Err(AutoFocusError::MonotonicCurve(format!(
            "fitted vertex {best_position} is outside sampled grid [{grid_min}, {grid_max}]"
        )));
    }
    let best_hfr = fit.vertex_value();

    let final_position = focuser
        .move_to(best_position)
        .await
        .map_err(AutoFocusError::Equipment)?;

    Ok(AutoFocusResult {
        best_position,
        best_hfr,
        final_position,
        samples_used: valid_samples.len(),
        curve_points,
        temperature_c,
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
        assert_eq!(result.final_position, result.best_position);
        assert_eq!(result.temperature_c, Some(4.5));
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
            },
        )
        .await;
        assert!(matches!(
            err,
            Err(AutoFocusError::NotEnoughStars { needed: 5, .. })
        ));
    }

    /// `capture()` errors mid-sweep on the second grid point — the
    /// run aborts and propagates the underlying message.
    #[tokio::test]
    async fn run_auto_focus_propagates_capture_error() {
        let foc = StubFocuser {
            position: Mutex::new(1234),
        };
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
            },
        )
        .await
        .unwrap();
        assert_eq!(result.temperature_c, None);
        assert!((result.best_position - 1234).abs() <= 1);
    }
}
