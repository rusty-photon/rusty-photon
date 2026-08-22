//! `center_on_target`: iterative capture → plate-solve → sync → slew
//! compound tool.
//!
//! The driving logic — input validation, residual computation, sync-on-
//! iter-1 invariant, fail-fast loop body — is pure Rust and fully unit-
//! testable via the [`CaptureOps`], [`PlateSolveOps`], and [`MountOps`]
//! traits. The MCP wrapper in `mcp/built_in/center_on_target.rs`
//! provides concrete adapters that bind to the live capture path,
//! the in-process `plate_solve` handler, and the singular Alpaca
//! mount; tests substitute synthetic adapters that drive the loop with
//! deterministic per-iteration outcomes.
//!
//! Behavioral contract: `docs/services/rp.md` → Compound Tools →
//! `center_on_target` Contract.
//!
//! **BDD-author note.** `do_slew_blocking` now sizes its deadline from
//! the slew distance (`compute_slew_deadline`, §2.1): a small
//! inner-iteration corrective slew floors at `MIN_SLEW_DEADLINE` (30 s), so
//! it no longer approaches rmcp's 300 s MCP-transport keep-alive — only a
//! multi-degree slew gets a deadline near that bound. Keeping BDD canned
//! WCS values within ~2′ of any prior synced position still matters: it
//! keeps iter-1's sync + slew traversal small (and fast) even under heavy
//! CI load. See `services/rp/tests/features/center_on_target.feature` for
//! the worked numbers.
//!
//! Note: the closed-loop centering BDD hit *two* distinct hang classes
//! in May 2026, neither the slew/keep-alive race above:
//!
//! 1. `do_capture` looped forever on a **failed** `sky-survey-camera`
//!    exposure — its follow-mode mount read timed out under load, so
//!    `ImageReady` never went true and the capture poll spun
//!    indefinitely. Fixed in `mcp/internals.rs::do_capture` (treat
//!    `CameraState::Error` as terminal + a readout deadline).
//! 2. (Issue #319.) A *single* per-iteration mount read stalled and
//!    hung forever, because rp's Alpaca HTTP client had **no
//!    per-request timeout** — a device that accepts the connection but
//!    never answers (an overloaded `OmniSim` in CI) blocks the `await`
//!    indefinitely, and the blocking-op poll deadlines guard *loops*,
//!    not a single in-flight request. Fixed in `equipment::alpaca` with
//!    a per-request connect + read timeout, plus a transient-failure
//!    retry on the idempotent per-iteration reads (the `use_mount_hints`
//!    read in `plate_solve`, the `Slewing` poll).
//!
//! CI job `timeout-minutes` and the BDD client's `MCP_CALL_TIMEOUT` are
//! independent backstops, not the fix.

use async_trait::async_trait;
use serde::Serialize;
use std::time::Duration;
use thiserror::Error;
use tracing::debug;

pub use super::ops::CaptureOps;

#[derive(Debug, Clone)]
pub struct CenterOnTargetParams {
    /// Target right ascension in decimal hours, `[0, 24)`. Same unit
    /// as `slew` and `sync_mount` (Alpaca's `RightAscension`).
    pub ra: f64,
    /// Target declination in decimal degrees, `[-90, 90]`.
    pub dec: f64,
    /// Per-iteration exposure.
    pub duration: Duration,
    /// Convergence threshold on the great-circle residual between the
    /// solved center and `(ra, dec)`, in arcseconds. Must be positive.
    pub tolerance_arcsec: f64,
    /// Hard cap on the number of iterations. Must be positive and
    /// `<= MAX_ATTEMPTS`.
    pub max_attempts: usize,
}

