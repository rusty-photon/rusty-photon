#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

//! `rp-focus-curve` — the V-curve a star traces through focus, and its
//! weighted fit (docs/crates/rp-focus-curve.md).
//!
//! [`fit_hyperbola`] fits `value(x) = a·√(1 + ((x − x₀)/b)²)` to
//! `(position, value, weight)` samples: the defocus curve of a star, a
//! cone of half-flux radius growing by `a/b` a step either side of focus,
//! rounded off at `a` near it. The focus-model provider's capture sweep
//! and `rp`'s guide-train sweep both fit it here; what each does around
//! the fit — the gate, the bound on the centre, a threshold on the R² —
//! stays with the caller.

use thiserror::Error;

/// The most refinement steps a fit takes. A fit from the closed-form
/// start converges in tens; the cap only bounds a pathological one.
const MAX_ITERATIONS: usize = 100;

/// A refinement step that lowers the residual by less than this fraction
/// of it ends the refinement.
const CONVERGED: f64 = 1e-12;

/// The damping the refinement starts from, and the floor it relaxes to
/// after a step that lowered the residual.
const INITIAL_DAMPING: f64 = 1e-3;
const MIN_DAMPING: f64 = 1e-12;

/// Damping past this leaves a step too short to lower the residual: the
/// fit sits at its minimum.
const MAX_DAMPING: f64 = 1e16;

/// How close to singular the start's design matrix may come, relative to
/// the product of its diagonal, and how small a curvature may be,
/// relative to the mean squared value, before it counts as none. Tested
/// against a scale, never against zero: rounding leaves a flat curve, or
/// a rank-deficient matrix, a residual rather than an exact zero.
const SINGULAR: f64 = 1e-12;

/// The centres the scanned starts try, as intervals across the samples
/// and a span beyond either end, and how many of the scanned starts are
/// refined beside the closed-form one.
const SCANNED_INTERVALS: u32 = 60;
const SCANNED_REFINED: usize = 2;

type Vec3 = [f64; 3];
type Mat3 = [Vec3; 3];

/// Why the samples produced no curve.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FitError {
    /// Fewer than three samples carry a weight.
    #[error("not enough stars: only {got} of the 3 samples a fit needs carry any weight")]
    NotEnoughStars { got: usize },
    /// The samples place no minimum: too few positions, values whose
    /// squares do not turn upward, or a fit that flattened to a constant.
    #[error("monotonic curve: {0}")]
    MonotonicCurve(String),
}

/// `value(x) = √(minimum² + slope²·(x − centre)²)`, which is the model's
/// `a·√(1 + ((x − x₀)/b)²)` with `centre` x₀, `minimum` a and `slope`
/// a/b. A `minimum` of 0 is the limiting V, two lines meeting at the
/// centre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hyperbola {
    /// Best focus, in focuser steps.
    pub centre: f64,
    /// The value at the centre: the focused HFR (or HFD), at least 0.
    pub minimum: f64,
    /// The wing slope, in value units per step; positive.
    pub slope: f64,
}

impl Hyperbola {
    /// The curve's value at `position`.
    #[must_use]
    pub fn value_at(&self, position: f64) -> f64 {
        self.minimum.hypot(self.slope * (position - self.centre))
    }
}

/// A fitted curve and how well it explains the samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HyperbolaFit {
    pub curve: Hyperbola,
    /// The weighted coefficient of determination over the fitted
    /// samples, clamped to `[0, 1]`.
    pub r_squared: f64,
}

/// One weighted sample, its position in the fit's frame.
#[derive(Debug, Clone, Copy)]
struct Point {
    u: f64,
    value: f64,
    weight: f64,
}

/// What the refinement moves: the centre in the fit's frame, and the
/// squares of the minimum and of the slope there, both held at zero or
/// above.
#[derive(Debug, Clone, Copy)]
struct Params {
    centre: f64,
    minimum_sq: f64,
    slope_sq: f64,
}

impl Params {
    fn value_at(self, u: f64) -> f64 {
        let offset = u - self.centre;
        self.slope_sq
            .mul_add(offset * offset, self.minimum_sq)
            .sqrt()
    }

    fn moved_by(self, [centre, minimum_sq, slope_sq]: Vec3) -> Self {
        Self {
            centre: self.centre + centre,
            minimum_sq: (self.minimum_sq + minimum_sq).max(0.0),
            slope_sq: (self.slope_sq + slope_sq).max(0.0),
        }
    }
}

