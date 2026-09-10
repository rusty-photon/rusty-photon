//! The Focuser Temperature Watch (rp.md § Focuser Temperature Watch):
//! a background poll over every connected focuser's temperature probe
//! that turns a drift into an **event, never an action**.
//!
//! Every `equipment.temperature_poll_interval` the watch reads
//! `Temperature` on each focuser whose session is live and emits
//! `temperature_changed {sensor, value}` once a reading has moved by at
//! least `equipment.temperature_event_delta_c` since the last emission
//! for that focuser. The first reading through a session seeds the
//! baseline silently; the session document decides whether a refocus
//! fits (tenet 3 — the trigger lives in the operator-started document).
//! Reading a probe moves nothing.
//!
//! The delta logic lives in [`WatchCore`], a pure state machine over
//! `(focuser id, probe read)` observations so it is unit-testable
//! without a runtime; [`TemperatureWatch`] owns the polling, the
//! per-session baseline reset and the emission.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::Focuser;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::config::equipment::TemperatureEventDeltaC;
use crate::equipment::EquipmentRegistry;
use crate::events::EventBus;

/// Bound on one `Temperature` read. A probe that never answers must
/// not wedge the pass — the interval is the cadence, not this.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// One probe read, as the core sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Probe {
    /// The property answered with a reading in °C.
    Reading(f64),
    /// The property answered `NOT_IMPLEMENTED`: no probe on this device.
    NotImplemented,
    /// Any other failure — a transport error, a timeout, a driver
    /// error. The reading is unknown; nothing about the baseline is.
    Failed,
}

/// What one observation asks the surrounding task to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Observation {
    /// The first reading through this session: the baseline is set and
    /// nothing is emitted.
    Seeded(f64),
    /// Moved by at least the delta since the baseline: emit, and the
    /// reading is the new baseline.
    Emit(f64),
    /// Under the delta: the baseline stays where it was.
    Quiet,
    /// No probe. `first` on the session's first `NOT_IMPLEMENTED`, so
    /// the task can say so once rather than every tick.
    NoProbe { first: bool },
    /// The read failed; the baseline is untouched.
    ReadFailed,
}

/// What the core remembers per focuser.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Baseline {
    /// No successful reading yet this session.
    Unknown,
    /// The device answered `NOT_IMPLEMENTED`; noted so it is logged once.
    NoProbe,
    /// The reading the last emission (or the seed) recorded.
    Value(f64),
}

/// The pure delta state machine. Feed it one probe read per focuser
/// per poll; reset a focuser when its session is re-established.
pub struct WatchCore {
    delta_c: f64,
    baselines: HashMap<String, Baseline>,
}

impl WatchCore {
    #[must_use]
    pub fn new(delta: TemperatureEventDeltaC) -> Self {
        Self {
            delta_c: delta.value(),
            baselines: HashMap::new(),
        }
    }

    /// Forget a focuser's baseline — its next reading seeds a fresh
    /// one silently. Called when its session was lost or re-established:
    /// the device behind the same config id may be a different one.
    pub fn reset(&mut self, focuser_id: &str) {
        self.baselines.remove(focuser_id);
    }

    /// One observation for `focuser_id`.
    pub fn observe(&mut self, focuser_id: &str, probe: Probe) -> Observation {
        let slot = self
            .baselines
            .entry(focuser_id.to_owned())
            .or_insert(Baseline::Unknown);
        match probe {
            // A NaN or infinite reading is a broken probe, not a value
            // to measure drift against.
            Probe::Reading(value) if !value.is_finite() => Observation::ReadFailed,
            Probe::Reading(value) => match *slot {
                Baseline::Value(baseline) => {
                    if (value - baseline).abs() >= self.delta_c {
                        *slot = Baseline::Value(value);
                        Observation::Emit(value)
                    } else {
                        Observation::Quiet
                    }
                }
                Baseline::Unknown | Baseline::NoProbe => {
                    *slot = Baseline::Value(value);
                    Observation::Seeded(value)
                }
            },
            Probe::NotImplemented => {
                let first = *slot != Baseline::NoProbe;
                *slot = Baseline::NoProbe;
                Observation::NoProbe { first }
            }
            Probe::Failed => Observation::ReadFailed,
        }
    }
}

