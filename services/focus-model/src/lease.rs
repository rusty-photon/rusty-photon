//! A training run's train lease (docs/services/focus-model.md § Training
//! runs on a guiding train).
//!
//! A training run on the guiding train captures through the PHD2 camera
//! facade under `rp`'s train lease, which holds guiding, mount motion
//! and every other client's work on the train's light path off the run
//! (rp.md § Train Lease). This module names the lease, sizes it from
//! the sweep, and tells `rp`'s refusal of a lease that is no longer
//! held from any other failure.

use std::time::Duration;

use crate::config::TrainConfig;
use crate::error::{FocusModelError, Result};
use crate::sizing::SweepPlan;

/// What a frame costs beyond its exposure: PHD2's readout and save,
/// the facade's read and download, `rp`'s FITS write and document, and
/// `measure_stars` on the frame.
pub const FRAME_COST: Duration = Duration::from_secs(2);

/// What one focuser move costs: a sample's short hop, with `rp`'s
/// settle and its read-back.
pub const MOVE_COST: Duration = Duration::from_secs(2);

/// The longest lease `rp` grants.
pub const LEASE_CEILING: Duration = Duration::from_secs(600);

/// A fresh lease id: `focus-model-` and 32 hex digits. `rp` refuses an
/// id it has seen before, so every lease names a new one.
#[must_use]
pub fn fresh_id() -> String {
    format!("focus-model-{}", uuid::Uuid::new_v4().simple())
}

/// The `max_duration` a training run asks its lease for: half again the
/// time its sweep needs, at most [`LEASE_CEILING`].
///
/// # Errors
///
/// Returns [`FocusModelError::Workflow`], naming the numbers, when the
/// sweep needs longer than the longest lease `rp` grants: a run that
/// cannot finish inside one is refused before anything moves.
pub fn lease_duration(train_id: &str, train: &TrainConfig, plan: &SweepPlan) -> Result<Duration> {
    let attempts = train.max_attempts.get();
    let frames_per_step = train.frames_per_step.get();
    // Each attempt's samples and its confirmation frame.
    let frames = attempts.saturating_mul(
        plan.points
            .saturating_mul(frames_per_step)
            .saturating_add(1),
    );
    // A move to each sample and to the vertex, per attempt; the move to
    // the predicted start and the put-back, once.
    let moves = attempts
        .saturating_mul(plan.points.saturating_add(1))
        .saturating_add(2);
    let estimate = whole_seconds(
        train
            .duration
            .saturating_add(FRAME_COST)
            .saturating_mul(frames)
            .saturating_add(MOVE_COST.saturating_mul(moves)),
    );
    if estimate > LEASE_CEILING {
        return Err(FocusModelError::Workflow(format!(
            "a training run on guiding train '{train_id}' needs about {} under rp's train lease, \
             longer than the {} a lease may last: {attempts} attempts of {} points × \
             {frames_per_step} frames + 1 at {} + {} a frame, and {moves} moves at {}; lower \
             max_attempts, frames_per_step or duration in trains.{train_id}",
            humantime::format_duration(estimate),
            humantime::format_duration(LEASE_CEILING),
            plan.points,
            humantime::format_duration(train.duration),
            humantime::format_duration(FRAME_COST),
            humantime::format_duration(MOVE_COST),
        )));
    }
    // The half again covers what the estimate leaves out: a filter
    // change, a rejected confirmation's fallback frame, a slower sample.
    let asked = estimate
        .saturating_mul(3)
        .checked_div(2)
        .unwrap_or(LEASE_CEILING);
    Ok(whole_seconds(asked).min(LEASE_CEILING))
}

/// `duration` rounded up to a whole second, the precision `rp` reads a
/// humantime `max_duration` at in practice.
fn whole_seconds(duration: Duration) -> Duration {
    Duration::from_secs(
        duration
            .as_secs()
            .saturating_add(u64::from(duration.subsec_nanos() > 0)),
    )
}