/// Fit the hyperbola to `(position, value, weight)` samples by weighted
/// least squares in the value.
///
/// A closed-form start from the squared values, and the best two of a
/// scan of centres, are each refined by Levenberg–Marquardt, and the fit
/// with the smallest residual is kept (docs/crates/rp-focus-curve.md
/// § The fit). A sample of weight 0 is left out.
///
/// Three samples are fitted exactly when a hyperbola passes through them,
/// and otherwise as the nearest V, at an R² below 1: a fit, not an error.
///
/// # Errors
///
/// [`FitError::NotEnoughStars`] with fewer than three weighted samples;
/// [`FitError::MonotonicCurve`] when they sit at fewer than three
/// positions, when the design matrix is too close to singular, when
/// their squared values do not turn upward, or when the fit flattens to
/// a constant, which has no centre.
pub fn fit_hyperbola(samples: &[(i32, f64, u32)]) -> Result<HyperbolaFit, FitError> {
    let weighted: Vec<(i32, f64, f64)> = samples
        .iter()
        .filter(|(_, _, weight)| *weight > 0)
        .map(|(position, value, weight)| (*position, *value, f64::from(*weight)))
        .collect();
    if weighted.len() < 3 {
        return Err(FitError::NotEnoughStars {
            got: weighted.len(),
        });
    }
    let mut positions: Vec<i32> = weighted.iter().map(|(position, _, _)| *position).collect();
    positions.sort_unstable();
    positions.dedup();
    let (Some(&lowest), Some(&highest), 3..) =
        (positions.first(), positions.last(), positions.len())
    else {
        return Err(FitError::MonotonicCurve(format!(
            "the samples sit at {} positions, and a curve needs 3",
            positions.len()
        )));
    };

    // Recentred on the weighted mean and scaled by half the span: at
    // real focuser scales the fourth-power sums would otherwise lose
    // most of their precision.
    let total_weight: f64 = weighted.iter().map(|(_, _, weight)| weight).sum();
    let mean = weighted
        .iter()
        .map(|(position, _, weight)| weight * f64::from(*position))
        .sum::<f64>()
        / total_weight;
    let half_span = (f64::from(highest) - f64::from(lowest)) / 2.0;
    let points: Vec<Point> = weighted
        .iter()
        .map(|(position, value, weight)| Point {
            u: (f64::from(*position) - mean) / half_span,
            value: *value,
            weight: *weight,
        })
        .collect();

    // A curvature at rounding level, against the squared values it
    // would have to bend, is no curvature.
    let flat = SINGULAR
        * weighted.iter().fold(0.0, |sum, (_, value, weight)| {
            (weight * value).mul_add(*value, sum)
        })
        / total_weight;
    let (params, residual) = best_fit(&points, start(&points, flat)?);
    if params.slope_sq <= flat {
        return Err(FitError::MonotonicCurve(
            "the fitted curve flattened to a constant, which has no centre".to_owned(),
        ));
    }
    let curve = Hyperbola {
        centre: half_span.mul_add(params.centre, mean),
        minimum: params.minimum_sq.sqrt(),
        slope: params.slope_sq.sqrt() / half_span,
    };
    if !(curve.centre.is_finite() && curve.minimum.is_finite() && curve.slope.is_finite()) {
        return Err(FitError::MonotonicCurve(format!(
            "the fit found no finite curve ({curve:?})"
        )));
    }
    Ok(HyperbolaFit {
        curve,
        r_squared: r_squared(&points, total_weight, residual),
    })
}

/// The weighted sums of the start's normal equations.
#[derive(Default)]
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

/// The closed-form start: the square of the hyperbola is the parabola
/// `value² = minimum² + slope²·(u − centre)²`, so a weighted linear fit
/// of the squared values places all three parameters. A curvature at or
/// below `flat` is none.
fn start(points: &[Point], flat: f64) -> Result<Params, FitError> {
    let mut m = Moments::default();
    for point in points {
        let (u, weight) = (point.u, point.weight);
        let u2 = u * u;
        let squared = point.value * point.value;
        m.m4 = (weight * u2).mul_add(u2, m.m4);
        m.m3 = (weight * u2).mul_add(u, m.m3);
        m.m2 = weight.mul_add(u2, m.m2);
        m.m1 = weight.mul_add(u, m.m1);
        m.m0 += weight;
        m.t2 = (weight * u2).mul_add(squared, m.t2);
        m.t1 = (weight * u).mul_add(squared, m.t1);
        m.t0 = weight.mul_add(squared, m.t0);
    }
    let design = [[m.m4, m.m3, m.m2], [m.m3, m.m2, m.m1], [m.m2, m.m1, m.m0]];
    let det = det3(design);
    let scale = (m.m4 * m.m2 * m.m0).abs();
    if det.abs() <= scale * SINGULAR {
        return Err(FitError::MonotonicCurve(format!(
            "design matrix is singular (det={det:.3e}, scale={scale:.3e})"
        )));
    }
    let [curvature, linear, constant] = cramer(design, [m.t2, m.t1, m.t0], det);
    if curvature.is_nan() || curvature <= flat {
        return Err(FitError::MonotonicCurve(format!(
            "the squared values do not turn upward (curvature {curvature:.3e})"
        )));
    }
    Ok(Params {
        centre: -linear / (2.0 * curvature),
        // A squared curve that dips below zero has wings steeper than
        // any rounded floor allows: start from the limiting V.
        minimum_sq: (-linear / (4.0 * curvature))
            .mul_add(linear, constant)
            .max(0.0),
        slope_sq: curvature,
    })
}

