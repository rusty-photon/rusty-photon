//! The night schedule (design § Night Schedule): with a `night` block in
//! the configuration, `session-runner` runs the rig's night itself, as
//! an MCP client of `rp`.
//!
//! [`config`] is the block and its load checks, [`state`] is
//! `night.json`, [`clock`] turns `rp`'s civil twilight into dusk and
//! dawn, [`rules`] are the cut rules, [`dusk`] is dusk from its safety
//! wait to the cooldown, [`run_list`] is the night's runs, [`dawn`] is
//! dawn and the after-dawn checks, and [`schedule`] is the loop that runs
//! them and keeps `night.json` on the night ahead.

pub mod clock;
pub mod config;
pub mod dawn;
pub mod dusk;
pub mod rules;
pub mod run_list;
pub mod schedule;
pub mod state;
#[cfg(test)]
mod test_rig;

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tracing::{debug, info, warn};

pub use config::{check, NightConfig, NightRun, SignedDuration};
pub use state::{NightState, Phase};

use tokio::sync::Notify;

use crate::engine::{EngineEvent, EventIntake, SystemClock};
use crate::events;
use crate::mcp_client::McpClient;
use crate::runs::{self, AppState, StartRefused, StartRequest};
use schedule::{KnownRun, RunSlot};

/// The schedule's place as `GET /night` reports it.
#[derive(Default)]
pub struct NightStatus {
    state: Mutex<Option<NightState>>,
    /// The run the schedule started, until it sees the run end.
    run: Mutex<Option<String>>,
}

impl NightStatus {
    /// The schedule's place, `None` until it has one.
    #[must_use]
    pub fn snapshot(&self) -> Option<NightState> {
        lock(&self.state).clone()
    }

    pub fn publish(&self, state: NightState) {
        *lock(&self.state) = Some(state);
    }

    /// The run the schedule started, until it sees the run end.
    #[must_use]
    pub fn run(&self) -> Option<String> {
        lock(&self.run).clone()
    }

    pub fn set_run(&self, run_id: Option<String>) {
        *lock(&self.run) = run_id;
    }

