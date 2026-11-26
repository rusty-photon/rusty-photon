//! The night schedule (design § Night Schedule): with a `night` block in
//! the configuration, `session-runner` runs the rig's night itself, as
//! an MCP client of `rp`.
//!
//! [`config`] is the block and its load checks, [`state`] is
//! `night.json`, [`clock`] turns `rp`'s civil twilight into dusk and
//! dawn, [`rules`] are the cut rules, [`dawn`] is dawn and the after-dawn
//! checks, and [`schedule`] is the loop that runs them and keeps
//! `night.json` on the night ahead.

pub mod clock;
pub mod config;
pub mod dawn;
pub mod rules;
pub mod schedule;
pub mod state;
#[cfg(test)]
mod test_rig;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tracing::{debug, info, warn};

pub use config::{check, NightConfig, NightRun, SignedDuration};
pub use state::{NightState, Phase};

use tokio::sync::Notify;

use crate::engine::{EventIntake, SystemClock};
use crate::events;
use crate::mcp_client::McpClient;
use crate::runs::{self, AppState};

/// The schedule's place as `GET /night` reports it.
#[derive(Default)]
pub struct NightStatus {
    state: Mutex<Option<NightState>>,
}

impl NightStatus {
    /// The schedule's place, `None` until it has one.
    #[must_use]
    pub fn snapshot(&self) -> Option<NightState> {
        self.lock().clone()
    }

    pub fn publish(&self, state: NightState) {
        *self.lock() = Some(state);
    }

    /// `GET /night`'s body. `configured` is whether there is a `night`
    /// block at all.
    #[must_use]
    pub fn report(&self, configured: bool) -> Value {
        if !configured {
            return json!({ "phase": "off" });
        }
        match self.snapshot() {
            Some(state) => json!({
                "phase": state.phase,
                "night": state.night,
                "dusk": state.dusk,
                "dawn": state.dawn,
                "run_id": null,
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

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<NightState>> {
        // A poisoned lock means a panic mid-update; the state is plain
        // data, still consistent enough to read.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Read `rp`'s events for the life of the service and notify
/// `warmup_done` at each `cooler_warmup_complete`.
///
/// Read without pause, the stream never backs up into a consumer `rp`
/// cuts loose for lagging, whose reconnect would read as `rp` back from
/// a restart.
async fn drain_events(mut events: EventIntake, warmup_done: Arc<Notify>) {
    loop {
        if events.next().await.event == "cooler_warmup_complete" {
            warmup_done.notify_one();
        }
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
    let warmup_done = Arc::new(Notify::new());
    tokio::spawn(drain_events(events, warmup_done.clone()));
    info!("night schedule started");
    schedule::Schedule::new(schedule::Seams {
        rp: &rp,
        clock: &SystemClock,
        slot: &state.runs,
        night,
        state_dir: &state.config.state_dir,
        status: &state.night,
        warmup_done,
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

    #[tokio::test]
    async fn the_drain_notifies_at_each_cooler_warmup_complete_and_at_nothing_else() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let notify = Arc::new(Notify::new());
        tokio::spawn(drain_events(EventIntake::new(rx), notify.clone()));
        let event = |name: &str| crate::engine::EngineEvent {
            event: name.to_owned(),
            payload: json!({}),
        };

        tx.send(event("exposure_complete")).await.unwrap();
        let early = tokio::time::timeout(Duration::from_millis(100), notify.notified()).await;
        assert!(early.is_err(), "another event woke the wait");

        tx.send(event("cooler_warmup_complete")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), notify.notified())
            .await
            .expect("cooler_warmup_complete wakes the wait");
    }

    #[test]
    fn without_a_night_block_the_report_is_off_and_nothing_else() {
        assert_eq!(
            NightStatus::default().report(false),
            json!({ "phase": "off" })
        );
    }

    #[test]
    fn before_the_twilight_is_known_the_night_is_null() {
        assert_eq!(
            NightStatus::default().report(true),
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
            status.report(true),
            json!({ "phase": "day", "night": "2026-11-23",
                    "dusk": "2026-11-24T00:58:00Z", "dawn": "2026-11-24T14:31:00Z",
                    "run_id": null, "cameras_given_up": ["main-cam"], "outputs_left_on": [] })
        );
    }
}