/// The polling task: one pass over the registry's focusers per
/// interval, emitting on the event bus.
pub struct TemperatureWatch {
    equipment: Arc<EquipmentRegistry>,
    event_bus: Arc<EventBus>,
    interval: Duration,
    core: WatchCore,
    /// The session handle each focuser was last read through. A
    /// different handle on the next pass is a re-established session
    /// (rp.md § Device Session Recovery): the baseline starts over.
    handles: HashMap<String, Arc<dyn Focuser>>,
}

impl TemperatureWatch {
    #[must_use]
    pub fn new(
        equipment: Arc<EquipmentRegistry>,
        event_bus: Arc<EventBus>,
        interval: Duration,
        delta: TemperatureEventDeltaC,
    ) -> Self {
        Self {
            equipment,
            event_bus,
            interval,
            core: WatchCore::new(delta),
            handles: HashMap::new(),
        }
    }

    /// Poll until cancelled (rp shutdown). The startup connect just
    /// ran, so the first read is one full interval out — nothing here
    /// runs on the connect path.
    ///
    /// Cancellation also preempts a pass in flight: a pass can spend up
    /// to [`READ_TIMEOUT`] per unanswering probe, and rp's shutdown
    /// joins this task. Dropping the pass mid-await only abandons a
    /// read; nothing here actuates hardware.
    pub async fn run(mut self, cancel: CancellationToken) {
        info!(interval = ?self.interval, "focuser temperature watch started");
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    debug!("focuser temperature watch stopped");
                    return;
                }
                () = tokio::time::sleep(self.interval) => {}
            }
            tokio::select! {
                () = cancel.cancelled() => {
                    debug!("focuser temperature watch stopped mid-pass");
                    return;
                }
                () = self.pass() => {}
            }
        }
    }

    /// One poll over every configured focuser.
    pub(crate) async fn pass(&mut self) {
        for entry in &self.equipment.focusers {
            // A disconnected slot keeps its stale handle (rp.md § Device
            // Session Recovery), so the flag is the test, not the handle.
            let device = if entry.is_connected() {
                entry.device()
            } else {
                None
            };
            let Some(device) = device else {
                // Disconnected: not polled. Its next session starts over.
                if self.handles.remove(&entry.id).is_some() {
                    debug!(focuser_id = %entry.id, "temperature watch: session lost, baseline dropped");
                    self.core.reset(&entry.id);
                }
                continue;
            };
            let same_session = self
                .handles
                .get(&entry.id)
                .is_some_and(|known| Arc::ptr_eq(known, &device));
            if !same_session {
                self.core.reset(&entry.id);
                self.handles.insert(entry.id.clone(), Arc::clone(&device));
            }
            let probe = read_probe(&entry.id, device.as_ref()).await;
            match self.core.observe(&entry.id, probe) {
                Observation::Seeded(value) => {
                    debug!(focuser_id = %entry.id, value, "temperature watch: baseline seeded");
                }
                Observation::Emit(value) => {
                    debug!(focuser_id = %entry.id, value, "temperature changed");
                    self.event_bus.emit(
                        "temperature_changed",
                        serde_json::json!({ "sensor": entry.id, "value": value }),
                    );
                }
                Observation::NoProbe { first: true } => {
                    debug!(focuser_id = %entry.id, "temperature watch: focuser has no temperature probe");
                }
                Observation::Quiet
                | Observation::NoProbe { first: false }
                | Observation::ReadFailed => {}
            }
        }
    }
}