    /// `GET /night`'s body. `configured` is whether there is a `night`
    /// block at all; `active` is the run that holds the run slot, so the
    /// report names the schedule's run while it is active.
    #[must_use]
    pub fn report(&self, configured: bool, active: Option<&str>) -> Value {
        if !configured {
            return json!({ "phase": "off" });
        }
        let run_id = self.run().filter(|run| Some(run.as_str()) == active);
        match self.snapshot() {
            Some(state) => json!({
                "phase": state.phase,
                "night": state.night,
                "dusk": state.dusk,
                "dawn": state.dawn,
                "run_id": run_id,
                "cameras_given_up": state.cameras_given_up,
                "outputs_left_on": state.outputs_left_on,
            }),
            None => json!({
                "phase": Phase::Day,
                "night": null,
                "dusk": null,
                "dawn": null,
                "run_id": null,
                "cameras_given_up": [],
                "outputs_left_on": [],
            }),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A poisoned lock means a panic mid-update; the data is plain, still
    // consistent enough to read.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What the schedule learns from `rp`'s event stream.
#[derive(Default)]
pub struct Signals {
    /// Notified at each `cooler_warmup_complete`: it wakes a cooler wait.
    warmup_done: Notify,
    /// Notified at each `safety_changed` to `safe`: it wakes dusk's
    /// safety wait.
    safe_again: Notify,
    /// The calibrators whose cover `rp` reported moved, by an
    /// `open_cover_complete` or a `close_cover_complete`, since the
    /// schedule last forgot their moves.
    covers_moved: Mutex<BTreeSet<String>>,
}

impl Signals {
    fn note(&self, event: &EngineEvent) {
        let field = |key: &str| event.payload.get(key).and_then(Value::as_str);
        match event.event.as_str() {
            "cooler_warmup_complete" => self.warmup_done.notify_one(),
            "safety_changed" if field("new_state") == Some("safe") => self.safe_again.notify_one(),
            "open_cover_complete" | "close_cover_complete" => {
                if let Some(calibrator) = field("calibrator_id") {
                    lock(&self.covers_moved).insert(calibrator.to_owned());
                }
            }
            _ => {}
        }
    }

    /// Forget every move of `calibrator`'s cover reported so far.
    pub fn forget_moves(&self, calibrator: &str) {
        lock(&self.covers_moved).remove(calibrator);
    }

    /// Whether `rp` reported `calibrator`'s cover moved since its moves
    /// were last forgotten.
    #[must_use]
    pub fn cover_moved(&self, calibrator: &str) -> bool {
        lock(&self.covers_moved).contains(calibrator)
    }
}

/// Read `rp`'s events for the life of the service into `signals`.
///
/// Read without pause, the stream never backs up into a consumer `rp`
/// cuts loose for lagging, whose reconnect would read as `rp` back from
/// a restart.
async fn drain_events(mut events: EventIntake, signals: Arc<Signals>) {
    loop {
        signals.note(&events.next().await);
    }
}

/// This service's run slot, as the schedule uses it.
pub struct ServiceSlot {
    state: Arc<AppState>,
    mcp_url: String,
}

impl RunSlot for ServiceSlot {
    fn active_run(&self) -> Option<String> {
        self.state.runs.active()
    }

    fn runs(&self) -> Vec<KnownRun> {
        self.state
            .runs
            .list()
            .into_iter()
            .map(|r| KnownRun {
                active: r.state.is_active(),
                run_id: r.run_id,
                session_id: r.session_id,
                workflow: r.workflow,
            })
            .collect()
    }

    fn stop_run(&self, run_id: &str) {
        if let Err(e) = self.state.runs.stop(run_id) {
            debug!(run_id, error = ?e, "the run ended before dawn could stop it");
        }
    }

    fn hold(&self) -> bool {
        self.state.runs.hold_for_night()
    }

    fn release(&self) {
        self.state.runs.release_night();
    }

    fn held(&self) -> bool {
        self.state.runs.held_for_night()
    }

    fn start_run(
        &self,
        run: &NightRun,
        session_id: &str,
        from_night: bool,
    ) -> impl Future<Output = Result<String, StartRefused>> + Send {
        let request = StartRequest {
            workflow: run.workflow.clone(),
            params: run.params.clone().unwrap_or_else(|| json!({})),
            session_id: session_id.to_owned(),
        };
        runs::start_run(&self.state, request, &self.mcp_url, from_night)
    }
}

/// First and longest wait between two connect attempts to `rp`.
const CONNECT_BACKOFF: Duration = Duration::from_secs(1);
const MAX_CONNECT_BACKOFF: Duration = Duration::from_secs(30);

/// Run the schedule for the life of the service: publish `night.json`
/// as the last process left it, connect to `rp`, then keep the file on
/// the night ahead.
pub async fn run(state: Arc<AppState>) {
    let (Some(night), Some(mcp_url)) = (
        state.config.night.as_ref(),
        state.config.mcp_server_url.as_deref(),
    ) else {
        return;
    };
    if let Some(left) = NightState::load(&state.config.state_dir).await {
        debug!(night = %left.night, phase = ?left.phase, "night.json read");
        state.night.publish(left);
    }
    let connection = state.config.rp_connection();
    let mut backoff = CONNECT_BACKOFF;
    // A failure is logged at `warn!` the first time it reads so, and at
    // `debug!` while it repeats: a bad `ca_cert` or URL never clears by
    // itself, and the night waits on it.
    let mut logged: Option<String> = None;
    let rp = loop {
        match McpClient::connect(mcp_url, connection.auth(), connection.ca_path()).await {
            Ok(rp) => break rp,
            Err(e) => {
                let message = e.to_string();
                if logged.as_deref() == Some(message.as_str()) {
                    debug!(error = %message, wait = ?backoff, "night schedule still cannot reach rp");
                } else {
                    warn!(error = %message, "night schedule cannot reach rp; no night runs until it can, and it keeps trying");
                    logged = Some(message);
                }
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(MAX_CONNECT_BACKOFF);
            }
        }
    };
    let (events, connects) =
        events::subscribe_counting(runs::events_url(&state.config, mcp_url), &connection).await;
    let signals = Arc::new(Signals::default());
    tokio::spawn(drain_events(events, signals.clone()));
    let slot = ServiceSlot {
        state: state.clone(),
        mcp_url: mcp_url.to_owned(),
    };
    info!("night schedule started");
    schedule::Schedule::new(schedule::Seams {
        rp: &rp,
        clock: &SystemClock,
        slot: &slot,
        night,
        state_dir: &state.config.state_dir,
        status: &state.night,
        signals,
        safety_poll: state.config.safety_poll_interval,
        connects,
    })
    .run()
    .await;
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};

    fn event(name: &str, payload: Value) -> EngineEvent {
        EngineEvent {
            event: name.to_owned(),
            payload,
        }
    }

    #[tokio::test]
    async fn the_drain_notifies_at_each_cooler_warmup_complete_and_at_nothing_else() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let signals = Arc::new(Signals::default());
        tokio::spawn(drain_events(EventIntake::new(rx), signals.clone()));

        tx.send(event("exposure_complete", json!({})))
            .await
            .unwrap();
        let early =
            tokio::time::timeout(Duration::from_millis(100), signals.warmup_done.notified()).await;
        assert!(early.is_err(), "another event woke the wait");

        tx.send(event(
            "cooler_warmup_complete",
            json!({ "camera_id": "main-cam" }),
        ))
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), signals.warmup_done.notified())
            .await
            .expect("cooler_warmup_complete wakes the wait");
    }

    #[tokio::test]
    async fn a_monitor_turning_safe_wakes_the_safety_wait_and_one_turning_unsafe_does_not() {
        let signals = Signals::default();

        signals.note(&event(
            "safety_changed",
            json!({ "monitor": "roof", "new_state": "unsafe" }),
        ));
        let early =
            tokio::time::timeout(Duration::from_millis(100), signals.safe_again.notified()).await;
        assert!(early.is_err(), "an unsafe transition woke the wait");

        signals.note(&event(
            "safety_changed",
            json!({ "monitor": "roof", "new_state": "safe" }),
        ));
        tokio::time::timeout(Duration::from_secs(5), signals.safe_again.notified())
            .await
            .expect("a safe transition wakes the wait");
    }

    #[test]
    fn a_cover_counts_as_moved_from_its_move_until_its_moves_are_forgotten() {
        let signals = Signals::default();
        signals.note(&event(
            "open_cover_started",
            json!({ "calibrator_id": "flat-panel" }),
        ));
        signals.note(&event(
            "open_cover_failed",
            json!({ "calibrator_id": "flat-panel", "error": "refused" }),
        ));
        assert!(!signals.cover_moved("flat-panel"), "a failed move is none");

        signals.note(&event(
            "close_cover_complete",
            json!({ "calibrator_id": "flat-panel", "status": "closed" }),
        ));
        assert!(signals.cover_moved("flat-panel"));
        assert!(!signals.cover_moved("other-panel"));

        signals.forget_moves("flat-panel");
        assert!(!signals.cover_moved("flat-panel"));
        signals.note(&event(
            "open_cover_complete",
            json!({ "calibrator_id": "flat-panel", "status": "open" }),
        ));
        assert!(signals.cover_moved("flat-panel"));
    }

    #[test]
    fn without_a_night_block_the_report_is_off_and_nothing_else() {
        assert_eq!(
            NightStatus::default().report(false, None),
            json!({ "phase": "off" })
        );
    }

    #[test]
    fn before_the_twilight_is_known_the_night_is_null() {
        assert_eq!(
            NightStatus::default().report(true, None),
            json!({ "phase": "day", "night": null, "dusk": null, "dawn": null,
                    "run_id": null, "cameras_given_up": [], "outputs_left_on": [] })
        );
    }

    #[test]
    fn the_report_carries_the_schedules_place() {
        let status = NightStatus::default();
        let mut state = NightState::ahead(
            NaiveDate::from_ymd_opt(2026, 11, 23).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 24, 0, 58, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 24, 14, 31, 0).unwrap(),
        );
        state.cameras_given_up.push("main-cam".into());
        status.publish(state);
        assert_eq!(
            status.report(true, None),
            json!({ "phase": "day", "night": "2026-11-23",
                    "dusk": "2026-11-24T00:58:00Z", "dawn": "2026-11-24T14:31:00Z",
                    "run_id": null, "cameras_given_up": ["main-cam"], "outputs_left_on": [] })
        );
    }
}