/// Whether `error` is `rp` refusing a call under the lease `lease_id`.
///
/// `rp` refuses a call under a lease it no longer holds: one released,
/// ended idle, at its `max_duration`, on the safety transition, or with
/// its facade lease lost. rp.md § Train Lease names the phrase.
#[must_use]
pub fn is_not_held(error: &FocusModelError, lease_id: &str) -> bool {
    matches!(error, FocusModelError::ToolCall(message)
        if message.contains(&format!("lease '{lease_id}' is not held")))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::sizing::SweepSource;

    fn train(json: &str) -> TrainConfig {
        serde_json::from_str(json).unwrap()
    }

    fn plan(points: u32) -> SweepPlan {
        SweepPlan {
            step_size: 17,
            half_width: 68,
            points,
            source: SweepSource::Derived,
            end_ratio: 4.0,
            wavelength_nm: 550.0,
            cfz_steps: None,
            hfr_focus: None,
            predicted_slope: None,
        }
    }

    #[test]
    fn a_lease_is_half_again_the_frames_and_moves_its_sweep_needs() {
        // 2 attempts × (9 + 1) frames at 3 s + 2 s, and 2 × 10 + 2
        // moves at 2 s: 100 s + 44 s = 144 s, asked for as 216 s.
        let asked = lease_duration("guide", &train(r#"{"duration": "3s"}"#), &plan(9)).unwrap();
        assert_eq!(asked, Duration::from_secs(216));
    }

    #[test]
    fn every_frame_of_a_step_is_counted() {
        // 1 attempt × (5 × 3 + 1) frames at 100 ms + 2 s = 33.6 s, and
        // 8 moves at 2 s = 16 s: 49.6 s, rounded up to 50 s, then 75 s.
        let knobs = r#"{"duration": "100ms", "frames_per_step": 3, "max_attempts": 1}"#;
        let asked = lease_duration("guide", &train(knobs), &plan(5)).unwrap();
        assert_eq!(asked, Duration::from_secs(75));
    }

    #[test]
    fn the_headroom_stops_at_the_longest_lease() {
        // 3 attempts × (9 × 2 + 1) frames at 5 s + 2 s = 399 s, and 32
        // moves at 2 s = 64 s: 463 s fits, and half again would not.
        let knobs = r#"{"duration": "5s", "frames_per_step": 2, "max_attempts": 3}"#;
        let asked = lease_duration("guide", &train(knobs), &plan(9)).unwrap();
        assert_eq!(asked, LEASE_CEILING);
    }

    #[test]
    fn a_run_longer_than_a_lease_is_refused_naming_the_numbers() {
        let knobs = r#"{"duration": "5s", "frames_per_step": 3, "max_attempts": 3}"#;
        let err = lease_duration("guide", &train(knobs), &plan(9)).unwrap_err();
        assert_eq!(
            err.tool_message(),
            "a training run on guiding train 'guide' needs about 10m 52s under rp's train lease, \
             longer than the 10m a lease may last: 3 attempts of 9 points × 3 frames + 1 at 5s \
             + 2s a frame, and 32 moves at 2s; lower max_attempts, frames_per_step or duration \
             in trains.guide"
        );
    }

    #[test]
    fn a_fresh_id_is_new_each_time_and_fits_rps_rule() {
        let a = fresh_id();
        let b = fresh_id();
        assert_ne!(a, b);
        assert!(a.starts_with("focus-model-"), "{a}");
        assert_eq!(a.len(), "focus-model-".len() + 32, "{a}");
        assert!(a.bytes().all(|c| c.is_ascii_graphic()), "{a}");
    }

    #[test]
    fn only_rps_refusal_of_this_lease_reads_as_not_held() {
        let refused = FocusModelError::ToolCall(
            "move_focuser: lease 'focus-model-a' is not held: it was released".to_owned(),
        );
        assert!(is_not_held(&refused, "focus-model-a"));
        assert!(!is_not_held(&refused, "focus-model-b"));
        let other = FocusModelError::ToolCall("move_focuser: the focuser is not connected".into());
        assert!(!is_not_held(&other, "focus-model-a"));
        let cancelled = FocusModelError::Cancelled("lease 'focus-model-a' is not held".into());
        assert!(!is_not_held(&cancelled, "focus-model-a"));
    }
}