/// Action taken on a single iteration *after* the per-iter solve.
/// Serialized as a kebab/snake-cased string in the result JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IterationAction {
    /// Iter 1 only: solved, synced, residual was outside tolerance,
    /// and the loop slewed back to the input target. Iter 1's record
    /// always uses `Sync` *unless* the residual was already inside
    /// tolerance after sync — in which case it collapses to
    /// `Converged` (sync still happened; the slew was skipped).
    Sync,
    /// Solved, residual outside tolerance, slewed back to the input
    /// target. Used on iter 2 onwards when the loop didn't converge.
    Slew,
    /// Solved, residual inside tolerance — terminal action. Always
    /// the last record; appears at most once.
    Converged,
}

#[derive(Debug, Clone, Serialize)]
pub struct IterationRecord {
    pub document_id: String,
    pub residual_arcsec: f64,
    pub solved_ra: f64,
    pub solved_dec: f64,
    pub action: IterationAction,
}

#[derive(Debug, Clone, Serialize)]
pub struct CenterOnTargetResult {
    pub final_error_arcsec: f64,
    pub attempts: usize,
    pub final_ra: f64,
    pub final_dec: f64,
    pub iterations: Vec<IterationRecord>,
}

/// Outcome of a single `plate_solve` call: the solved field-center in
/// decimal degrees. Mirrors the wrapper's success body, narrowed to
/// the two fields the centering loop actually consumes.
#[derive(Debug, Clone, Copy)]
pub struct SolveOutcome {
    pub ra_center_deg: f64,
    pub dec_center_deg: f64,
}

#[derive(Debug, Error)]
pub enum CenterOnTargetError {
    #[error("tolerance_arcsec must be positive (got {0})")]
    InvalidTolerance(f64),
    #[error("max_attempts must be positive (got {0})")]
    InvalidMaxAttempts(usize),
    #[error(
        "max_attempts={requested} exceeds the safety cap of {max} (lower max_attempts; the loop \
         is meant to converge in 2–4 iterations)"
    )]
    MaxAttemptsExceedsCap { requested: usize, max: usize },
    #[error("ra out of range: {0} (must be in [0.0, 24.0))")]
    InvalidRa(f64),
    #[error("dec out of range: {0} (must be in [-90.0, 90.0])")]
    InvalidDec(f64),
    #[error(
        "tolerance_not_reached: residual {last_residual_arcsec:.2}\" after \
         {attempts} attempts (tolerance {tolerance_arcsec:.2}\")"
    )]
    ToleranceNotReached {
        last_residual_arcsec: f64,
        tolerance_arcsec: f64,
        attempts: usize,
    },
    #[error("equipment error during centering: {0}")]
    Equipment(String),
}

#[async_trait]
pub trait PlateSolveOps {
    /// Plate-solve the named `document_id`. The MCP-side adapter sets
    /// `use_mount_hints: true` so the wrapper hint plumbing stays in
    /// the existing `plate_solve` Contract. Synthetic adapters
    /// ignore the `document_id` and return canned outcomes.
    async fn solve(&self, document_id: &str) -> Result<SolveOutcome, String>;
}

#[async_trait]
pub trait MountOps {
    /// Sync the mount's reported position to `(ra_deg, dec_deg)`.
    /// Inputs are decimal degrees here so the loop math is consistent;
    /// the live adapter converts to Alpaca's hours-for-RA at the call
    /// site (see `do_sync_mount` in `mcp/internals.rs`).
    async fn sync_to(&self, ra_deg: f64, dec_deg: f64) -> Result<(), String>;
    /// Slew the mount to the input target. Inputs are decimal hours
    /// for RA and decimal degrees for Dec, matching the primitive
    /// `slew` MCP tool's contract.
    async fn slew_to(&self, ra_hours: f64, dec_deg: f64) -> Result<(), String>;
}

/// Hard cap on `max_attempts`.
///
/// Plausible centering runs converge in 2–4 iterations; 50 is generous
/// enough that no real workflow trips the cap, low enough that a
/// misconfigured loop can't tie up the rig for hours. Mirrors
/// `auto_focus`'s `MAX_GRID_POINTS` guardrail.
pub const MAX_ATTEMPTS: usize = 50;