/// Refine the closed-form start and the best of the scanned ones, and
/// keep the fit with the smallest residual. A residual can hold more
/// than one minimum, most of all on samples from one wing, and the
/// closed-form start can sit nearer the wrong one.
fn best_fit(points: &[Point], closed_form: Params) -> (Params, f64) {
    scanned_starts(points)
        .into_iter()
        .map(|start| refine(points, start))
        .fold(refine(points, closed_form), |best, fit| {
            if fit.1 < best.1 {
                fit
            } else {
                best
            }
        })
}

/// Starts at centres scanned across the samples and a span beyond either
/// end, each with the minimum and slope [`profile_start`] places for it:
/// the [`SCANNED_REFINED`] with the smallest residual.
fn scanned_starts(points: &[Point]) -> Vec<Params> {
    let (low, high) = points
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), point| {
            (low.min(point.u), high.max(point.u))
        });
    let width = high - low;
    let mut starts: Vec<(Params, f64)> = (0..=SCANNED_INTERVALS)
        .filter_map(|interval| {
            let fraction = f64::from(interval) / f64::from(SCANNED_INTERVALS);
            let centre = (3.0 * width).mul_add(fraction, low - width);
            profile_start(points, centre).map(|start| (start, residual(points, start)))
        })
        .collect();
    starts.sort_by(|a, b| a.1.total_cmp(&b.1));
    starts
        .into_iter()
        .take(SCANNED_REFINED)
        .map(|(start, _)| start)
        .collect()
}

/// For a fixed centre, the minimum and slope a linear fit of the squared
/// values places, each sample weighted by `w / value²` so it counts
/// about as its residual in the value does; `None` when that fit does
/// not turn upward.
fn profile_start(points: &[Point], centre: f64) -> Option<Params> {
    let (mut s0, mut s1, mut s2, mut t0, mut t1) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for point in points {
        let offset = point.u - centre;
        let offset_sq = offset * offset;
        let squared = point.value * point.value;
        let weight = point.weight / squared.max(f64::MIN_POSITIVE);
        s0 += weight;
        s1 = weight.mul_add(offset_sq, s1);
        s2 = (weight * offset_sq).mul_add(offset_sq, s2);
        t0 = weight.mul_add(squared, t0);
        t1 = (weight * offset_sq).mul_add(squared, t1);
    }
    let det = s0.mul_add(s2, -(s1 * s1));
    if det.is_nan() || det <= 0.0 {
        return None;
    }
    let minimum_sq = s2.mul_add(t0, -(s1 * t1)) / det;
    let slope_sq = s0.mul_add(t1, -(s1 * t0)) / det;
    (slope_sq.is_finite() && slope_sq > 0.0 && minimum_sq.is_finite()).then_some(Params {
        centre,
        minimum_sq: minimum_sq.max(0.0),
        slope_sq,
    })
}

/// Levenberg–Marquardt from `start` on the weighted residuals in the
/// value itself. Only a step that lowers the residual is taken, so the
/// result is never worse than the start. Answers the parameters and
/// their residual.
fn refine(points: &[Point], start: Params) -> (Params, f64) {
    // The derivatives divide by the curve's value, which is 0 only at
    // the centre of the limiting V; a floor keeps them finite there.
    let largest = points
        .iter()
        .map(|point| point.value.abs())
        .fold(0.0, f64::max);
    let floor = (f64::EPSILON * largest).max(f64::MIN_POSITIVE);
    let mut params = start;
    let mut residual = residual(points, params);
    let mut damping = INITIAL_DAMPING;
    for _ in 0..MAX_ITERATIONS {
        if residual <= 0.0 {
            break;
        }
        let (normal, gradient) = normal_equations(points, params, floor);
        let Some((next, next_residual)) =
            descend(points, params, residual, normal, gradient, &mut damping)
        else {
            break;
        };
        let improvement = residual - next_residual;
        let previous = residual;
        params = next;
        residual = next_residual;
        if improvement <= CONVERGED * previous {
            break;
        }
    }
    (params, residual)
}

