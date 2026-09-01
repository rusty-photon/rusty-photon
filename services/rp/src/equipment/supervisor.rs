//! Reconnect supervisor (rp.md § Device Session Recovery).
//!
//! An Alpaca device session is server-side state: a downstream service
//! restart silently resets `Connected` to false while rp's client
//! handle stays valid, and a device that was unreachable at startup
//! never had a session at all. This loop walks every configured device
//! at `equipment.reconnect_interval`, health-checks each session
//! through the `Connected` property, and re-establishes dead sessions
//! with the full per-type connect routine — roster re-enumeration,
//! `Connected = true`, and a fresh read of the connect-time property
//! cache. Nothing is carried over from a dead session.
//!
//! Tenet 3 holds on this path exactly as on first connect: everything
//! here re-*reads* state; `Connected = true` is non-actuating by driver
//! contract.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::Device;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::session::DeviceSession;
use super::{
    camera, cover_calibrator, dome, filter_wheel, focuser, mount, observing_conditions, rotator,
    safety_monitor, switch, EquipmentRegistry,
};
use crate::events::EventBus;

pub struct ReconnectSupervisor {
    equipment: Arc<EquipmentRegistry>,
    event_bus: Arc<EventBus>,
    interval: Duration,
    ca_cert_path: Option<PathBuf>,
}

impl ReconnectSupervisor {
    #[must_use]
    pub const fn new(
        equipment: Arc<EquipmentRegistry>,
        event_bus: Arc<EventBus>,
        interval: Duration,
        ca_cert_path: Option<PathBuf>,
    ) -> Self {
        Self {
            equipment,
            event_bus,
            interval,
            ca_cert_path,
        }
    }

    /// Poll until cancelled (rp shutdown). The startup connect just
    /// ran, so the first pass waits one full interval.
    pub async fn run(self, cancel: CancellationToken) {
        info!(interval = ?self.interval, "equipment reconnect supervisor started");
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    debug!("equipment reconnect supervisor stopped");
                    return;
                }
                () = tokio::time::sleep(self.interval) => {}
            }
            self.pass().await;
        }
    }

    /// One supervision pass over every configured device.
    pub(crate) async fn pass(&self) {
        self.pass_imaging_chain().await;
        self.pass_roster().await;
    }

    /// The imaging-chain device kinds (camera through safety monitor).
    async fn pass_imaging_chain(&self) {
        let ca = self.ca_cert_path.as_deref();
        for entry in &self.equipment.cameras {
            supervise(
                "camera",
                Some(&entry.id),
                &entry.session,
                &self.event_bus,
                || async {
                    let (cam, invariants) = camera::establish_camera(&entry.config, ca).await?;
                    entry.set_invariants(invariants);
                    Ok(cam)
                },
            )
            .await;
        }
        for entry in &self.equipment.filter_wheels {
            supervise(
                "filter_wheel",
                Some(&entry.id),
                &entry.session,
                &self.event_bus,
                || filter_wheel::establish_filter_wheel(&entry.config, ca),
            )
            .await;
        }
        for entry in &self.equipment.cover_calibrators {
            supervise(
                "cover_calibrator",
                Some(&entry.id),
                &entry.session,
                &self.event_bus,
                || cover_calibrator::establish_cover_calibrator(&entry.config, ca),
            )
            .await;
        }
        for entry in &self.equipment.focusers {
            supervise(
                "focuser",
                Some(&entry.id),
                &entry.session,
                &self.event_bus,
                || focuser::establish_focuser(&entry.config, ca),
            )
            .await;
        }
        for entry in &self.equipment.safety_monitors {
            supervise(
                "safety_monitor",
                Some(&entry.id),
                &entry.session,
                &self.event_bus,
                || safety_monitor::establish_safety_monitor(&entry.config, ca),
            )
            .await;
        }
    }

    /// The roster-membership device kinds plus the singular mount.
    async fn pass_roster(&self) {
        let ca = self.ca_cert_path.as_deref();
        for entry in &self.equipment.switches {
            supervise(
                "switch",
                Some(&entry.id),
                &entry.session,
                &self.event_bus,
                || switch::establish_switch(&entry.config, ca),
            )
            .await;
        }
        for entry in &self.equipment.rotators {
            supervise(
                "rotator",
                Some(&entry.id),
                &entry.session,
                &self.event_bus,
                || rotator::establish_rotator(&entry.config, ca),
            )
            .await;
        }
        for entry in &self.equipment.observing_conditions {
            supervise(
                "observing_conditions",
                Some(&entry.id),
                &entry.session,
                &self.event_bus,
                || observing_conditions::establish_observing_conditions(&entry.config, ca),
            )
            .await;
        }
        for entry in &self.equipment.domes {
            supervise(
                "dome",
                Some(&entry.id),
                &entry.session,
                &self.event_bus,
                || dome::establish_dome(&entry.config, ca),
            )
            .await;
        }
        if let Some(entry) = self.equipment.mount.as_ref() {
            supervise("mount", None, &entry.session, &self.event_bus, || {
                mount::establish_mount(&entry.config, ca)
            })
            .await;
        }
    }
}