/// Reject centering parameters the loop cannot act on.
///
/// # Errors
///
/// Returns the `Invalid*` variant naming an out-of-range `ra` or `dec`,
/// a non-positive `tolerance_arcsec`, or a zero `max_attempts`, or
/// [`CenterOnTargetError::MaxAttemptsExceedsCap`] if `max_attempts`
/// exceeds [`MAX_ATTEMPTS`].
pub fn validate_params(params: &CenterOnTargetParams) -> Result<(), CenterOnTargetError> {
    if !(0.0..24.0).contains(&params.ra) {
        return Err(CenterOnTargetError::InvalidRa(params.ra));
    }
    if !(-90.0..=90.0).contains(&params.dec) {
        return Err(CenterOnTargetError::InvalidDec(params.dec));
    }
    if params.tolerance_arcsec <= 0.0 {
        return Err(CenterOnTargetError::InvalidTolerance(
            params.tolerance_arcsec,
        ));
    }
    if params.max_attempts == 0 {
        return Err(CenterOnTargetError::InvalidMaxAttempts(0));
    }
    if params.max_attempts > MAX_ATTEMPTS {
        return Err(CenterOnTargetError::MaxAttemptsExceedsCap {
            requested: params.max_attempts,
            max: MAX_ATTEMPTS,
        });
    }
    Ok(())
}

/// Great-circle separation between two equatorial directions, returned
/// in arcseconds. Inputs are decimal degrees for both RA and Dec.
///
/// Uses the haversine formula — numerically stable for the small
/// angles centering deals with (typically arcseconds-to-arcminutes),
/// and closed-form so no external dependency is needed.
#[must_use]
#[expect(
    clippy::suboptimal_flops,
    reason = "the haversine formula in its reference shape; fusing terms makes the code stop matching the algorithm it names"
)]
pub fn haversine_arcsec(ra1_deg: f64, dec1_deg: f64, ra2_deg: f64, dec2_deg: f64) -> f64 {
    let to_rad = std::f64::consts::PI / 180.0;
    let phi1 = dec1_deg * to_rad;
    let phi2 = dec2_deg * to_rad;
    let dphi = phi2 - phi1;
    let dlambda = (ra2_deg - ra1_deg) * to_rad;
    let a = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlambda / 2.0).sin().powi(2);
    // 2·atan2(√a, √(1−a)) is the haversine central angle (radians).
    let central_angle_rad = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    let central_angle_deg = central_angle_rad / to_rad;
    central_angle_deg * 3600.0
}