/// One accepted step: raise the damping until a step lowers the residual,
/// then relax it for the next. `None` when no step short of
/// [`MAX_DAMPING`] does, which is the minimum.
fn descend(
    points: &[Point],
    params: Params,
    residual_now: f64,
    normal: Mat3,
    gradient: Vec3,
    damping: &mut f64,
) -> Option<(Params, f64)> {
    loop {
        if *damping > MAX_DAMPING {
            return None;
        }
        if let Some(step) = damped_step(normal, gradient, *damping, params) {
            let next = params.moved_by(step);
            let next_residual = residual(points, next);
            if next_residual < residual_now {
                *damping = (*damping / 10.0).max(MIN_DAMPING);
                return Some((next, next_residual));
            }
        }
        *damping *= 10.0;
    }
}

/// The Gauss–Newton normal matrix `JᵀWJ` and gradient `JᵀWr` at `params`.
fn normal_equations(points: &[Point], params: Params, floor: f64) -> (Mat3, Vec3) {
    let mut normal = [[0.0; 3]; 3];
    let mut gradient = [0.0; 3];
    for point in points {
        let offset = point.u - params.centre;
        let exact = params.value_at(point.u);
        let value = exact.max(floor);
        let jacobian = [
            -params.slope_sq * offset / value,
            0.5 / value,
            0.5 * offset * offset / value,
        ];
        let weighted = jacobian.map(|derivative| point.weight * derivative);
        let error = point.value - exact;
        for (row, weighted_derivative) in normal.iter_mut().zip(weighted) {
            for (cell, derivative) in row.iter_mut().zip(jacobian) {
                *cell = weighted_derivative.mul_add(derivative, *cell);
            }
        }
        for (cell, weighted_derivative) in gradient.iter_mut().zip(weighted) {
            *cell = weighted_derivative.mul_add(error, *cell);
        }
    }
    (normal, gradient)
}

/// Solve the damped normal equations. The damping scales each diagonal
/// entry by its own size, floored so a parameter the samples barely
/// constrain is still damped; `None` for a matrix that will not invert.
///
/// A squared parameter that sits at zero, where the step would take it
/// below, is held there and the others are solved without it. A step
/// solved as if it could go negative and then clamped is no step along
/// the bound, and the refinement would stall short of the best limiting
/// V.
fn damped_step(normal: Mat3, gradient: Vec3, damping: f64, params: Params) -> Option<Vec3> {
    let [[n00, n01, n02], [n10, n11, n12], [n20, n21, n22]] = normal;
    let floor = n00.max(n11).max(n22) * SINGULAR;
    let damped = [
        [damping.mul_add(n00.max(floor), n00), n01, n02],
        [n10, damping.mul_add(n11.max(floor), n11), n12],
        [n20, n21, damping.mul_add(n22.max(floor), n22)],
    ];
    let mut held = [false; 3];
    loop {
        let (matrix, rhs) = hold(damped, gradient, held);
        let det = det3(matrix);
        if !det.is_normal() {
            return None;
        }
        let step = cramer(matrix, rhs, det);
        if !step.iter().all(|value| value.is_finite()) {
            return None;
        }
        let [_, minimum_step, slope_step] = step;
        let pushed = [
            false,
            held[1] || (params.minimum_sq <= 0.0 && minimum_step < 0.0),
            held[2] || (params.slope_sq <= 0.0 && slope_step < 0.0),
        ];
        if pushed == held {
            return Some(step);
        }
        held = pushed;
    }
}

/// The system with each held parameter's row and column replaced by the
/// identity and its right-hand side by zero, so the solve leaves it
/// where it is and solves the others without it.
fn hold(matrix: Mat3, rhs: Vec3, held: [bool; 3]) -> (Mat3, Vec3) {
    let mut matrix = matrix;
    let mut rhs = rhs;
    for (row_index, ((row, row_held), value)) in
        matrix.iter_mut().zip(held).zip(rhs.iter_mut()).enumerate()
    {
        for (column_index, (cell, column_held)) in row.iter_mut().zip(held).enumerate() {
            if row_held || column_held {
                *cell = if row_index == column_index { 1.0 } else { 0.0 };
            }
        }
        if row_held {
            *value = 0.0;
        }
    }
    (matrix, rhs)
}

/// The weighted residual `Σ w·(value − curve)²`.
fn residual(points: &[Point], params: Params) -> f64 {
    points.iter().fold(0.0, |sum, point| {
        let error = point.value - params.value_at(point.u);
        (point.weight * error).mul_add(error, sum)
    })
}