/// One bounded `Temperature` read, classified for the core.
async fn read_probe(focuser_id: &str, device: &dyn Focuser) -> Probe {
    match tokio::time::timeout(READ_TIMEOUT, device.temperature()).await {
        Ok(Ok(value)) => Probe::Reading(value),
        Ok(Err(e)) if e.code == ascom_alpaca::ASCOMErrorCode::NOT_IMPLEMENTED => {
            Probe::NotImplemented
        }
        Ok(Err(e)) => {
            debug!(focuser_id, error = %e, "temperature watch: probe read failed");
            Probe::Failed
        }
        Err(_) => {
            debug!(
                focuser_id,
                "temperature watch: probe read timed out after {READ_TIMEOUT:?}"
            );
            Probe::Failed
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    use axum::routing::{get, put};
    use axum::{Json, Router};

    use super::*;
    use crate::config;
    use crate::equipment::test_support::spawn_stub;

    fn delta(value: f64) -> TemperatureEventDeltaC {
        TemperatureEventDeltaC::try_new(value).unwrap()
    }

    #[test]
    fn the_first_reading_seeds_silently_and_the_delta_counts_from_the_last_emission() {
        let mut core = WatchCore::new(delta(0.5));
        assert_eq!(
            core.observe("f", Probe::Reading(10.0)),
            Observation::Seeded(10.0)
        );
        // 0.3 under the delta: quiet, and the baseline stays at 10.0 ...
        assert_eq!(core.observe("f", Probe::Reading(10.3)), Observation::Quiet);
        // ... so 10.6 is 0.6 from the baseline even though it is only
        // 0.3 from the previous poll.
        assert_eq!(
            core.observe("f", Probe::Reading(10.6)),
            Observation::Emit(10.6)
        );
        assert_eq!(core.observe("f", Probe::Reading(10.9)), Observation::Quiet);
        // Cooling emits the same way.
        assert_eq!(
            core.observe("f", Probe::Reading(10.1)),
            Observation::Emit(10.1)
        );
    }

    #[test]
    fn a_drift_of_exactly_the_delta_emits() {
        let mut core = WatchCore::new(delta(0.5));
        assert_eq!(
            core.observe("f", Probe::Reading(4.0)),
            Observation::Seeded(4.0)
        );
        assert_eq!(
            core.observe("f", Probe::Reading(4.5)),
            Observation::Emit(4.5)
        );
    }

    #[test]
    fn a_missing_probe_is_reported_once_and_a_later_reading_seeds() {
        let mut core = WatchCore::new(delta(0.5));
        assert_eq!(
            core.observe("f", Probe::NotImplemented),
            Observation::NoProbe { first: true }
        );
        assert_eq!(
            core.observe("f", Probe::NotImplemented),
            Observation::NoProbe { first: false }
        );
        assert_eq!(
            core.observe("f", Probe::Reading(1.0)),
            Observation::Seeded(1.0)
        );
    }

    #[test]
    fn a_failed_read_keeps_the_baseline() {
        let mut core = WatchCore::new(delta(0.5));
        assert_eq!(
            core.observe("f", Probe::Reading(10.0)),
            Observation::Seeded(10.0)
        );
        assert_eq!(core.observe("f", Probe::Failed), Observation::ReadFailed);
        assert_eq!(
            core.observe("f", Probe::Reading(f64::NAN)),
            Observation::ReadFailed
        );
        assert_eq!(
            core.observe("f", Probe::Reading(f64::INFINITY)),
            Observation::ReadFailed
        );
        // Still measured from 10.0, not re-seeded.
        assert_eq!(
            core.observe("f", Probe::Reading(10.6)),
            Observation::Emit(10.6)
        );
    }

    #[test]
    fn a_reset_makes_the_next_reading_a_seed() {
        let mut core = WatchCore::new(delta(0.5));
        assert_eq!(
            core.observe("f", Probe::Reading(10.0)),
            Observation::Seeded(10.0)
        );
        core.reset("f");
        assert_eq!(
            core.observe("f", Probe::Reading(25.0)),
            Observation::Seeded(25.0)
        );
        // Other focusers are unaffected by a reset.
        assert_eq!(
            core.observe("g", Probe::Reading(3.0)),
            Observation::Seeded(3.0)
        );
        core.reset("f");
        assert_eq!(
            core.observe("g", Probe::Reading(3.6)),
            Observation::Emit(3.6)
        );
    }

    #[test]
    fn focusers_are_tracked_independently() {
        let mut core = WatchCore::new(delta(1.0));
        assert_eq!(
            core.observe("a", Probe::Reading(0.0)),
            Observation::Seeded(0.0)
        );
        assert_eq!(
            core.observe("b", Probe::Reading(20.0)),
            Observation::Seeded(20.0)
        );
        assert_eq!(
            core.observe("a", Probe::Reading(1.0)),
            Observation::Emit(1.0)
        );
        assert_eq!(core.observe("b", Probe::Reading(20.5)), Observation::Quiet);
    }

    // --- the task against an Alpaca stub focuser ---------------------

    /// What the stub's `Temperature` answers: a reading, or an Alpaca
    /// error number (`0x400` is `NOT_IMPLEMENTED`).
    #[derive(Clone, Copy)]
    enum StubProbe {
        Reading(f64),
        Error(u32),
    }

    struct ProbeState {
        probe: Mutex<StubProbe>,
        reads: AtomicU32,
    }

    fn focuser_router(state: Arc<ProbeState>) -> Router {
        Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [{
                            "DeviceName": "Focuser 0",
                            "DeviceType": "Focuser",
                            "DeviceNumber": 0,
                            "UniqueID": "watch-focuser-uid"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/focuser/0/connected",
                put(|| async { Json(serde_json::json!({ "ErrorNumber": 0, "ErrorMessage": "" })) }),
            )
            .route(
                "/api/v1/focuser/0/temperature",
                get(move || {
                    let state = state.clone();
                    async move {
                        state.reads.fetch_add(1, Ordering::SeqCst);
                        let probe = *state.probe.lock().unwrap();
                        match probe {
                            StubProbe::Reading(value) => Json(serde_json::json!({
                                "Value": value, "ErrorNumber": 0, "ErrorMessage": ""
                            })),
                            StubProbe::Error(number) => Json(serde_json::json!({
                                "ErrorNumber": number, "ErrorMessage": "scripted"
                            })),
                        }
                    }
                }),
            )
    }

    fn focuser_config(url: &str) -> config::FocuserConfig {
        config::FocuserConfig {
            id: "watched".to_string(),
            alpaca_url: url.to_string(),
            device_number: 0,
            min_position: None,
            max_position: None,
            steps_per_sec: config::focuser::FocuserStepsPerSec::default(),
            backlash: None,
            auth: None,
        }
    }

    async fn registry_with(url: &str) -> Arc<EquipmentRegistry> {
        let equipment = config::EquipmentConfig {
            focusers: vec![focuser_config(url)],
            ..config::EquipmentConfig::default()
        };
        Arc::new(EquipmentRegistry::new(&equipment, None).await)
    }

    fn watch_over(equipment: Arc<EquipmentRegistry>) -> TemperatureWatch {
        TemperatureWatch::new(
            equipment,
            Arc::new(EventBus::from_config(&[], None).unwrap()),
            Duration::from_millis(10),
            delta(0.5),
        )
    }

    fn probe_state(probe: StubProbe) -> Arc<ProbeState> {
        Arc::new(ProbeState {
            probe: Mutex::new(probe),
            reads: AtomicU32::new(0),
        })
    }

    #[tokio::test]
    async fn a_pass_seeds_then_emits_once_the_probe_drifts_past_the_delta() {
        let state = probe_state(StubProbe::Reading(10.0));
        let stub = spawn_stub(focuser_router(state.clone())).await;
        let mut watch = watch_over(registry_with(&stub.url()).await);
        let mut events = watch.event_bus.subscribe();

        watch.pass().await;
        assert_eq!(state.reads.load(Ordering::SeqCst), 1);
        assert!(events.try_recv().is_err(), "the seed must not emit");

        *state.probe.lock().unwrap() = StubProbe::Reading(10.3);
        watch.pass().await;
        assert!(events.try_recv().is_err(), "0.3 is under the delta");

        *state.probe.lock().unwrap() = StubProbe::Reading(10.6);
        watch.pass().await;
        let event = events.try_recv().expect("0.6 from the baseline must emit");
        assert_eq!(event.event, "temperature_changed");
        assert_eq!(event.payload["sensor"], "watched");
        assert_eq!(event.payload["value"], 10.6);
        assert!(events.try_recv().is_err(), "exactly one emission");
    }

    #[tokio::test]
    async fn a_re_established_session_starts_over_from_its_first_reading() {
        let state = probe_state(StubProbe::Reading(10.0));
        let stub = spawn_stub(focuser_router(state.clone())).await;
        let registry = registry_with(&stub.url()).await;
        let mut watch = watch_over(registry.clone());
        let mut events = watch.event_bus.subscribe();
        watch.pass().await;

        // The supervisor installs a fresh handle for the same config id
        // (a service restart); the reading behind it is far from the old
        // baseline, and must seed rather than emit.
        let fresh = registry_with(&stub.url()).await.focusers[0]
            .device()
            .expect("the second connect must yield a handle");
        registry.focusers[0].session.install(fresh);
        *state.probe.lock().unwrap() = StubProbe::Reading(25.0);
        watch.pass().await;
        assert!(
            events.try_recv().is_err(),
            "the new session's first reading seeds"
        );

        *state.probe.lock().unwrap() = StubProbe::Reading(25.6);
        watch.pass().await;
        let event = events
            .try_recv()
            .expect("drift from the new baseline emits");
        assert_eq!(event.payload["value"], 25.6);
    }

    #[tokio::test]
    async fn a_lost_session_is_not_polled_and_its_baseline_is_dropped() {
        let state = probe_state(StubProbe::Reading(10.0));
        let stub = spawn_stub(focuser_router(state.clone())).await;
        let registry = registry_with(&stub.url()).await;
        let mut watch = watch_over(registry.clone());
        let mut events = watch.event_bus.subscribe();
        watch.pass().await;

        registry.focusers[0].session.mark_disconnected();
        watch.pass().await;
        assert_eq!(
            state.reads.load(Ordering::SeqCst),
            1,
            "a disconnected focuser is not read"
        );

        // Reconnected through the same stale handle (the slot keeps it
        // while disconnected): the handle is the one already known, but
        // the baseline was dropped with the session, so the next reading
        // seeds.
        let handle = registry.focusers[0]
            .device()
            .expect("a disconnected slot keeps its stale handle");
        registry.focusers[0].session.install(handle);
        *state.probe.lock().unwrap() = StubProbe::Reading(30.0);
        watch.pass().await;
        assert!(
            events.try_recv().is_err(),
            "the first reading after a loss seeds"
        );
    }

    #[tokio::test]
    async fn a_missing_probe_never_emits_and_a_failed_read_keeps_the_baseline() {
        let state = probe_state(StubProbe::Error(0x400));
        let stub = spawn_stub(focuser_router(state.clone())).await;
        let mut watch = watch_over(registry_with(&stub.url()).await);
        let mut events = watch.event_bus.subscribe();
        watch.pass().await;
        watch.pass().await;
        assert_eq!(state.reads.load(Ordering::SeqCst), 2, "still polled");
        assert!(events.try_recv().is_err());

        *state.probe.lock().unwrap() = StubProbe::Reading(10.0);
        watch.pass().await;
        *state.probe.lock().unwrap() = StubProbe::Error(0x500);
        watch.pass().await;
        *state.probe.lock().unwrap() = StubProbe::Reading(10.6);
        watch.pass().await;
        let event = events
            .try_recv()
            .expect("the fault must not have dropped the baseline");
        assert_eq!(event.payload["value"], 10.6);
    }

    #[tokio::test]
    async fn a_disconnected_at_startup_focuser_is_skipped() {
        let registry = registry_with("http://127.0.0.1:1").await;
        assert!(!registry.focusers[0].is_connected());
        let mut watch = watch_over(registry);
        let mut events = watch.event_bus.subscribe();
        watch.pass().await;
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_task_stops_on_cancellation() {
        let state = probe_state(StubProbe::Reading(10.0));
        let stub = spawn_stub(focuser_router(state.clone())).await;
        let watch = watch_over(registry_with(&stub.url()).await);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(watch.run(cancel.clone()));
        tokio::time::sleep(Duration::from_millis(60)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the watch must stop on cancellation")
            .unwrap();
        assert!(
            state.reads.load(Ordering::SeqCst) >= 1,
            "the task polled while it ran"
        );
    }
}
