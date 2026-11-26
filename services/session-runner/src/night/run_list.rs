//! The night's runs (design § Night Schedule → A night, step 6, and
//! § Restarts, case 6).
//!
//! Each configured run starts in turn under the session id
//! `night-<date>-<n>`, once the one before it has ended, however it
//! ended. Meanwhile the covers dusk could not open are tried again every
//! 5 minutes.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use super::clock::NightWindow;
use super::dawn::later;
use super::dusk::COVER_RETRY;
use super::schedule::{until, RunSlot, Schedule, INITIAL_BACKOFF, MAX_BACKOFF, TWILIGHT_RETRY};
use super::state::NightState;
use crate::engine::{Clock, ToolClient};
use crate::runs::{RunManifest, StartRefused};

/// How often the runs phase looks whether the run that holds the slot
/// has ended.
const RUN_POLL: Duration = Duration::from_secs(5);

/// The session id of the night's run at `index`.
fn session_id(night: &NightState, index: usize) -> String {
    format!("night-{}-{index}", night.night)
}

impl<C: ToolClient + Sync, K: Clock + Sync, S: RunSlot> Schedule<'_, C, K, S> {
    /// The runs phase until the list is done, or until dawn: start each
    /// run in turn and wait for it to end, and try the covers still to
    /// open again meanwhile. Returns how long until the next pass.
    pub(super) async fn runs(&mut self, ahead: NightWindow, mut state: NightState) -> Duration {
        let dawn = ahead.dawn;
        if !self.memory.runs_taken_up {
            self.memory.runs_taken_up = true;
            self.take_up_runs(&mut state).await;
        }
        let mut backoff = INITIAL_BACKOFF;
        loop {
            let now = self.clock.now();
            if now >= dawn {
                return Duration::ZERO;
            }
            self.retry_covers(&mut state, dawn).await;
            if let Some(run_id) = self.status.run() {
                if self.slot.active_run().as_deref() == Some(run_id.as_str()) {
                    self.clock.sleep(RUN_POLL.min(until(now, dawn))).await;
                    continue;
                }
                debug!(run_id, "the night's run ended");
                self.status.set_run(None);
                state.next_run = state.next_run.saturating_add(1);
                self.save(&state).await;
            }
            if let Some(active) = self.slot.active_run() {
                if self.memory.logged_wait_for.as_deref() != Some(active.as_str()) {
                    info!(run_id = %active, "waiting for the active run to end before the night's next run");
                    self.memory.logged_wait_for = Some(active);
                }
                self.clock.sleep(RUN_POLL.min(until(now, dawn))).await;
                continue;
            }
            let Some(run) = self.night.runs.get(state.next_run) else {
                // Every run was refused while the schedule held the slot.
                if self.slot.held() {
                    self.slot.release();
                }
                if !self.memory.logged_runs_done {
                    info!(night = %state.night, "the night's runs are done; the schedule waits for dawn");
                    self.memory.logged_runs_done = true;
                }
                let mut wait = until(now, dawn).min(TWILIGHT_RETRY);
                if let (false, Some(next)) = (
                    state.covers_to_open.is_empty(),
                    self.memory.next_cover_retry,
                ) {
                    wait = wait.min(until(now, next));
                }
                return wait;
            };
            let session_id = session_id(&state, state.next_run);
            let from_night = self.slot.held();
            match self.slot.start_run(run, &session_id, from_night).await {
                Ok(run_id) => {
                    info!(run_id, session_id, workflow = %run.workflow, "started the night's run");
                    self.status.set_run(Some(run_id));
                    backoff = INITIAL_BACKOFF;
                }
                Err(StartRefused::Unreachable(e)) => {
                    debug!(session_id, error = %e, wait = ?backoff, "rp unreachable; starting the night's run again");
                    self.clock.sleep(backoff.min(until(now, dawn))).await;
                    backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
                }
                // A run an operator started took the slot first.
                Err(StartRefused::Taken(_)) => {
                    self.clock.sleep(RUN_POLL.min(until(now, dawn))).await;
                }
                Err(refused) => {
                    warn!(session_id, workflow = %run.workflow, error = %refused, "the night's run was refused; the next starts");
                    if from_night && matches!(refused, StartRefused::NotRecorded { .. }) {
                        // The run took the hold with it and never started.
                        self.slot.hold();
                    }
                    state.next_run = state.next_run.saturating_add(1);
                    self.save(&state).await;
                }
            }
        }
    }

    /// Take up the night's runs where the previous process left them. A
    /// run of this night the self-resume picked up is waited for, and the
    /// night goes on from the run after it, whatever `night.json`'s index
    /// says; a run's manifest left in place (`resume_on_start: false`)
    /// moves the night past that run.
    async fn take_up_runs(&self, state: &mut NightState) {
        let prefix = format!("night-{}-", state.night);
        let index = |session_id: &str| {
            session_id
                .strip_prefix(prefix.as_str())
                .and_then(|n| n.parse::<usize>().ok())
        };
        let known = self
            .slot
            .runs()
            .into_iter()
            .filter_map(|run| Some((index(&run.session_id)?, run)))
            .max_by_key(|(n, _)| *n);
        if let Some((n, run)) = &known {
            if run.active {
                info!(run_id = %run.run_id, session_id = %run.session_id, "the night's run the previous process left goes on; the schedule waits for it");
                state.next_run = *n;
                self.save(state).await;
                self.status.set_run(Some(run.run_id.clone()));
                return;
            }
        }
        let left = RunManifest::scan(self.state_dir)
            .await
            .iter()
            .filter_map(|m| index(&m.session_id))
            .max();
        if let Some(last) = known.map(|(n, _)| n).max(left) {
            let next = state.next_run.max(last.saturating_add(1));
            if next != state.next_run {
                debug!(
                    next,
                    "the night goes on past the run the previous process left"
                );
                state.next_run = next;
                self.save(state).await;
            }
        }
    }

    /// Try again each cover dusk could not open, every 5 minutes, while
    /// `get_cover_state` does not read it open: until dawn, or until `rp`
    /// reports the cover moved, since a run that closed it for darks or
    /// panel flats keeps it closed.
    async fn retry_covers(&mut self, state: &mut NightState, dawn: DateTime<Utc>) {
        if state.covers_to_open.is_empty() {
            return;
        }
        let now = self.clock.now();
        if self.memory.next_cover_retry.is_some_and(|next| now < next) {
            return;
        }
        self.memory.next_cover_retry = Some(later(now, COVER_RETRY));
        let mut still = Vec::new();
        for calibrator in &state.covers_to_open {
            if self.signals.cover_moved(calibrator) {
                info!(
                    calibrator,
                    "rp reports the cover moved; the schedule stops opening it"
                );
                continue;
            }
            let open = self
                .call(
                    "get_cover_state",
                    json!({ "calibrator_id": calibrator }),
                    dawn,
                )
                .await
                .is_ok_and(|s| s.get("cover_state").and_then(Value::as_str) == Some("Open"));
            if open {
                info!(calibrator, "the cover reads open");
                continue;
            }
            match self
                .call("open_cover", json!({ "calibrator_id": calibrator }), dawn)
                .await
            {
                Ok(_) => info!(calibrator, "opened the cover on a retry"),
                Err(e) => {
                    debug!(calibrator, error = %e, "the cover still does not open");
                    still.push(calibrator.clone());
                }
            }
        }
        if still != state.covers_to_open {
            state.covers_to_open = still;
            self.save(state).await;
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::atomic::Ordering;

    use chrono::TimeDelta;

    use super::super::state::Phase;
    use super::super::test_rig::{at, date, done_up_to, Harness};
    use super::*;
    use crate::engine::EngineEvent;
    use crate::night::schedule::KnownRun;

    fn state(h: &Harness) -> NightState {
        h.status.snapshot().unwrap()
    }

    fn sessions(indices: &[usize]) -> Vec<String> {
        indices
            .iter()
            .map(|n| format!("night-2026-11-23-{n}"))
            .collect()
    }

    /// night.json as a previous process left it in the runs phase of
    /// night 23, at `next_run`.
    fn left_in_runs(h: &Harness, next_run: usize) {
        let mut runs = done_up_to(23);
        runs.phase = Phase::Runs;
        runs.next_run = next_run;
        h.status.publish(runs);
    }

    #[tokio::test]
    async fn the_runs_start_in_turn_and_the_first_takes_the_slot_from_dusk() {
        let h = Harness::with_runs(at(24, 1), &["sky_flat", "deep_sky", "deep_sky"]);
        let mut schedule = h.schedule();

        schedule.tick().await;
        assert!(
            h.slot.held.load(Ordering::SeqCst),
            "held until the first run"
        );
        schedule.tick().await;

        assert_eq!(
            *h.slot.starts.lock().unwrap(),
            [
                (sessions(&[0])[0].clone(), true),
                (sessions(&[1])[0].clone(), false),
                (sessions(&[2])[0].clone(), false),
            ]
        );
        assert_eq!(state(&h).next_run, 3);
        assert!(!h.slot.held.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_run_starts_once_the_one_before_it_has_ended() {
        let h = Harness::with_runs(at(24, 1), &["sky_flat", "deep_sky"]);
        *h.slot.run_reads.lock().unwrap() = Some(3);
        let mut schedule = h.schedule();

        schedule.tick().await;
        schedule.tick().await;

        assert_eq!(h.slot.sessions(), sessions(&[0, 1]));
        assert!(h.clock.now() > at(24, 1), "the first run was waited for");
        assert_eq!(h.status.run(), None, "both have ended");
    }

    #[tokio::test]
    async fn a_refused_run_is_skipped_and_the_next_starts() {
        let h = Harness::with_runs(at(24, 1), &["sky_flat", "no_catalog", "deep_sky"]);
        h.slot.refused.lock().unwrap().insert("no_catalog".into());
        let mut schedule = h.schedule();

        schedule.tick().await;
        schedule.tick().await;

        assert_eq!(h.slot.sessions(), sessions(&[0, 2]));
        assert_eq!(state(&h).next_run, 3);
    }

    #[tokio::test]
    async fn every_run_refused_lets_the_slot_go() {
        let h = Harness::with_runs(at(24, 1), &["no_catalog"]);
        h.slot.refused.lock().unwrap().insert("no_catalog".into());
        let mut schedule = h.schedule();

        schedule.tick().await;
        let wait = schedule.tick().await;

        assert_eq!(h.slot.sessions(), Vec::<String>::new());
        assert!(!h.slot.held.load(Ordering::SeqCst));
        assert_eq!(wait, TWILIGHT_RETRY, "waits for dawn, an hour at a time");
    }

    #[tokio::test]
    async fn a_start_rp_cannot_reach_is_retried_backing_off() {
        let h = Harness::with_runs(at(24, 1), &["deep_sky"]);
        h.slot.unreachable_starts.store(3, Ordering::SeqCst);
        let mut schedule = h.schedule();

        schedule.tick().await;
        let before = h.clock.now();
        schedule.tick().await;

        assert_eq!(h.slot.sessions(), sessions(&[0]));
        assert_eq!(h.clock.now() - before, TimeDelta::seconds(1 + 2 + 4));
    }

    #[tokio::test]
    async fn an_operators_run_is_waited_for_before_the_nights_next_run() {
        let h = Harness::with_runs(at(24, 1), &["deep_sky"]);
        left_in_runs(&h, 0);
        h.slot.operator_run("operator-run", 3);
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(
            *h.slot.starts.lock().unwrap(),
            [(sessions(&[0])[0].clone(), false)]
        );
        assert!(h.clock.now() > at(24, 1));
    }

    #[tokio::test]
    async fn a_cover_that_failed_to_open_is_tried_again_every_five_minutes() {
        let h = Harness::with_runs(at(24, 1), &[]);
        h.rp.rig.lock().unwrap().opens_fail = true;
        let mut schedule = h.schedule();

        schedule.tick().await;
        let wait = schedule.tick().await;
        assert_eq!(wait, COVER_RETRY, "until the cover's retry");
        assert_eq!(h.rp.called("open_cover"), 1);

        h.clock.sleep(wait).await;
        schedule.tick().await;
        assert_eq!(h.rp.called("open_cover"), 2);
        assert_eq!(state(&h).covers_to_open, ["flat-panel"]);

        h.rp.rig.lock().unwrap().opens_fail = false;
        h.clock.sleep(COVER_RETRY).await;
        schedule.tick().await;
        assert_eq!(h.rp.called("open_cover"), 3);
        assert_eq!(state(&h).covers_to_open, Vec::<String>::new());
        assert_eq!(h.rp.rig.lock().unwrap().covers["flat-panel"], "Open");
    }

    #[tokio::test]
    async fn a_cover_that_reads_open_is_not_opened_again() {
        let h = Harness::with_runs(at(24, 1), &[]);
        h.rp.rig.lock().unwrap().opens_fail = true;
        let mut schedule = h.schedule();

        schedule.tick().await;
        h.rp.rig
            .lock()
            .unwrap()
            .covers
            .insert("flat-panel".into(), "Open".into());
        h.clock.sleep(COVER_RETRY).await;
        schedule.tick().await;

        assert_eq!(h.rp.called("open_cover"), 1);
        assert_eq!(state(&h).covers_to_open, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_cover_rp_reports_moved_is_left_where_it_is() {
        let h = Harness::with_runs(at(24, 1), &[]);
        h.rp.rig.lock().unwrap().opens_fail = true;
        let mut schedule = h.schedule();

        schedule.tick().await;
        h.signals.note(&EngineEvent {
            event: "close_cover_complete".into(),
            payload: json!({ "calibrator_id": "flat-panel", "status": "closed" }),
        });
        h.clock.sleep(COVER_RETRY).await;
        schedule.tick().await;

        assert_eq!(h.rp.called("open_cover"), 1);
        assert_eq!(h.rp.called("get_cover_state"), 0);
        assert_eq!(state(&h).covers_to_open, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_move_before_dusk_does_not_stop_the_retries() {
        let h = Harness::with_runs(at(24, 1), &[]);
        h.rp.rig.lock().unwrap().opens_fail = true;
        h.signals.note(&EngineEvent {
            event: "close_cover_complete".into(),
            payload: json!({ "calibrator_id": "flat-panel", "status": "closed" }),
        });
        let mut schedule = h.schedule();

        schedule.tick().await;
        h.clock.sleep(COVER_RETRY).await;
        schedule.tick().await;

        assert_eq!(h.rp.called("open_cover"), 2, "dawn's close is no move");
    }

    #[tokio::test]
    async fn the_cover_retries_go_on_while_a_run_is_active() {
        let h = Harness::with_runs(at(24, 1), &["deep_sky"]);
        h.rp.rig.lock().unwrap().opens_fail = true;
        *h.slot.run_reads.lock().unwrap() = Some(u32::try_from(12 * 30).unwrap());
        let mut schedule = h.schedule();

        schedule.tick().await;
        schedule.tick().await;

        assert!(
            h.rp.called("open_cover") >= 5,
            "retried every 5 minutes during a 30-minute run: {}",
            h.rp.called("open_cover")
        );
    }

    #[tokio::test]
    async fn a_run_the_self_resume_picked_up_is_waited_for_and_the_night_goes_on_after_it() {
        let h = Harness::with_runs(at(24, 3), &["sky_flat", "deep_sky", "deep_sky"]);
        left_in_runs(&h, 0);
        h.slot.known.lock().unwrap().push(KnownRun {
            run_id: "run-resumed".into(),
            session_id: sessions(&[1])[0].clone(),
            active: true,
        });
        h.slot.operator_run("run-resumed", 3);
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(h.slot.sessions(), sessions(&[2]));
        assert_eq!(state(&h).next_run, 3);
    }

    #[tokio::test]
    async fn a_manifest_left_in_place_moves_the_night_past_its_run() {
        let h = Harness::with_runs(at(24, 3), &["sky_flat", "deep_sky"]);
        left_in_runs(&h, 0);
        RunManifest {
            run_id: "run-left".into(),
            session_id: sessions(&[0])[0].clone(),
            workflow: "sky_flat".into(),
            params: json!({}),
            started_at: at(24, 2),
        }
        .write(h.dir.path())
        .await
        .unwrap();
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(h.slot.sessions(), sessions(&[1]));
    }

    #[tokio::test]
    async fn a_manifest_of_an_earlier_run_does_not_move_the_night_back() {
        let h = Harness::with_runs(at(24, 3), &["sky_flat", "deep_sky", "deep_sky"]);
        left_in_runs(&h, 2);
        RunManifest {
            run_id: "run-left".into(),
            session_id: sessions(&[0])[0].clone(),
            workflow: "sky_flat".into(),
            params: json!({}),
            started_at: at(24, 2),
        }
        .write(h.dir.path())
        .await
        .unwrap();
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(h.slot.sessions(), sessions(&[2]));
    }

    #[tokio::test]
    async fn a_run_of_another_night_is_no_run_of_this_one() {
        let h = Harness::with_runs(at(24, 3), &["deep_sky"]);
        left_in_runs(&h, 0);
        h.slot.known.lock().unwrap().push(KnownRun {
            run_id: "run-old".into(),
            session_id: "night-2026-11-22-0".into(),
            active: false,
        });
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(h.slot.sessions(), sessions(&[0]));
    }

    #[tokio::test]
    async fn dawn_stops_the_nights_run_and_ends_the_cover_retries() {
        let h = Harness::with_runs(at(24, 1), &["deep_sky"]);
        {
            let mut rig = h.rp.rig.lock().unwrap();
            rig.opens_fail = true;
            // A cooler the warm-up never stops holds its output, so the
            // file stays on night 23 after dawn.
            rig.coolers.insert("main-cam".into(), Some(true));
            rig.reads_to_cool = None;
        }
        *h.slot.run_reads.lock().unwrap() = Some(u32::MAX);
        let mut schedule = h.schedule();

        h.run_until(&mut schedule, 10, |h| {
            h.status
                .snapshot()
                .is_some_and(|s| s.night == date(23) && s.phase == Phase::AfterDawn)
        })
        .await;

        assert_eq!(*h.slot.stopped.lock().unwrap(), ["run-night-2026-11-23-0"]);
        assert_eq!(h.status.run(), None);
        assert_eq!(state(&h).covers_to_open, Vec::<String>::new());
    }

    #[tokio::test]
    async fn the_night_report_names_the_schedules_run_while_it_is_active() {
        let h = Harness::with_runs(at(24, 1), &["deep_sky"]);
        *h.slot.run_reads.lock().unwrap() = Some(u32::MAX);
        let mut schedule = h.schedule();

        schedule.tick().await;
        schedule.tick().await;

        let report = h.status.report(true, Some("run-night-2026-11-23-0"));
        assert_eq!(report["run_id"], json!("run-night-2026-11-23-0"));
        let report = h.status.report(true, Some("operator-run"));
        assert_eq!(report["run_id"], Value::Null);
    }
}