/// `1 − residual / spread` over the weighted samples, clamped to
/// `[0, 1]`; 0 when the values have no spread to explain.
fn r_squared(points: &[Point], total_weight: f64, residual: f64) -> f64 {
    let mean = points
        .iter()
        .fold(0.0, |sum, point| point.weight.mul_add(point.value, sum))
        / total_weight;
    let spread = points.iter().fold(0.0, |sum, point| {
        let deviation = point.value - mean;
        (point.weight * deviation).mul_add(deviation, sum)
    });
    if spread > 0.0 {
        (1.0 - residual / spread).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Determinant of a 3×3 matrix given by rows: the first-row cofactor
/// expansion with every multiply-add fused.
const fn det3([r0, r1, r2]: Mat3) -> f64 {
    let minor0 = r1[2].mul_add(-r2[1], r1[1] * r2[2]);
    let minor1 = r1[2].mul_add(-r2[0], r1[0] * r2[2]);
    let minor2 = r1[1].mul_add(-r2[0], r1[0] * r2[1]);
    r0[2].mul_add(minor2, r0[1].mul_add(-minor1, r0[0] * minor0))
}

/// Cramer's rule for `matrix · x = rhs`, given the matrix's determinant.
const fn cramer([r0, r1, r2]: Mat3, [b0, b1, b2]: Vec3, det: f64) -> Vec3 {
    [
        det3([[b0, r0[1], r0[2]], [b1, r1[1], r1[2]], [b2, r2[1], r2[2]]]) / det,
        det3([[r0[0], b0, r0[2]], [r1[0], b1, r1[2]], [r2[0], b2, r2[2]]]) / det,
        det3([[r0[0], r0[1], b0], [r1[0], r1[1], b1], [r2[0], r2[1], b2]]) / det,
    ]
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// Samples of `curve` on a grid of `points` positions `step` apart
    /// around `centre`, each of weight `weight`.
    fn sampled(
        curve: Hyperbola,
        centre: i32,
        step: i32,
        points: i32,
        weight: u32,
    ) -> Vec<(i32, f64, u32)> {
        (0..points)
            .map(|index| {
                let position = centre + (index - points / 2) * step;
                (position, curve.value_at(f64::from(position)), weight)
            })
            .collect()
    }

    const RIG: Hyperbola = Hyperbola {
        centre: 29_766.0,
        minimum: 1.2,
        slope: 0.04,
    };

    #[test]
    fn an_exact_hyperbola_is_recovered_at_real_focuser_scales() {
        let fit = fit_hyperbola(&sampled(RIG, 29_666, 100, 9, 100)).unwrap();
        assert!((fit.curve.centre - RIG.centre).abs() < 1e-6, "{fit:?}");
        assert!((fit.curve.minimum - RIG.minimum).abs() < 1e-9, "{fit:?}");
        assert!((fit.curve.slope - RIG.slope).abs() < 1e-12, "{fit:?}");
        assert!(fit.r_squared > 1.0 - 1e-12, "{fit:?}");
    }

    #[test]
    fn the_limiting_v_is_recovered() {
        let v = Hyperbola {
            centre: 5_013.0,
            minimum: 0.0,
            slope: 0.05,
        };
        let fit = fit_hyperbola(&sampled(v, 5_000, 40, 9, 10)).unwrap();
        assert!((fit.curve.centre - v.centre).abs() < 1e-6, "{fit:?}");
        assert!(fit.curve.minimum < 1e-6, "{fit:?}");
        assert!((fit.curve.slope - v.slope).abs() < 1e-9, "{fit:?}");
    }

    #[test]
    fn three_samples_on_a_hyperbola_are_fitted_exactly() {
        let samples = sampled(RIG, 29_766, 150, 3, 50);
        let fit = fit_hyperbola(&samples).unwrap();
        assert!((fit.curve.centre - RIG.centre).abs() < 1e-6, "{fit:?}");
        assert!((fit.curve.minimum - RIG.minimum).abs() < 1e-6, "{fit:?}");
        assert!(fit.r_squared > 1.0 - 1e-12, "{fit:?}");
    }

    /// The three dense samples of the #1187 coarse sweep, focus near
    /// 29766: their squares dip below zero, so no hyperbola passes
    /// through them, and the fit is the nearest V.
    #[test]
    fn three_samples_no_hyperbola_passes_through_fit_the_nearest_v() {
        let samples = [
            (29_366, 16.66, 41),
            (29_666, 4.01, 370),
            (29_966, 7.95, 159),
        ];
        let fit = fit_hyperbola(&samples).unwrap();
        assert_eq!(fit.curve.minimum, 0.0, "{fit:?}");
        assert!((fit.curve.centre - 29_767.19).abs() < 0.01, "{fit:?}");
        assert!(fit.r_squared > 0.998 && fit.r_squared < 1.0, "{fit:?}");
        assert!(at_a_minimum(&samples, fit), "{fit:?}");
    }

    #[test]
    fn a_flat_curve_has_no_minimum() {
        let samples: Vec<(i32, f64, u32)> = (0..7).map(|index| (index * 10, 2.0, 10)).collect();
        let err = fit_hyperbola(&samples).unwrap_err();
        assert!(
            matches!(&err, FitError::MonotonicCurve(why) if why.contains("do not turn upward")),
            "{err}"
        );
    }

    #[test]
    fn a_concave_curve_has_no_minimum() {
        let samples: Vec<(i32, f64, u32)> = (-3..=3)
            .map(|index| (100 + index * 10, 20.0 - f64::from(index * index), 10))
            .collect();
        let err = fit_hyperbola(&samples).unwrap_err();
        assert!(
            matches!(&err, FitError::MonotonicCurve(why) if why.contains("do not turn upward")),
            "{err}"
        );
    }

    #[test]
    fn fewer_than_three_weighted_samples_cannot_be_fitted() {
        let err = fit_hyperbola(&[(0, 1.0, 10), (10, 2.0, 0), (20, 3.0, 0)]).unwrap_err();
        assert_eq!(err, FitError::NotEnoughStars { got: 1 });
        assert_eq!(
            err.to_string(),
            "not enough stars: only 1 of the 3 samples a fit needs carry any weight"
        );
    }

    #[test]
    fn samples_at_two_positions_place_no_curve() {
        let err = fit_hyperbola(&[(0, 3.0, 10), (0, 3.2, 10), (50, 1.0, 10)]).unwrap_err();
        assert_eq!(
            err,
            FitError::MonotonicCurve(
                "the samples sit at 2 positions, and a curve needs 3".to_owned()
            )
        );
    }

    #[test]
    fn a_zero_weight_sample_is_left_out() {
        let mut samples = sampled(RIG, 29_666, 100, 9, 100);
        let clean = fit_hyperbola(&samples).unwrap();
        samples.push((29_800, 40.0, 0));
        let with_ghost = fit_hyperbola(&samples).unwrap();
        assert_eq!(clean, with_ghost);
    }

    /// One wild sample beside eight clean ones: light, it barely moves
    /// the centre; as heavy as the rest together, it pulls the curve to
    /// itself.
    #[test]
    fn the_weights_decide_how_far_a_sample_pulls_the_fit() {
        let mut samples = sampled(RIG, 29_666, 100, 9, 100);
        samples[6].1 = 1.3;
        samples[6].2 = 1;
        let light = fit_hyperbola(&samples).unwrap();
        samples[6].2 = 800;
        let heavy = fit_hyperbola(&samples).unwrap();
        assert!((light.curve.centre - RIG.centre).abs() < 2.0, "{light:?}");
        assert!(
            (heavy.curve.centre - RIG.centre).abs()
                > 10.0 * (light.curve.centre - RIG.centre).abs(),
            "light {light:?}, heavy {heavy:?}"
        );
    }

    /// Samples from one wing alone fit well and put the centre past
    /// their end: the case a caller's bound on the centre exists for.
    #[test]
    fn samples_on_one_wing_extrapolate_the_centre_past_their_end() {
        let samples: Vec<(i32, f64, u32)> = (0..5)
            .map(|index| {
                let position = 29_900 + index * 100;
                (position, RIG.value_at(f64::from(position)), 100)
            })
            .collect();
        let fit = fit_hyperbola(&samples).unwrap();
        assert!(fit.curve.centre < 29_900.0, "{fit:?}");
        assert!(fit.r_squared > 0.99, "{fit:?}");
    }

    #[test]
    fn the_curve_is_its_minimum_at_the_centre_and_its_slope_far_from_it() {
        assert_eq!(RIG.value_at(RIG.centre), RIG.minimum);
        let far = RIG.value_at(RIG.centre + 1e6);
        assert!((far - RIG.slope * 1e6).abs() < 1e-3, "{far}");
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

        fn between(&mut self, low: f64, high: f64) -> f64 {
            low + (high - low) * self.unit()
        }
    }

    fn weighted_residual(samples: &[(i32, f64, u32)], curve: Hyperbola) -> f64 {
        samples
            .iter()
            .map(|(position, value, weight)| {
                let error = value - curve.value_at(f64::from(*position));
                f64::from(*weight) * error * error
            })
            .sum()
    }

    /// The smallest residual a scan of the parameters finds: 61 centres
    /// across the samples, the limiting V and 48 minimums from 0.03 to
    /// 30, and 60 slopes from 1e-4 to 1 a step.
    fn scanned_minimum(samples: &[(i32, f64, u32)]) -> f64 {
        let low = f64::from(samples.iter().map(|sample| sample.0).min().unwrap());
        let high = f64::from(samples.iter().map(|sample| sample.0).max().unwrap());
        let minimums: Vec<f64> = std::iter::once(0.0)
            .chain((0..48).map(|index| 10_f64.powf(-1.5 + f64::from(index) / 16.0)))
            .collect();
        let mut best = f64::INFINITY;
        for centre_index in 0..=60 {
            let centre = low + (high - low) * f64::from(centre_index) / 60.0;
            for minimum in &minimums {
                for slope_index in 0..60 {
                    let slope = 10_f64.powf(-4.0 + f64::from(slope_index) / 15.0);
                    let curve = Hyperbola {
                        centre,
                        minimum: *minimum,
                        slope,
                    };
                    best = best.min(weighted_residual(samples, curve));
                }
            }
        }
        best
    }

    /// Curves of every steepness and floor, 8 % noise, star counts that
    /// fall with defocus: the refinement ends at a residual no scan of
    /// the parameters beats, so the start never leaves it in a local
    /// minimum.
    #[test]
    fn the_refinement_reaches_the_least_squares_minimum_on_noisy_curves() {
        let mut noise = Noise(0x5eed_1187);
        for _ in 0..12 {
            let truth = Hyperbola {
                centre: 30_000.0,
                minimum: noise.between(0.5, 4.0),
                slope: noise.between(0.005, 0.2),
            };
            let half_width = (truth.minimum * 15_f64.sqrt() / truth.slope).ceil() as i32;
            let step = (half_width / 4).max(1);
            let offset = (noise.between(-0.4, 0.4) * f64::from(half_width)) as i32;
            let samples: Vec<(i32, f64, u32)> = (-4..=4)
                .map(|index| {
                    let position = 30_000 + offset + index * step;
                    let value = truth.value_at(f64::from(position));
                    let measured = value * (1.0 + 0.08 * (2.0 * noise.unit() - 1.0));
                    let stars = (300.0 * truth.minimum / value) as u32;
                    (position, measured, stars.max(1))
                })
                .collect();
            let fit = fit_hyperbola(&samples).unwrap();
            let fitted = weighted_residual(&samples, fit.curve);
            let scanned = scanned_minimum(&samples);
            assert!(
                fitted <= scanned * (1.0 + 1e-9) + 1e-12,
                "fit {fit:?} left {fitted}, a scan found {scanned}, samples {samples:?}"
            );
            assert!((0.0..=1.0).contains(&fit.r_squared), "{fit:?}");
        }
    }

    fn fit_residual(samples: &[(i32, f64, u32)], fit: HyperbolaFit) -> f64 {
        weighted_residual(samples, fit.curve)
    }

    /// The residual a compass search reaches from `from`: each of the
    /// centre, the minimum and the slope tried a step either way, the
    /// steps halved once none of the six lowers the residual. The curve
    /// depends on the minimum and the slope only through their squares,
    /// so the search needs no bound. Never above the residual at `from`.
    fn polished(samples: &[(i32, f64, u32)], from: Hyperbola) -> f64 {
        let low = f64::from(samples.iter().map(|sample| sample.0).min().unwrap());
        let high = f64::from(samples.iter().map(|sample| sample.0).max().unwrap());
        let curve = |[centre, minimum, slope]: [f64; 3]| Hyperbola {
            centre,
            minimum,
            slope,
        };
        let mut at = [from.centre, from.minimum, from.slope];
        let mut residual = weighted_residual(samples, from);
        let mut steps = [
            (high - low) / 20.0,
            from.minimum.max(0.1) / 20.0,
            from.slope / 20.0,
        ];
        for _ in 0..60 {
            let mut moved = true;
            while moved {
                moved = false;
                for axis in 0..3 {
                    for sign in [-1.0, 1.0] {
                        let mut next = at;
                        next[axis] += sign * steps[axis];
                        let next_residual = weighted_residual(samples, curve(next));
                        if next_residual < residual {
                            at = next;
                            residual = next_residual;
                            moved = true;
                        }
                    }
                }
            }
            steps = steps.map(|step| step / 2.0);
        }
        residual
    }

    /// Whether the fit sits at a minimum of its residual: a local search
    /// from it finds nothing lower beyond the refinement's own
    /// tolerance.
    fn at_a_minimum(samples: &[(i32, f64, u32)], fit: HyperbolaFit) -> bool {
        let fitted = fit_residual(samples, fit);
        polished(samples, fit.curve) >= fitted * (1.0 - 1e-6) - 1e-12
    }

    /// Curves whose floor is near zero or absent, sampled across focus
    /// with 15 % noise: fits that end at or beside the limiting V, where
    /// the minimum is held at zero, and the refinement still reaches a
    /// minimum of the residual rather than stopping against the bound.
    #[test]
    fn the_refinement_reaches_a_minimum_beside_the_limiting_v() {
        let mut noise = Noise(0x1675_0003);
        let mut limiting = 0;
        for _ in 0..60 {
            let truth = Hyperbola {
                centre: 30_000.0,
                minimum: noise.between(0.0, 0.3),
                slope: noise.between(0.01, 0.2),
            };
            let points = [5, 7, 9][(noise.unit() * 3.0) as usize];
            let step = (noise.between(0.5, 2.0) / truth.slope).round().max(1.0) as i32;
            let first =
                30_000 - (points / 2) * step + (noise.between(-1.0, 1.0) * f64::from(step)) as i32;
            let samples: Vec<(i32, f64, u32)> = (0..points)
                .map(|index| {
                    let position = first + index * step;
                    let value = truth.value_at(f64::from(position));
                    let measured = (value * (1.0 + 0.15 * (2.0 * noise.unit() - 1.0))).max(0.05);
                    (position, measured, (300.0 / (1.0 + value)) as u32)
                })
                .collect();
            let fit = fit_hyperbola(&samples).unwrap();
            if fit.curve.minimum == 0.0 {
                limiting += 1;
            }
            assert!(
                at_a_minimum(&samples, fit),
                "fit {fit:?} left {}, a local search from it found {}, samples {samples:?}",
                fit_residual(&samples, fit),
                polished(&samples, fit.curve)
            );
        }
        assert!(
            limiting >= 10,
            "only {limiting} of 60 fits were the limiting V"
        );
    }

    /// Five samples starting one to eight steps before focus, so most
    /// walk down one wing and stop short of it: the refinement reaches a
    /// minimum of the residual from wherever its starts leave it.
    #[test]
    fn the_refinement_reaches_a_minimum_on_one_wing() {
        let mut noise = Noise(0x1675_0001);
        let mut fitted = 0;
        for _ in 0..60 {
            let truth = Hyperbola {
                centre: 30_000.0,
                minimum: noise.between(0.5, 4.0),
                slope: noise.between(0.005, 0.3),
            };
            let step = (noise.between(0.3, 1.5) * truth.minimum / truth.slope)
                .round()
                .max(1.0) as i32;
            let first = 30_000
                - (noise.between(1.0, 9.0) as i32) * step
                - (noise.unit() * f64::from(step)) as i32;
            let samples: Vec<(i32, f64, u32)> = (0..5)
                .map(|index| {
                    let position = first + index * step;
                    let value = truth.value_at(f64::from(position));
                    let measured = value * (1.0 + 0.08 * (2.0 * noise.unit() - 1.0));
                    let stars = (300.0 * truth.minimum / value) as u32;
                    (position, measured, stars.max(1))
                })
                .collect();
            let Ok(fit) = fit_hyperbola(&samples) else {
                continue;
            };
            fitted += 1;
            assert!(
                at_a_minimum(&samples, fit),
                "fit {fit:?} left {}, a local search from it found {}, samples {samples:?}",
                fit_residual(&samples, fit),
                polished(&samples, fit.curve)
            );
        }
        assert!(fitted >= 50, "only {fitted} of 60 one-wing sweeps fitted");
    }

    /// One wing whose squared values place the closed-form start in the
    /// basin of a curve flattening to a constant; a scanned start finds
    /// the V that the samples walk down, centred past their end.
    #[test]
    fn a_scanned_start_finds_the_minimum_the_closed_form_start_misses() {
        let samples = [
            (29_981, 5.135, 68),
            (29_985, 5.204, 85),
            (29_989, 3.464, 112),
            (29_993, 2.766, 161),
            (29_997, 1.595, 248),
        ];
        let fit = fit_hyperbola(&samples).unwrap();
        assert!((fit.curve.centre - 30_003.74).abs() < 0.01, "{fit:?}");
        assert!(fit_residual(&samples, fit) < 51.0, "{fit:?}");
        assert!(at_a_minimum(&samples, fit), "{fit:?}");
    }
}
