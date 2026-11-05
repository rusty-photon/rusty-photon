//! The pure parts of `auto_focus`, the guiding train's PHD2-metric
//! V-curve sweep.
//!
//! The grid and its walk order, the lowest-sample choice, the
//! confirmation verdict, a fitted position rounded to a step, and the
//! confirmation-tolerance range the config block and the per-call
//! parameter share. The sweep itself, which reads the guider's metrics,
//! lives in `mcp/built_in/auto_focus.rs`; the fit is
//! `rp-focus-curve`'s.
//!
//! Behavioral contract: `docs/services/rp.md` → Compound Tools →
//! `auto_focus` Contract.

use std::cmp::Ordering;

/// Default confirmation tolerance.
///
/// Wide enough for seeing jitter, narrow enough that a fit landing off
/// the measured minimum (about twice the best sample) is rejected.
pub const DEFAULT_CONFIRMATION_TOLERANCE: f64 = 0.25;

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

/// Whether `value` is a usable confirmation tolerance: finite and
/// non-negative. Shared by the per-call and config-block validations.
#[must_use]
pub fn valid_confirmation_tolerance(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

/// The confirmation verdict on the sample measured at the fitted
/// position: accepted when there is one and it is no worse than
/// `lowest` (the best valid sweep sample) by more than `tolerance`.
#[must_use]
pub fn confirmation_accepted(sample: Option<f64>, lowest: f64, tolerance: f64) -> bool {
    sample.is_some_and(|value| value <= lowest * (1.0 + tolerance))
}

/// The lowest-valued sample; ties go to the better-supported one (the
/// larger weight) so the choice is deterministic and favours the better
/// measurement.
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
/// produce duplicate samples at a bound and distort the fit.
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

/// The focuser step nearest a fitted position: the hyperbola's centre.
#[must_use]
pub const fn nearest_step(position: f64) -> i32 {
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "`f64` to `i32` has no total spelling; `as` saturates at the i32 rails and maps NaN to 0, and the caller's sampled-range check rejects rail-hitting centres (and 0, unless the sampled positions straddle it — no in-tree path produces a NaN fit)"
    )]
    let step = position.round() as i32;
    step
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

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

    // ---- the confirmation and the lowest sample ----

    #[test]
    fn confirmation_verdict_needs_a_sample_inside_the_tolerance() {
        // Inside the tolerance.
        assert!(confirmation_accepted(Some(2.4), 2.0, 0.25));
        // Exactly at the bar passes.
        assert!(confirmation_accepted(Some(2.5), 2.0, 0.25));
        // Worse than the bar.
        assert!(!confirmation_accepted(Some(2.6), 2.0, 0.25));
        // No sample.
        assert!(!confirmation_accepted(None, 2.0, 0.25));
    }

    #[test]
    fn lowest_sample_breaks_ties_toward_the_larger_weight() {
        let samples = [(1, 4.0, 10), (2, 3.0, 5), (3, 3.0, 50), (4, 9.0, 400)];
        assert_eq!(lowest_sample(&samples), Some((3, 3.0, 50)));
        assert_eq!(lowest_sample(&[]), None);
    }

    #[test]
    fn confirmation_tolerance_is_finite_and_not_negative() {
        assert!(valid_confirmation_tolerance(0.0));
        assert!(valid_confirmation_tolerance(DEFAULT_CONFIRMATION_TOLERANCE));
        for bad in [-0.5, f64::NAN, f64::NEG_INFINITY, f64::INFINITY] {
            assert!(!valid_confirmation_tolerance(bad), "{bad} was accepted");
        }
    }

    #[test]
    fn nearest_step_rounds_to_the_closest_position() {
        assert_eq!(nearest_step(29_741.4), 29_741);
        assert_eq!(nearest_step(29_741.6), 29_742);
        assert_eq!(nearest_step(-3.5), -4);
    }
}