/// Health-check one entry's session and re-establish it when dead.
///
/// The health check is the fast path for an entry that believes its
/// session is alive: read the Alpaca `Connected` property through the
/// held handle, and `Ok(true)` means healthy — nothing else happens. A
/// `false` reading, a failed read, or an entry already marked
/// disconnected triggers `reestablish` — the per-type full connect
/// routine. A disconnected entry runs the full routine even when some
/// other client turned the device back on in the meantime: nothing is
/// adopted from a session rp did not establish, so the connect-time
/// property cache is always the establish routine's own fresh read.
/// (The camera closure writes its invariants *before* the handle
/// installs, so a caller holding a usable handle never pairs it with
/// stale invariants; the only observable mix is a dead old handle with
/// fresh invariants, and a dead handle cannot produce a capture.)
///
/// On success the fresh handle is installed and an `equipment_changed`
/// event with `connected: true` is emitted unconditionally: a service
/// that bounced between two passes never observably flipped the flag,
/// but the session was still re-established and the operator should
/// see it. On failure the entry is marked disconnected, with the
/// `connected: false` event emitted once per transition — not once per
/// attempt.
async fn supervise<T, F, Fut>(
    kind: &str,
    id: Option<&str>,
    session: &DeviceSession<T>,
    event_bus: &EventBus,
    reestablish: F,
) where
    T: Device + ?Sized,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Arc<T>, String>>,
{
    if session.is_connected() {
        if let Some(device) = session.device() {
            match device.connected().await {
                Ok(true) => return,
                Ok(false) => {
                    debug!(
                        kind,
                        id, "device reports Connected=false; re-establishing the session"
                    );
                }
                Err(e) => {
                    debug!(kind, id, error = %e, "device health check failed; re-establishing the session");
                }
            }
        }
    } else {
        debug!(
            kind,
            id, "device session marked dead; running the full establish routine"
        );
    }

    let was_connected = session.is_connected();
    match reestablish().await {
        Ok(device) => {
            session.install(device);
            info!(kind, id, "device session re-established");
            emit(event_bus, kind, id, true);
        }
        Err(e) => {
            if was_connected {
                warn!(kind, id, error = %e, "device session lost and re-establish failed; retrying every pass");
                session.mark_disconnected();
                emit(event_bus, kind, id, false);
            } else {
                debug!(kind, id, error = %e, "device still unavailable");
            }
        }
    }
}