/// Drive the centering loop against the supplied capture / plate-solve
/// / mount adapters.
///
/// See `docs/services/rp.md` → `center_on_target` Contract for the
/// behavioral spec; this function is the reference implementation.
///
/// `emit_iteration` is called after each iteration's record is built
/// (whether the action is `Sync`, `Slew`, or `Converged`). The MCP
/// wrapper plumbs this into a `centering_iteration` event; tests can
/// no-op the closure.
///
/// # Errors
///
/// Returns [`validate_params`]'s rejection,
/// [`CenterOnTargetError::Equipment`] if any capture, plate solve,
/// sync, or slew fails, or [`CenterOnTargetError::ToleranceNotReached`]
/// if `max_attempts` iterations end without the residual dropping to
/// `tolerance_arcsec`.
pub async fn run_center_on_target<C, P, M>(
    capturer: &C,
    solver: &P,
    mounter: &M,
    params: CenterOnTargetParams,
    mut emit_iteration: impl FnMut(&IterationRecord),
) -> Result<CenterOnTargetResult, CenterOnTargetError>
where
    C: CaptureOps + Sync,
    P: PlateSolveOps + Sync,
    M: MountOps + Sync,
{
    validate_params(&params)?;

    let target_ra_deg = params.ra * 15.0;
    let target_dec_deg = params.dec;
    debug!(
        ra_hours = params.ra,
        dec_deg = params.dec,
        tolerance_arcsec = params.tolerance_arcsec,
        max_attempts = params.max_attempts,
        "center_on_target loop starting"
    );

    let mut iterations: Vec<IterationRecord> = Vec::new();
    let mut last_residual: f64 = f64::INFINITY;

    for iter in 0..params.max_attempts {
        let document_id = capturer
            .capture(params.duration)
            .await
            .map_err(CenterOnTargetError::Equipment)?;
        let outcome = solver
            .solve(&document_id)
            .await
            .map_err(CenterOnTargetError::Equipment)?;

        // Sync on iter 1 unconditionally — the first solve is the
        // absolute pointing reference. Subsequent iterations rely on
        // the mount honouring relative slews instead of re-syncing
        // (repeated syncs interact badly with model-building drivers).
        if iter == 0 {
            mounter
                .sync_to(outcome.ra_center_deg, outcome.dec_center_deg)
                .await
                .map_err(CenterOnTargetError::Equipment)?;
        }

        let residual_arcsec = haversine_arcsec(
            outcome.ra_center_deg,
            outcome.dec_center_deg,
            target_ra_deg,
            target_dec_deg,
        );
        last_residual = residual_arcsec;

        let action = if residual_arcsec <= params.tolerance_arcsec {
            IterationAction::Converged
        } else if iter == 0 {
            // Iter 1 already issued the sync; slew now to relocate.
            IterationAction::Sync
        } else {
            IterationAction::Slew
        };

        if matches!(action, IterationAction::Slew | IterationAction::Sync) {
            mounter
                .slew_to(params.ra, params.dec)
                .await
                .map_err(CenterOnTargetError::Equipment)?;
        }

        let record = IterationRecord {
            document_id,
            residual_arcsec,
            solved_ra: outcome.ra_center_deg,
            solved_dec: outcome.dec_center_deg,
            action,
        };
        emit_iteration(&record);
        iterations.push(record);

        if matches!(action, IterationAction::Converged) {
            return Ok(CenterOnTargetResult {
                final_error_arcsec: residual_arcsec,
                attempts: iterations.len(),
                final_ra: outcome.ra_center_deg,
                final_dec: outcome.dec_center_deg,
                iterations,
            });
        }
    }

    Err(CenterOnTargetError::ToleranceNotReached {
        last_residual_arcsec: last_residual,
        tolerance_arcsec: params.tolerance_arcsec,
        attempts: iterations.len(),
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ---- pure helpers ----

    fn baseline_params() -> CenterOnTargetParams {
        CenterOnTargetParams {
            ra: 10.6848 / 15.0, // ≈ 0.7123 hours, so 10.6848° in degrees
            dec: 41.269,
            duration: Duration::from_millis(100),
            tolerance_arcsec: 60.0,
            max_attempts: 5,
        }
    }

    #[test]
    fn validate_params_accepts_minimum_valid_input() {
        let p = CenterOnTargetParams {
            ra: 0.0,
            dec: 0.0,
            duration: Duration::from_millis(1),
            tolerance_arcsec: 1e-6,
            max_attempts: 1,
        };
        validate_params(&p).unwrap();
    }

    #[test]
    fn validate_params_rejects_zero_tolerance() {
        let mut p = baseline_params();
        p.tolerance_arcsec = 0.0;
        assert!(matches!(
            validate_params(&p),
            Err(CenterOnTargetError::InvalidTolerance(_))
        ));
    }

    #[test]
    fn validate_params_rejects_zero_max_attempts() {
        let mut p = baseline_params();
        p.max_attempts = 0;
        assert!(matches!(
            validate_params(&p),
            Err(CenterOnTargetError::InvalidMaxAttempts(0))
        ));
    }

    #[test]
    fn validate_params_rejects_max_attempts_above_cap() {
        let mut p = baseline_params();
        p.max_attempts = MAX_ATTEMPTS + 1;
        match validate_params(&p) {
            Err(CenterOnTargetError::MaxAttemptsExceedsCap { requested, max }) => {
                assert_eq!(requested, MAX_ATTEMPTS + 1);
                assert_eq!(max, MAX_ATTEMPTS);
            }
            other => panic!("expected MaxAttemptsExceedsCap, got {other:?}"),
        }
    }

    #[test]
    fn validate_params_rejects_ra_at_or_above_24() {
        let mut p = baseline_params();
        p.ra = 24.0;
        assert!(matches!(
            validate_params(&p),
            Err(CenterOnTargetError::InvalidRa(_))
        ));
    }

    #[test]
    fn validate_params_rejects_dec_above_90() {
        let mut p = baseline_params();
        p.dec = 91.0;
        assert!(matches!(
            validate_params(&p),
            Err(CenterOnTargetError::InvalidDec(_))
        ));
    }

    // ---- haversine sanity ----

    #[test]
    fn haversine_zero_separation_is_zero() {
        let arcsec = haversine_arcsec(160.27, 41.269, 160.27, 41.269);
        assert!(
            arcsec < 1e-6,
            "expected 0 arcsec for identical inputs, got {arcsec}"
        );
    }

    #[test]
    fn haversine_one_arcsec_at_equator() {
        // 1 arcsec along RA at the equator = 1 arcsec separation. The
        // 1/3600 degree shift is exact at dec=0 because cos(0)=1 in
        // the haversine.
        let arcsec = haversine_arcsec(0.0, 0.0, 1.0 / 3600.0, 0.0);
        assert!(
            (arcsec - 1.0).abs() < 1e-6,
            "expected 1 arcsec, got {arcsec}"
        );
    }

    #[test]
    fn haversine_one_arcmin_along_dec() {
        // 1 arcmin along Dec = 60 arcsec, regardless of RA.
        let arcsec = haversine_arcsec(160.27, 41.0, 160.27, 41.0 + 1.0 / 60.0);
        assert!(
            (arcsec - 60.0).abs() < 1e-6,
            "expected 60 arcsec, got {arcsec}"
        );
    }

    /// Pin the wrap-safe property of haversine across the 0°/360°
    /// boundary. Future readers (and reviewers) should not have to
    /// re-derive that the `sin²(Δλ/2)` kernel has period 360° in
    /// `Δλ` and therefore needs no manual `Δλ ∈ [-180°, 180°]`
    /// normalization. Same minimal angular separation (~72″ between
    /// 359.99° and 0.01°) regardless of which side we put first.
    #[test]
    fn haversine_is_wrap_safe_across_360() {
        let across_zero = haversine_arcsec(359.99, 0.0, 0.01, 0.0);
        let no_wrap = haversine_arcsec(0.0, 0.0, 0.02, 0.0);
        assert!(
            (across_zero - no_wrap).abs() < 1e-6,
            "wrap mismatch: across-zero {across_zero} vs no-wrap {no_wrap}"
        );
        assert!(
            (across_zero - 72.0).abs() < 0.05,
            "expected ~72″ separation, got {across_zero}"
        );
    }

    // ---- end-to-end run_center_on_target over synthetic adapters ----

    struct StubCapturer {
        counter: Mutex<u64>,
    }

    #[async_trait]
    impl CaptureOps for StubCapturer {
        async fn capture(&self, _: Duration) -> Result<String, String> {
            let mut c = self.counter.lock().unwrap();
            *c += 1;
            Ok(format!("doc-{:04}", *c))
        }
    }

    /// Plate solver that walks a queue of canned outcomes; once the
    /// queue is exhausted, keeps returning the last entry. Mirrors
    /// the BDD `Sequence` stub.
    struct StubSolver {
        queue: Mutex<Vec<SolveOutcome>>,
        cursor: Mutex<usize>,
    }

    impl StubSolver {
        fn new(outcomes: Vec<SolveOutcome>) -> Self {
            assert!(
                !outcomes.is_empty(),
                "StubSolver needs at least one outcome"
            );
            Self {
                queue: Mutex::new(outcomes),
                cursor: Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl PlateSolveOps for StubSolver {
        async fn solve(&self, _: &str) -> Result<SolveOutcome, String> {
            let queue = self.queue.lock().unwrap();
            let mut cursor = self.cursor.lock().unwrap();
            let idx = (*cursor).min(queue.len() - 1);
            *cursor += 1;
            Ok(queue[idx])
        }
    }

    /// Mount that records every `sync_to` / `slew_to` call. Tests inspect
    /// the log to verify the sync-on-iter-1-only invariant.
    #[derive(Default)]
    struct StubMounter {
        sync_calls: Mutex<Vec<(f64, f64)>>,
        slew_calls: Mutex<Vec<(f64, f64)>>,
    }

    #[async_trait]
    impl MountOps for StubMounter {
        async fn sync_to(&self, ra_deg: f64, dec_deg: f64) -> Result<(), String> {
            self.sync_calls.lock().unwrap().push((ra_deg, dec_deg));
            Ok(())
        }
        async fn slew_to(&self, ra_hours: f64, dec_deg: f64) -> Result<(), String> {
            self.slew_calls.lock().unwrap().push((ra_hours, dec_deg));
            Ok(())
        }
    }

    #[tokio::test]
    async fn run_converges_on_iteration_1() {
        let cap = StubCapturer {
            counter: Mutex::new(0),
        };
        let solver = StubSolver::new(vec![SolveOutcome {
            ra_center_deg: 10.6848,
            dec_center_deg: 41.269,
        }]);
        let mounter = StubMounter::default();
        let result = run_center_on_target(&cap, &solver, &mounter, baseline_params(), |_| {})
            .await
            .unwrap();
        assert_eq!(result.attempts, 1);
        assert_eq!(result.iterations.len(), 1);
        assert_eq!(result.iterations[0].action, IterationAction::Converged);
        // sync fires unconditionally on iter 1 even when converged.
        assert_eq!(mounter.sync_calls.lock().unwrap().len(), 1);
        // converged ⇒ no slew issued for iter 1.
        assert_eq!(mounter.slew_calls.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn run_converges_on_iteration_2_records_sync_then_converged() {
        let cap = StubCapturer {
            counter: Mutex::new(0),
        };
        let solver = StubSolver::new(vec![
            // iter 1: 0.04° (≈ 144 arcsec) off in Dec — outside the
            // 60-arcsec tolerance.
            SolveOutcome {
                ra_center_deg: 10.6848,
                dec_center_deg: 41.229,
            },
            // iter 2: spot on.
            SolveOutcome {
                ra_center_deg: 10.6848,
                dec_center_deg: 41.269,
            },
        ]);
        let mounter = StubMounter::default();
        let result = run_center_on_target(&cap, &solver, &mounter, baseline_params(), |_| {})
            .await
            .unwrap();
        assert_eq!(result.attempts, 2);
        assert_eq!(result.iterations[0].action, IterationAction::Sync);
        assert_eq!(result.iterations[1].action, IterationAction::Converged);
        // Sync-on-iter-1-only invariant: exactly one sync, even
        // though the loop ran twice.
        assert_eq!(mounter.sync_calls.lock().unwrap().len(), 1);
        // One slew on iter 1 (after sync, residual outside tolerance);
        // no slew on iter 2 (converged action skips slew).
        assert_eq!(mounter.slew_calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn run_errors_tolerance_not_reached_after_max_attempts() {
        let cap = StubCapturer {
            counter: Mutex::new(0),
        };
        // Solver always returns the same off-target point — residual
        // never improves.
        let solver = StubSolver::new(vec![SolveOutcome {
            ra_center_deg: 9.9,
            dec_center_deg: 41.0,
        }]);
        let mounter = StubMounter::default();
        let mut params = baseline_params();
        params.max_attempts = 3;
        let err = run_center_on_target(&cap, &solver, &mounter, params, |_| {}).await;
        match err {
            Err(CenterOnTargetError::ToleranceNotReached { attempts, .. }) => {
                assert_eq!(attempts, 3);
            }
            other => panic!("expected ToleranceNotReached, got {other:?}"),
        }
        // The loop ran 3 iterations, each ending with a slew (since
        // residual stayed > tolerance every time).
        assert_eq!(mounter.slew_calls.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn run_propagates_capture_error() {
        struct FailingCapturer;
        #[async_trait]
        impl CaptureOps for FailingCapturer {
            async fn capture(&self, _: Duration) -> Result<String, String> {
                Err("camera offline".to_string())
            }
        }
        let solver = StubSolver::new(vec![SolveOutcome {
            ra_center_deg: 10.6848,
            dec_center_deg: 41.269,
        }]);
        let mounter = StubMounter::default();
        let err = run_center_on_target(
            &FailingCapturer,
            &solver,
            &mounter,
            baseline_params(),
            |_| {},
        )
        .await;
        match err {
            Err(CenterOnTargetError::Equipment(msg)) => assert!(
                msg.contains("camera offline"),
                "expected propagated message, got: {msg}"
            ),
            other => panic!("expected Equipment, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_propagates_solve_error() {
        struct FailingSolver;
        #[async_trait]
        impl PlateSolveOps for FailingSolver {
            async fn solve(&self, _: &str) -> Result<SolveOutcome, String> {
                Err("plate_solve: solve_failed: ASTAP exited with code 1".to_string())
            }
        }
        let cap = StubCapturer {
            counter: Mutex::new(0),
        };
        let mounter = StubMounter::default();
        let err =
            run_center_on_target(&cap, &FailingSolver, &mounter, baseline_params(), |_| {}).await;
        match err {
            Err(CenterOnTargetError::Equipment(msg)) => assert!(
                msg.contains("solve_failed"),
                "expected propagated message, got: {msg}"
            ),
            other => panic!("expected Equipment, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_propagates_slew_error() {
        struct FailingSlewMounter;
        #[async_trait]
        impl MountOps for FailingSlewMounter {
            async fn sync_to(&self, _: f64, _: f64) -> Result<(), String> {
                Ok(())
            }
            async fn slew_to(&self, _: f64, _: f64) -> Result<(), String> {
                Err("failed to slew: tracking is off".to_string())
            }
        }
        let cap = StubCapturer {
            counter: Mutex::new(0),
        };
        let solver = StubSolver::new(vec![SolveOutcome {
            // Solved point far from target → slew fires after sync.
            ra_center_deg: 9.0,
            dec_center_deg: 30.0,
        }]);
        let err = run_center_on_target(
            &cap,
            &solver,
            &FailingSlewMounter,
            baseline_params(),
            |_| {},
        )
        .await;
        match err {
            Err(CenterOnTargetError::Equipment(msg)) => assert!(
                msg.contains("tracking is off"),
                "expected propagated message, got: {msg}"
            ),
            other => panic!("expected Equipment, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_invokes_emit_iteration_callback_per_record() {
        let cap = StubCapturer {
            counter: Mutex::new(0),
        };
        let solver = StubSolver::new(vec![
            SolveOutcome {
                ra_center_deg: 10.6848,
                dec_center_deg: 41.229,
            },
            SolveOutcome {
                ra_center_deg: 10.6848,
                dec_center_deg: 41.269,
            },
        ]);
        let mounter = StubMounter::default();
        let actions = std::sync::Mutex::new(Vec::new());
        let result = run_center_on_target(&cap, &solver, &mounter, baseline_params(), |rec| {
            actions.lock().unwrap().push(rec.action);
        })
        .await
        .unwrap();
        let logged = actions.into_inner().unwrap();
        assert_eq!(logged.len(), 2);
        assert_eq!(
            logged,
            vec![IterationAction::Sync, IterationAction::Converged]
        );
        assert_eq!(result.iterations.len(), 2);
    }
}