fn emit(event_bus: &EventBus, kind: &str, id: Option<&str>, connected: bool) {
    event_bus.emit(
        "equipment_changed",
        serde_json::json!({
            "kind": kind,
            "device": id,
            "connected": connected,
        }),
    );
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use axum::routing::get;
    use axum::{Json, Router};

    use super::*;
    use crate::config;
    use crate::equipment::test_support::spawn_stub;
    use crate::equipment::SafetyMonitorEntry;

    /// Shared mutable state of the stateful Alpaca stub below — the
    /// "server side" of the session, which a test flips to simulate a
    /// downstream service restart (Connected reverts to false) or an
    /// outage (every request errors).
    #[derive(Default)]
    struct StubState {
        connected: AtomicBool,
        broken: AtomicBool,
        set_connected_calls: AtomicU32,
        max_adu: AtomicU32,
    }

    fn alpaca_ok(value: serde_json::Value) -> Json<serde_json::Value> {
        Json(serde_json::json!({"Value": value, "ErrorNumber": 0, "ErrorMessage": ""}))
    }

    /// One `SafetyMonitor` at index 0 whose `Connected` state lives in
    /// `StubState` — a PUT records the call and stores the value.
    fn monitor_router(state: Arc<StubState>) -> Router {
        let put_state = state.clone();
        let get_state = state.clone();
        let devices_state = state;
        Router::new()
            .route(
                "/management/v1/configureddevices",
                get(move || {
                    let state = devices_state.clone();
                    async move {
                        if state.broken.load(Ordering::SeqCst) {
                            // An empty roster maps to the Permanent
                            // ("device not found") outcome, so the
                            // re-establish fails without the Transient
                            // path's multi-second retry backoff — keeps
                            // the failing-pass tests on real time fast.
                            return alpaca_ok(serde_json::json!([]));
                        }
                        alpaca_ok(serde_json::json!([{
                            "DeviceName": "SM", "DeviceType": "SafetyMonitor",
                            "DeviceNumber": 0, "UniqueID": "sm-0"
                        }]))
                    }
                }),
            )
            .route(
                "/api/v1/safetymonitor/0/connected",
                get(move || {
                    let state = get_state.clone();
                    async move {
                        if state.broken.load(Ordering::SeqCst) {
                            return Json(serde_json::json!({
                                "ErrorNumber": 1035, "ErrorMessage": "simulated outage"
                            }));
                        }
                        alpaca_ok(serde_json::json!(state.connected.load(Ordering::SeqCst)))
                    }
                })
                .put(move || {
                    let state = put_state.clone();
                    async move {
                        state.set_connected_calls.fetch_add(1, Ordering::SeqCst);
                        state.connected.store(true, Ordering::SeqCst);
                        Json(serde_json::json!({"ErrorNumber": 0, "ErrorMessage": ""}))
                    }
                }),
            )
    }

    fn monitor_config(url: &str) -> config::SafetyMonitorConfig {
        config::SafetyMonitorConfig {
            id: "sm-under-test".to_string(),
            alpaca_url: url.to_string(),
            device_number: 0,
            auth: None,
        }
    }

    fn registry_with_monitor(entry: SafetyMonitorEntry) -> Arc<EquipmentRegistry> {
        Arc::new(EquipmentRegistry {
            safety_monitors: vec![entry],
            ..Default::default()
        })
    }

    fn supervisor_over(equipment: Arc<EquipmentRegistry>) -> ReconnectSupervisor {
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        ReconnectSupervisor::new(equipment, event_bus, Duration::from_millis(10), None)
    }

    async fn connected_entry(url: &str) -> SafetyMonitorEntry {
        crate::equipment::safety_monitor::connect_safety_monitor(&monitor_config(url), None).await
    }

    /// A healthy session — Connected reads true — is left completely
    /// alone: no reconnect PUT beyond the startup one, no event.
    #[tokio::test]
    async fn pass_leaves_a_healthy_session_alone() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let entry = connected_entry(&stub.url()).await;
        assert!(entry.is_connected());
        assert_eq!(state.set_connected_calls.load(Ordering::SeqCst), 1);

        let supervisor = supervisor_over(registry_with_monitor(entry));
        let mut events = supervisor.event_bus.subscribe();
        supervisor.pass().await;

        assert_eq!(
            state.set_connected_calls.load(Ordering::SeqCst),
            1,
            "a healthy session must not be re-connected"
        );
        assert!(events.try_recv().is_err(), "no event for a healthy session");
    }

    /// The incident shape (#1138): the downstream service restarted, so
    /// its fresh process reports Connected=false. The pass re-issues
    /// Connected=true through the full connect routine and emits
    /// `equipment_changed` with connected=true even though the flag
    /// never observably flipped.
    #[tokio::test]
    async fn pass_reestablishes_a_session_the_service_restart_killed() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let entry = connected_entry(&stub.url()).await;

        // Simulate the restart: server-side Connected is gone.
        state.connected.store(false, Ordering::SeqCst);

        let supervisor = supervisor_over(registry_with_monitor(entry));
        let mut events = supervisor.event_bus.subscribe();
        supervisor.pass().await;

        assert!(
            state.connected.load(Ordering::SeqCst),
            "the pass must re-issue Connected=true"
        );
        assert_eq!(state.set_connected_calls.load(Ordering::SeqCst), 2);
        let entry = &supervisor.equipment.safety_monitors[0];
        assert!(entry.is_connected());
        let event = events.try_recv().expect("re-establishment must emit");
        assert_eq!(event.event, "equipment_changed");
        assert_eq!(event.payload["kind"], "safety_monitor");
        assert_eq!(event.payload["device"], "sm-under-test");
        assert_eq!(event.payload["connected"], true);
    }

    /// A device that was down at rp startup (entry registered with no
    /// session at all) is picked up once its service answers.
    #[tokio::test]
    async fn pass_picks_up_a_device_that_missed_startup() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let entry = SafetyMonitorEntry {
            id: "sm-under-test".to_string(),
            config: monitor_config(&stub.url()),
            session: DeviceSession::disconnected(),
        };

        let supervisor = supervisor_over(registry_with_monitor(entry));
        let mut events = supervisor.event_bus.subscribe();
        supervisor.pass().await;

        let entry = &supervisor.equipment.safety_monitors[0];
        assert!(entry.is_connected(), "the pass must establish the session");
        assert!(entry.device().is_some());
        let event = events.try_recv().expect("establishment must emit");
        assert_eq!(event.payload["connected"], true);
    }

    /// A disconnected entry runs the full establish routine even when
    /// some other client already turned the device back on: nothing is
    /// adopted from a session rp did not establish — `Connected = true`
    /// is re-issued (idempotent, non-actuating) and the property cache
    /// re-read, so no stale state survives.
    #[tokio::test]
    async fn pass_reestablishes_rather_than_adopting_a_foreign_session() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let entry = connected_entry(&stub.url()).await;
        assert_eq!(state.set_connected_calls.load(Ordering::SeqCst), 1);

        // The entry thinks the session is dead, but server-side
        // Connected is (still/again) true — e.g. another client
        // re-connected the device after an outage.
        entry.session.mark_disconnected();

        let supervisor = supervisor_over(registry_with_monitor(entry));
        let mut events = supervisor.event_bus.subscribe();
        supervisor.pass().await;

        let entry = &supervisor.equipment.safety_monitors[0];
        assert!(entry.is_connected(), "the session must be healthy again");
        assert_eq!(
            state.set_connected_calls.load(Ordering::SeqCst),
            2,
            "the full establish routine must run — no foreign-session adoption"
        );
        let event = events.try_recv().expect("re-establishment must emit");
        assert_eq!(event.payload["connected"], true);
    }

    /// When the health check fails and the re-establish fails too (the
    /// service is down, not merely restarted), the entry flips to
    /// disconnected with one event — and a second failing pass stays
    /// silent: once per transition, not once per attempt.
    #[tokio::test]
    async fn pass_marks_a_lost_session_disconnected_once() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let entry = connected_entry(&stub.url()).await;
        assert!(
            entry.is_connected(),
            "fixture: startup connect must succeed"
        );

        state.broken.store(true, Ordering::SeqCst);

        let supervisor = supervisor_over(registry_with_monitor(entry));
        let mut events = supervisor.event_bus.subscribe();
        supervisor.pass().await;

        let entry = &supervisor.equipment.safety_monitors[0];
        assert!(
            !entry.is_connected(),
            "a lost session must read disconnected"
        );
        assert!(
            entry.device().is_some(),
            "the stale handle stays until a successful re-establish replaces it"
        );
        let event = events.try_recv().expect("the loss must emit");
        assert_eq!(event.payload["connected"], false);

        supervisor.pass().await;
        assert!(
            events.try_recv().is_err(),
            "a still-down device must not emit again"
        );
    }

    /// A camera's invariant metadata is re-read on re-establish — the
    /// service may have come back with a different device behind the
    /// same config entry, so nothing survives from the dead session.
    #[tokio::test]
    async fn pass_rereads_camera_invariants_on_reestablish() {
        let state = Arc::new(StubState::default());
        state.max_adu.store(65535, Ordering::SeqCst);

        let devices_state = state.clone();
        let connected_get_state = state.clone();
        let connected_put_state = state.clone();
        let maxadu_state = state.clone();
        let router = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(move || {
                    let _ = &devices_state;
                    async move {
                        alpaca_ok(serde_json::json!([{
                            "DeviceName": "Cam", "DeviceType": "Camera",
                            "DeviceNumber": 0, "UniqueID": "cam-0"
                        }]))
                    }
                }),
            )
            .route(
                "/api/v1/camera/0/connected",
                get(move || {
                    let state = connected_get_state.clone();
                    async move {
                        alpaca_ok(serde_json::json!(state.connected.load(Ordering::SeqCst)))
                    }
                })
                .put(move || {
                    let state = connected_put_state.clone();
                    async move {
                        state.connected.store(true, Ordering::SeqCst);
                        Json(serde_json::json!({"ErrorNumber": 0, "ErrorMessage": ""}))
                    }
                }),
            )
            .route(
                "/api/v1/camera/0/maxadu",
                get(move || {
                    let state = maxadu_state.clone();
                    async move {
                        alpaca_ok(serde_json::json!(state.max_adu.load(Ordering::SeqCst)))
                    }
                }),
            );
        let stub = spawn_stub(router).await;

        let camera_config = config::CameraConfig {
            id: "cam-under-test".to_string(),
            name: "test".to_string(),
            alpaca_url: stub.url(),
            device_type: String::new(),
            device_number: 0,
            cooler_targets_c: Vec::new(),
            gain: None,
            offset: None,
            readout_time_estimate: None,
            auth: None,
        };
        let entry = crate::equipment::camera::connect_camera(&camera_config, None).await;
        assert_eq!(entry.invariants().max_adu, Some(65535));

        // Restart with a different sensor behind the same config entry.
        state.connected.store(false, Ordering::SeqCst);
        state.max_adu.store(4095, Ordering::SeqCst);

        let equipment = Arc::new(EquipmentRegistry {
            cameras: vec![entry],
            ..Default::default()
        });
        let supervisor = supervisor_over(equipment);
        supervisor.pass().await;

        let entry = &supervisor.equipment.cameras[0];
        assert!(entry.is_connected());
        assert_eq!(
            entry.invariants().max_adu,
            Some(4095),
            "invariants must be re-read from the fresh session, not assumed"
        );
    }

    /// The loop wakes on its interval and heals; cancellation stops it.
    #[tokio::test]
    async fn run_heals_on_interval_and_stops_on_cancel() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let entry = SafetyMonitorEntry {
            id: "sm-under-test".to_string(),
            config: monitor_config(&stub.url()),
            session: DeviceSession::disconnected(),
        };
        let equipment = registry_with_monitor(entry);
        let supervisor = supervisor_over(equipment.clone());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(supervisor.run(cancel.clone()));

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !equipment.safety_monitors[0].is_connected() {
            assert!(
                std::time::Instant::now() < deadline,
                "the run loop never healed the session"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        cancel.cancel();
        task.await.expect("supervisor task must exit on cancel");
    }
}
