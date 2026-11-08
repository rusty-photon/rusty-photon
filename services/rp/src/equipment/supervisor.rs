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
//! cache. Nothing is carried over from a dead session. A pinned entry's
//! healthy session is also re-checked against its server's listing, and
//! a session or handle its pin refuses is dropped (rp.md § Device
//! Identity Pin).
//!
//! Tenet 3 holds on this path exactly as on first connect: everything
//! here re-*reads* state; `Connected = true` is non-actuating by driver
//! contract, except phd2-guider's camera facade, whose connect may take
//! a 1 ms frame (rp.md § Device Session Recovery).
//!
//! An entry `disconnect_device` holds out is skipped until
//! `connect_device` releases it, and both of those tools live here too
//! ([`disconnect_entry`], [`connect_entry`]; rp.md § Power Switching).
//!
//! The same loop carries the tool-provider lane
//! ([`crate::mcp::providers::Providers::pass`]): a provider's MCP
//! session is health-checked and re-dialed on the same cadence, with
//! `provider_changed` where a device emits `equipment_changed`.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::Device;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::alpaca::ConnectBudget;
use super::binding::{check_pin, EstablishError, PinCheck, RosterEntry};
use super::session::DeviceSession;
use super::{
    camera, cover_calibrator, dome, filter_wheel, focuser, mount, observing_conditions, rotator,
    safety_monitor, switch, with_session, EntryRef, EquipmentRegistry,
};
use crate::events::EventBus;

pub struct ReconnectSupervisor {
    equipment: Arc<EquipmentRegistry>,
    event_bus: Arc<EventBus>,
    interval: Duration,
    ca_cert_path: Option<PathBuf>,
    /// The tool providers to supervise alongside the devices; empty
    /// (no lane) until [`Self::with_providers`].
    providers: Arc<crate::mcp::providers::Providers>,
}

impl ReconnectSupervisor {
    #[must_use]
    pub fn new(
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
            providers: Arc::new(crate::mcp::providers::Providers::none()),
        }
    }

    /// Add the tool-provider lane (rp.md § Device Session Recovery):
    /// every pass health-checks and re-dials the providers after the
    /// devices.
    #[must_use]
    pub fn with_providers(mut self, providers: Arc<crate::mcp::providers::Providers>) -> Self {
        self.providers = providers;
        self
    }

    /// Poll until cancelled (rp shutdown). The startup connect just
    /// ran, so the first pass waits one full interval.
    ///
    /// Cancellation also preempts a pass in flight — a pass can spend
    /// many seconds in per-device connect retries, and rp's shutdown
    /// joins this task, so waiting the pass out would stall shutdown.
    /// Dropping the pass mid-await only abandons an in-flight read or
    /// `Connected = true` request; nothing here actuates hardware but the
    /// 1 ms frame a phd2-guider facade connect may take (rp.md § Device
    /// Session Recovery).
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
            tokio::select! {
                () = cancel.cancelled() => {
                    debug!("equipment reconnect supervisor stopped mid-pass");
                    return;
                }
                () = self.pass() => {}
            }
        }
    }

    /// One supervision pass over every configured device, in
    /// [`EquipmentRegistry::entries`] order, then every tool provider.
    pub(crate) async fn pass(&self) {
        let ca = self.ca_cert_path.as_deref();
        for entry in self.equipment.entries() {
            supervise_entry(entry, ca, &self.event_bus, None).await;
        }
        self.providers.pass().await;
    }
}

/// What one supervision of an entry came to.
#[derive(Debug)]
pub(crate) enum Supervised {
    /// The entry is held out, so nothing was tried.
    HeldOut,
    /// The live session read healthy (and its pin held), and was left
    /// as it is.
    Healthy,
    /// A fresh session was established. `live` is false when the entry
    /// was held out while it connected: the session was kept for the
    /// disconnect, not installed live.
    Reestablished { live: bool },
    /// The session, or the attempt to establish one, failed; the entry
    /// is disconnected. The reason as text.
    Failed(String),
}

/// [`supervise_with_metadata`] for one entry of any kind, with that
/// kind's establish routine. `forced` replaces the budget the entry's
/// own record would pick (see [`supervise_with_metadata`]).
async fn supervise_entry(
    entry: EntryRef<'_>,
    ca: Option<&Path>,
    event_bus: &EventBus,
    forced: Option<ConnectBudget>,
) -> Supervised {
    let kind = entry.kind();
    match entry {
        EntryRef::Camera(e) => {
            supervise_with_metadata(kind, &e.config, ca, &e.session, event_bus, forced, |b| {
                camera::establish_camera(&e.config, ca, b)
            })
            .await
        }
        EntryRef::FilterWheel(e) => {
            supervise_with_metadata(kind, &e.config, ca, &e.session, event_bus, forced, |b| {
                filter_wheel::establish_filter_wheel(&e.config, ca, b)
            })
            .await
        }
        EntryRef::CoverCalibrator(e) => {
            supervise(kind, &e.config, ca, &e.session, event_bus, forced, |b| {
                cover_calibrator::establish_cover_calibrator(&e.config, ca, b)
            })
            .await
        }
        EntryRef::Focuser(e) => {
            supervise_with_metadata(kind, &e.config, ca, &e.session, event_bus, forced, |b| {
                focuser::establish_focuser(&e.config, ca, b)
            })
            .await
        }
        EntryRef::SafetyMonitor(e) => {
            supervise(kind, &e.config, ca, &e.session, event_bus, forced, |b| {
                safety_monitor::establish_safety_monitor(&e.config, ca, b)
            })
            .await
        }
        EntryRef::Switch(e) => {
            supervise(kind, &e.config, ca, &e.session, event_bus, forced, |b| {
                switch::establish_switch(&e.config, ca, b)
            })
            .await
        }
        EntryRef::Rotator(e) => {
            supervise_with_metadata(kind, &e.config, ca, &e.session, event_bus, forced, |b| {
                rotator::establish_rotator(&e.config, ca, b)
            })
            .await
        }
        EntryRef::ObservingConditions(e) => {
            supervise(kind, &e.config, ca, &e.session, event_bus, forced, |b| {
                observing_conditions::establish_observing_conditions(&e.config, ca, b)
            })
            .await
        }
        EntryRef::Dome(e) => {
            supervise(kind, &e.config, ca, &e.session, event_bus, forced, |b| {
                dome::establish_dome(&e.config, ca, b)
            })
            .await
        }
        EntryRef::Mount(e) => {
            supervise(kind, &e.config, ca, &e.session, event_bus, forced, |b| {
                mount::establish_mount(&e.config, ca, b)
            })
            .await
        }
    }
}

/// `connect_device` (rp.md § Power Switching): release the entry's
/// hold-out, then supervise it at once with the connect routine's full
/// budget. A live session that reads healthy is left as it is; any
/// other is established afresh. The release happens under the
/// registry's power lock, so it cannot land between a `set_switch`'s
/// hold-out check and its write.
pub(crate) async fn connect_entry(
    equipment: &EquipmentRegistry,
    entry: EntryRef<'_>,
    event_bus: &EventBus,
) -> Supervised {
    {
        let _power = equipment.power.lock().await;
        with_session!(entry, session => session.release());
    }
    debug!(
        kind = entry.kind(),
        id = entry.id(),
        "hold-out released by connect_device"
    );
    supervise_entry(
        entry,
        equipment.ca_cert_path.as_deref(),
        event_bus,
        Some(ConnectBudget::Full),
    )
    .await
}

/// The deadline `disconnect_device` gives a driver's close (rp.md
/// § Power Switching): a close takes well under a second, and a wedged
/// camera can hang it outright.
pub(crate) const DISCONNECT_DEADLINE: Duration = Duration::from_secs(10);

/// How a `disconnect_device`'s `Connected = false` ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Close {
    /// The driver acknowledged it within the deadline.
    Closed,
    /// The deadline passed first.
    TimedOut,
    /// The driver, or the transport, failed it; the reason as text.
    Failed(String),
    /// rp has never reached the device and holds no handle to send it
    /// through.
    NoSession,
}

/// `disconnect_device` (rp.md § Power Switching): hold the entry out,
/// which also marks it disconnected, emit `equipment_changed` with
/// `connected: false` if its session was live, then send the driver
/// `Connected = false` within `deadline`. A connect routine running for
/// the entry is waited out first, inside the deadline, so the
/// disconnect is the last request rp sends the driver. The entry stays
/// held out however the close ends.
pub(crate) async fn disconnect_entry(
    entry: EntryRef<'_>,
    event_bus: &EventBus,
    deadline: Duration,
) -> Close {
    let kind = entry.kind();
    let id = entry.id();
    if with_session!(entry, session => session.hold_out()) {
        emit(event_bus, kind, entry.event_id(), false);
    }
    let close = tokio::time::timeout(deadline, async {
        with_session!(entry, session => close_session(session).await)
    })
    .await
    .unwrap_or(Close::TimedOut);
    match &close {
        Close::Closed | Close::NoSession => {
            info!(kind, id, "device disconnected through rp; held out of the reconnect supervisor until connect_device");
        }
        Close::TimedOut => {
            warn!(kind, id, deadline = ?deadline, "the driver's close did not return within the deadline; the device is held out all the same");
        }
        Close::Failed(e) => {
            warn!(kind, id, error = %e, "the driver's close failed; the device is held out all the same");
        }
    }
    close
}

/// Send `Connected = false` through the handle the slot keeps, once any
/// connect routine running for it has finished.
async fn close_session<T: ?Sized + Device, M: Send + Sync>(session: &DeviceSession<T, M>) -> Close {
    let _connecting = session.connecting().lock().await;
    match session.kept_device() {
        None => Close::NoSession,
        Some(device) => match device.set_connected(false).await {
            Ok(()) => Close::Closed,
            Err(e) => Close::Failed(e.to_string()),
        },
    }
}

/// Health-check one entry's session and re-establish it when dead, for
/// a device kind that caches nothing about its session.
///
/// The metadata-free half of [`supervise_with_metadata`], which
/// carries the documentation for both.
async fn supervise<C, F, Fut>(
    kind: &str,
    config: &C,
    ca: Option<&Path>,
    session: &DeviceSession<C::Device>,
    event_bus: &EventBus,
    forced: Option<ConnectBudget>,
    reestablish: F,
) -> Supervised
where
    C: RosterEntry,
    F: FnOnce(ConnectBudget) -> Fut,
    Fut: Future<Output = Result<Arc<C::Device>, EstablishError>>,
{
    supervise_with_metadata(
        kind,
        config,
        ca,
        session,
        event_bus,
        forced,
        |budget| async move { reestablish(budget).await.map(|device| (device, ())) },
    )
    .await
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
///
/// The entry is marked disconnected before `reestablish` runs. The
/// routine issues `Connected = true` before it hands back the new
/// handle, and from that moment a read through the old handle reaches
/// the new session; a slot still reading connected with the old handle
/// would let a reader take that answer for the old session's.
///
/// `reestablish` hands back the handle *with* the metadata read from
/// it, and the two install as one step — so a reader that takes them
/// together, through [`DeviceSession::snapshot`], always gets one
/// session's pair, whichever pass lands underneath it (rp.md § Device
/// Session Recovery). A reader that takes the two halves through their
/// separate accessors still straddles this install; that is what
/// `snapshot` is for.
///
/// `reestablish` is handed the attempts it may make: the full budget for
/// a session that was live until this pass, a single attempt for an
/// entry that was already disconnected (rp.md § Device Session
/// Recovery), unless `forced` names the budget, as `connect_device`
/// does.
///
/// A held-out entry is not tried at all (rp.md § Power Switching). The
/// check runs again once the entry's connect lock is taken, and once
/// more after the health check, so a hold-out that lands while
/// `Connected` is read stops the pass before it reconnects. The routine
/// holds the lock throughout, so a `disconnect_device` waits for it,
/// and a hold-out that lands while the establish routine runs keeps the
/// new session out of the slot's live state
/// ([`DeviceSession::install`]).
///
/// On success the fresh handle is installed and an `equipment_changed`
/// event with `connected: true` is emitted unconditionally: a service
/// that bounced between two passes drew no `connected: false` event,
/// but the session was still re-established and the operator should
/// see it. On failure the entry stays disconnected, with the
/// `connected: false` event and an `info!` line emitted once per
/// transition — not once per attempt — and each later failed pass
/// logged at `debug!`.
///
/// A session that reads healthy is then held to the entry's identity
/// pin by [`check_pin`] (rp.md § Device Identity Pin), which this runs
/// itself from the entry's `config`, so no kind can skip it: a healthy
/// `Connected` through rp's handle says nothing about *which* device
/// now answers at the entry's number. When the pin no longer holds, the
/// session and its handle are dropped at once, as they are whenever the
/// re-establish routine itself refuses the device on identity: the
/// handle is known to address another device, so no tool call may
/// reach it.
async fn supervise_with_metadata<C, M, F, Fut>(
    kind: &str,
    config: &C,
    ca: Option<&Path>,
    session: &DeviceSession<C::Device, M>,
    event_bus: &EventBus,
    forced: Option<ConnectBudget>,
    reestablish: F,
) -> Supervised
where
    C: RosterEntry,
    M: Default + Send + Sync,
    F: FnOnce(ConnectBudget) -> Fut,
    Fut: Future<Output = Result<(Arc<C::Device>, M), EstablishError>>,
{
    let address = config.address();
    let id = address.id;
    if session.is_held_out() {
        return Supervised::HeldOut;
    }
    let _connecting = session.connecting().lock().await;
    if session.is_held_out() {
        return Supervised::HeldOut;
    }
    if session.is_connected() {
        if let Some(device) = session.device() {
            match device.connected().await {
                Ok(true) => match check_pin(config, ca).await {
                    PinCheck::Holds => return Supervised::Healthy,
                    PinCheck::Unchecked(e) => {
                        debug!(kind, id, error = %e, "could not re-check the pinned UniqueID; keeping the session it was checked against at establish");
                        return Supervised::Healthy;
                    }
                    PinCheck::Refused(e) => {
                        session.retire();
                        warn!(kind, id, error = %e, "the device at this entry's number no longer reports its pinned UniqueID; dropped the session and its handle");
                        emit(event_bus, kind, id, false);
                        return Supervised::Failed(e);
                    }
                },
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

    // A disconnect_device can hold the entry out while the health check
    // reads; it has emitted the loss itself, and the entry is not to be
    // reconnected.
    if session.is_held_out() {
        return Supervised::HeldOut;
    }

    // rp's own record of the previous pass decides the budget, so no
    // driver's error code has to be trusted: a session live until this
    // pass gets the full routine, whose retries ride out a stall, and an
    // entry that was already disconnected gets one attempt.
    let was_connected = session.is_connected();
    let budget = forced.unwrap_or(if was_connected {
        ConnectBudget::Full
    } else {
        ConnectBudget::Single
    });
    session.mark_disconnected();
    match reestablish(budget).await {
        Ok((device, metadata)) => {
            let live = session.install(device, metadata);
            if live {
                // The establish routine has just logged the bound device
                // at info; this line only marks the install.
                debug!(kind, id, "device session re-established");
                emit(event_bus, kind, id, true);
            } else {
                // A disconnect_device held the entry out while the
                // routine ran; it emitted nothing, since the session read
                // disconnected by then, so the loss is announced here.
                debug!(kind, id, "device held out while it connected; the session is kept for the disconnect, not installed live");
                if was_connected {
                    emit(event_bus, kind, id, false);
                }
            }
            Supervised::Reestablished { live }
        }
        Err(e) => {
            if e.is_identity_refusal() {
                session.retire();
            }
            // One line at default verbosity per transition to
            // unreachable: a device switched off for the day is no
            // fault, and logs nothing more until it answers again. An
            // entry a disconnect_device held out while the routine ran is
            // not tried again, so it says only that the routine failed;
            // its loss is still announced here, since the hold-out found
            // the session already marked dead and emitted nothing.
            if session.is_held_out() {
                debug!(kind, id, error = %e, "device held out while it connected; the connect failed and the entry is not tried again until connect_device");
            } else if was_connected {
                info!(kind, id, error = %e, "device session lost and re-establish failed; trying once every pass, logged at debug, until it answers");
            } else {
                debug!(kind, id, error = %e, "device still unavailable");
            }
            if was_connected {
                emit(event_bus, kind, id, false);
            }
            Supervised::Failed(e.to_string())
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
        /// When set, the next `GET /connected` parks forever — a device
        /// that accepts the request and never answers, wedging a pass
        /// inside its health check.
        hang_connected: AtomicBool,
        /// How many probes have parked on `hang_connected`.
        hung_probes: AtomicU32,
        set_connected_calls: AtomicU32,
        width: AtomicU32,
        /// When set, the listing names another device at number 0 — a
        /// driver that came back with its devices reordered — while
        /// `Connected` keeps its own state.
        swapped: AtomicBool,
        /// When set, the listing answers an Alpaca error while
        /// `Connected` keeps answering: the pin cannot be re-checked.
        listing_unreadable: AtomicBool,
        /// `configureddevices` reads served.
        listing_reads: AtomicU32,
        /// When set, the driver answers but its device is switched off:
        /// `Connected = true` fails with an Alpaca error, a transient
        /// failure for the connect routine, while the listing keeps
        /// answering. Each refused request is still counted in
        /// `set_connected_calls`.
        device_off: AtomicBool,
        /// `Connected = false` requests taken (also counted in
        /// `set_connected_calls`).
        disconnect_calls: AtomicU32,
        /// When set, a `Connected = false` request parks forever: a
        /// driver whose close hangs, as a wedged camera's does.
        hang_disconnect: AtomicBool,
        /// When set, the next `GET /connected` parks until the test
        /// signals `release_probe`, then answers `connected` as it
        /// stands by then. Counted in `hung_probes` while parked.
        park_probe: AtomicBool,
        release_probe: tokio::sync::Notify,
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
                        state.listing_reads.fetch_add(1, Ordering::SeqCst);
                        if state.listing_unreadable.load(Ordering::SeqCst) {
                            return Json(serde_json::json!({
                                "ErrorNumber": 1280, "ErrorMessage": "simulated listing failure"
                            }));
                        }
                        if state.swapped.load(Ordering::SeqCst) {
                            return alpaca_ok(serde_json::json!([{
                                "DeviceName": "SM", "DeviceType": "SafetyMonitor",
                                "DeviceNumber": 0, "UniqueID": "sm-other"
                            }]));
                        }
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
                        if state.hang_connected.load(Ordering::SeqCst) {
                            state.hung_probes.fetch_add(1, Ordering::SeqCst);
                            std::future::pending::<()>().await;
                        }
                        if state.park_probe.swap(false, Ordering::SeqCst) {
                            state.hung_probes.fetch_add(1, Ordering::SeqCst);
                            state.release_probe.notified().await;
                        }
                        alpaca_ok(serde_json::json!(state.connected.load(Ordering::SeqCst)))
                    }
                })
                .put(move |form: axum::Form<Vec<(String, String)>>| {
                    let state = put_state.clone();
                    async move {
                        state.set_connected_calls.fetch_add(1, Ordering::SeqCst);
                        let requested = form
                            .0
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case("Connected"))
                            .is_none_or(|(_, v)| v.eq_ignore_ascii_case("true"));
                        if !requested {
                            state.disconnect_calls.fetch_add(1, Ordering::SeqCst);
                            if state.hang_disconnect.load(Ordering::SeqCst) {
                                std::future::pending::<()>().await;
                            }
                        }
                        if state.device_off.load(Ordering::SeqCst) {
                            return Json(serde_json::json!({
                                "ErrorNumber": 1031, "ErrorMessage": "the device does not answer"
                            }));
                        }
                        state.connected.store(requested, Ordering::SeqCst);
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
            unique_id: None,
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

    /// A monitor entry pinned to the `UniqueID` the stub lists while
    /// unswapped, connected at startup.
    async fn pinned_entry(url: &str) -> SafetyMonitorEntry {
        let config = config::SafetyMonitorConfig {
            unique_id: Some(config::UniqueIdPin::try_new("sm-0".to_string()).unwrap()),
            ..monitor_config(url)
        };
        crate::equipment::safety_monitor::connect_safety_monitor(&config, None).await
    }

    /// The driver came back with another device at the pinned entry's
    /// number, and some other client has switched it on: `Connected`
    /// reads true through rp's handle, so only the pin re-check sees
    /// the swap. The session and its handle go at once, with one event,
    /// and nothing is switched on.
    #[tokio::test]
    async fn pass_drops_a_healthy_pinned_session_whose_number_lists_another_device() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let entry = pinned_entry(&stub.url()).await;
        assert!(entry.is_connected(), "fixture: the pin matches at startup");
        state.swapped.store(true, Ordering::SeqCst);

        let supervisor = supervisor_over(registry_with_monitor(entry));
        let mut events = supervisor.event_bus.subscribe();
        supervisor.pass().await;

        let entry = &supervisor.equipment.safety_monitors[0];
        assert!(!entry.is_connected(), "a refused pin must end the session");
        assert!(
            entry.device().is_none(),
            "the handle addresses another device now, so it must be dropped"
        );
        let event = events.try_recv().expect("the loss must emit");
        assert_eq!(event.payload["connected"], false);
        assert_eq!(
            state.set_connected_calls.load(Ordering::SeqCst),
            1,
            "only the startup connect may have switched a device on"
        );
    }

    /// A pinned session that reads healthy is kept when its listing
    /// cannot be read: it was checked at establish, and a failed read is
    /// no evidence of a swap.
    #[tokio::test]
    async fn pass_keeps_a_healthy_pinned_session_whose_listing_cannot_be_read() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let entry = pinned_entry(&stub.url()).await;
        let reads_at_startup = state.listing_reads.load(Ordering::SeqCst);
        state.listing_unreadable.store(true, Ordering::SeqCst);

        let supervisor = supervisor_over(registry_with_monitor(entry));
        let mut events = supervisor.event_bus.subscribe();
        supervisor.pass().await;

        assert!(
            state.listing_reads.load(Ordering::SeqCst) > reads_at_startup,
            "the pass must have tried to re-check the pin"
        );
        let entry = &supervisor.equipment.safety_monitors[0];
        assert!(entry.is_connected());
        assert!(entry.device().is_some());
        assert!(
            events.try_recv().is_err(),
            "nothing changed, so nothing is emitted"
        );
    }

    /// An unpinned healthy session costs no listing read: its behaviour
    /// is exactly what it was before pins existed.
    #[tokio::test]
    async fn pass_does_not_read_the_listing_for_a_healthy_unpinned_session() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let entry = connected_entry(&stub.url()).await;
        let reads_at_startup = state.listing_reads.load(Ordering::SeqCst);

        let supervisor = supervisor_over(registry_with_monitor(entry));
        supervisor.pass().await;

        assert_eq!(state.listing_reads.load(Ordering::SeqCst), reads_at_startup);
    }

    /// A re-establish the pin refuses drops the dead session's stale
    /// handle too: it addresses the device now at the entry's number.
    #[tokio::test]
    async fn pass_drops_the_stale_handle_when_the_re_establish_is_refused_on_identity() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let entry = pinned_entry(&stub.url()).await;
        // The service restarted (Connected back to false) with another
        // device listed at the entry's number.
        state.connected.store(false, Ordering::SeqCst);
        state.swapped.store(true, Ordering::SeqCst);

        let supervisor = supervisor_over(registry_with_monitor(entry));
        supervisor.pass().await;

        let entry = &supervisor.equipment.safety_monitors[0];
        assert!(!entry.is_connected());
        assert!(entry.device().is_none(), "the stale handle must be dropped");
        assert_eq!(
            state.set_connected_calls.load(Ordering::SeqCst),
            1,
            "the refused device must not be switched on"
        );
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
    /// `equipment_changed` with connected=true even though no
    /// connected=false event was emitted for the bounce.
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

    /// The establish routine turns the replacement session on at the
    /// device before it hands back the new handle, so the slot must
    /// already read disconnected while the routine runs: a reader that
    /// took the old handle then would reach the new session through it
    /// and take the answer for the old session's.
    #[tokio::test]
    async fn a_dead_session_reads_disconnected_while_it_is_re_established() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let entry = connected_entry(&stub.url()).await;
        state.connected.store(false, Ordering::SeqCst);

        let event_bus = EventBus::from_config(&[], None).unwrap();
        let connected_during_establish = AtomicBool::new(true);
        let probe = &connected_during_establish;
        let watched = &entry;
        supervise(
            "safety_monitor",
            &entry.config,
            None,
            &entry.session,
            &event_bus,
            None,
            |budget| async move {
                probe.store(watched.is_connected(), Ordering::SeqCst);
                crate::equipment::safety_monitor::establish_safety_monitor(
                    &watched.config,
                    None,
                    budget,
                )
                .await
            },
        )
        .await;

        assert!(
            !connected_during_establish.load(Ordering::SeqCst),
            "the slot must read disconnected before the establish routine runs"
        );
        assert!(
            entry.is_connected(),
            "the re-established session is live once installed"
        );
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

    /// A session that was live until this pass and is now lost (its
    /// service restarted with the device switched off) gets the full
    /// connect routine on its first failing pass: the retries ride out a
    /// stall, and a lost session is the case they exist for.
    #[tokio::test]
    async fn a_session_lost_on_this_pass_gets_every_connect_attempt() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let entry = connected_entry(&stub.url()).await;
        assert_eq!(
            state.set_connected_calls.load(Ordering::SeqCst),
            1,
            "fixture: the startup connect is the only one so far"
        );
        state.connected.store(false, Ordering::SeqCst);
        state.device_off.store(true, Ordering::SeqCst);

        let supervisor = supervisor_over(registry_with_monitor(entry));
        let mut events = supervisor.event_bus.subscribe();
        supervisor.pass().await;

        assert_eq!(
            state.set_connected_calls.load(Ordering::SeqCst),
            1 + crate::equipment::alpaca::CONNECT_ATTEMPTS,
            "a session lost on this pass must get every attempt of the routine"
        );
        assert!(!supervisor.equipment.safety_monitors[0].is_connected());
        let event = events.try_recv().expect("the loss must emit");
        assert_eq!(event.payload["connected"], false);
    }

    /// An entry rp's own record already holds disconnected — its session
    /// was lost on an earlier pass, and its device is still switched off
    /// — gets one attempt a pass: the pass walks the devices one after
    /// another, so backoff for it would delay every other device's check.
    #[tokio::test]
    async fn an_entry_disconnected_before_the_pass_gets_one_connect_attempt() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let entry = connected_entry(&stub.url()).await;
        entry.session.mark_disconnected();
        state.connected.store(false, Ordering::SeqCst);
        state.device_off.store(true, Ordering::SeqCst);
        let before = state.set_connected_calls.load(Ordering::SeqCst);

        let supervisor = supervisor_over(registry_with_monitor(entry));
        let mut events = supervisor.event_bus.subscribe();
        supervisor.pass().await;

        assert_eq!(
            state.set_connected_calls.load(Ordering::SeqCst) - before,
            1,
            "an entry already disconnected must get one attempt, not the full routine"
        );
        assert!(!supervisor.equipment.safety_monitors[0].is_connected());
        assert!(
            events.try_recv().is_err(),
            "no transition, so nothing is emitted"
        );
    }

    /// A camera's invariant metadata is re-read on re-establish — the
    /// service may have come back with a different device behind the
    /// same config entry, so nothing survives from the dead session.
    #[tokio::test]
    async fn pass_rereads_camera_invariants_on_reestablish() {
        let state = Arc::new(StubState::default());
        state.width.store(6248, Ordering::SeqCst);

        let devices_state = state.clone();
        let connected_get_state = state.clone();
        let connected_put_state = state.clone();
        let width_state = state.clone();
        let router = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(move || {
                    let state = devices_state.clone();
                    async move {
                        // The restart below puts a different camera
                        // behind the same config entry, and the roster
                        // is where that shows: each session's handle
                        // carries the `UniqueID` it was enumerated
                        // with, which is what lets the assertions tell
                        // the two handles apart.
                        let unique_id = if state.width.load(Ordering::SeqCst) == 6248 {
                            "cam-0"
                        } else {
                            "cam-1"
                        };
                        alpaca_ok(serde_json::json!([{
                            "DeviceName": "Cam", "DeviceType": "Camera",
                            "DeviceNumber": 0, "UniqueID": unique_id
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
                "/api/v1/camera/0/cameraxsize",
                get(move || {
                    let state = width_state.clone();
                    async move {
                        alpaca_ok(serde_json::json!(state.width.load(Ordering::SeqCst)))
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
            unique_id: None,
            cooler_targets_c: Vec::new(),
            gain: None,
            offset: None,
            readout_time_estimate: None,
            auth: None,
        };
        let entry = crate::equipment::camera::connect_camera(&camera_config, None).await;
        assert_eq!(entry.invariants().sensor_width_px, Some(6248));
        let dead_handle = entry
            .device()
            .expect("the first connect must yield a handle");
        assert_eq!(dead_handle.unique_id(), "cam-0");

        // Restart with a different sensor behind the same config entry.
        state.connected.store(false, Ordering::SeqCst);
        state.width.store(3096, Ordering::SeqCst);

        let equipment = Arc::new(EquipmentRegistry {
            cameras: vec![entry],
            ..Default::default()
        });
        let supervisor = supervisor_over(equipment);
        supervisor.pass().await;

        let entry = &supervisor.equipment.cameras[0];
        assert!(entry.is_connected());
        assert_eq!(
            entry.invariants().sensor_width_px,
            Some(3096),
            "invariants must be re-read from the fresh session, not assumed"
        );
        // The pair, not just its metadata half: fresh invariants beside
        // the dead session's handle is exactly the tear this change
        // exists to prevent, and an assertion on the width alone would
        // pass on it.
        let (handle, paired) = entry
            .snapshot()
            .expect("a re-established session holds a handle");
        assert_eq!(
            handle.unique_id(),
            "cam-1",
            "the snapshot must hold the handle this pass enumerated, not the dead session's"
        );
        assert_eq!(
            paired.sensor_width_px,
            Some(3096),
            "the handle and its invariants install together, so a snapshot pairs the fresh two"
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

    /// Cancellation preempts a pass wedged inside a health check —
    /// shutdown must not wait out a slow device (the per-request read
    /// timeout alone is 10 s, and a pass visits every device).
    #[tokio::test]
    async fn run_cancellation_preempts_a_wedged_pass() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let entry = connected_entry(&stub.url()).await;
        assert!(
            entry.is_connected(),
            "fixture: startup connect must succeed"
        );

        // Wedge the health check: the next GET /connected parks forever.
        state.hang_connected.store(true, Ordering::SeqCst);

        let supervisor = supervisor_over(registry_with_monitor(entry));
        let cancel = CancellationToken::new();
        let task = tokio::spawn(supervisor.run(cancel.clone()));

        // Wait until the pass has actually parked inside the probe.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while state.hung_probes.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the pass never reached the wedged health check"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("cancellation must preempt the wedged pass, not wait it out")
            .expect("supervisor task must not panic");
    }

    /// A disconnect holds the entry out, closes the device, and from then
    /// on no pass health-checks or reconnects it.
    #[tokio::test]
    async fn a_held_out_entry_is_neither_health_checked_nor_reconnected() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let equipment = registry_with_monitor(connected_entry(&stub.url()).await);
        let event_bus = EventBus::from_config(&[], None).unwrap();
        let entry = equipment.entry("sm-under-test").unwrap();

        let close = disconnect_entry(entry, &event_bus, DISCONNECT_DEADLINE).await;

        assert_eq!(close, Close::Closed);
        assert!(!state.connected.load(Ordering::SeqCst));
        let calls = state.set_connected_calls.load(Ordering::SeqCst);
        let supervisor = supervisor_over(equipment.clone());
        supervisor.pass().await;
        supervisor.pass().await;
        assert_eq!(state.set_connected_calls.load(Ordering::SeqCst), calls);
        assert!(!state.connected.load(Ordering::SeqCst));
        let entry = equipment.entry("sm-under-test").unwrap();
        assert!(entry.is_held_out());
        assert!(!entry.is_connected());
        assert!(equipment.safety_monitors[0].device().is_none());
    }

    /// A hold-out that lands while the pass reads `Connected` stops the
    /// pass from reconnecting the entry, even when the read then finds
    /// the device disconnected: the only request the device gets is the
    /// disconnect's own.
    #[tokio::test]
    async fn a_hold_out_during_a_health_check_stops_the_pass_reconnecting() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let equipment = registry_with_monitor(connected_entry(&stub.url()).await);
        let calls = state.set_connected_calls.load(Ordering::SeqCst);
        state.park_probe.store(true, Ordering::SeqCst);
        let supervisor = supervisor_over(equipment.clone());
        let pass = tokio::spawn(async move { supervisor.pass().await });
        while state.hung_probes.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let held = equipment.clone();
        let disconnect = tokio::spawn(async move {
            let event_bus = EventBus::from_config(&[], None).unwrap();
            let entry = held.entry("sm-under-test").unwrap();
            disconnect_entry(entry, &event_bus, DISCONNECT_DEADLINE).await
        });
        while !equipment.entry("sm-under-test").unwrap().is_held_out() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        state.connected.store(false, Ordering::SeqCst);
        state.release_probe.notify_one();
        pass.await.unwrap();

        assert_eq!(disconnect.await.unwrap(), Close::Closed);
        assert_eq!(state.set_connected_calls.load(Ordering::SeqCst), calls + 1);
        assert_eq!(state.disconnect_calls.load(Ordering::SeqCst), 1);
        assert!(!state.connected.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_close_past_the_deadline_leaves_the_entry_held_out() {
        let state = Arc::new(StubState::default());
        state.hang_disconnect.store(true, Ordering::SeqCst);
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let equipment = registry_with_monitor(connected_entry(&stub.url()).await);
        let event_bus = EventBus::from_config(&[], None).unwrap();
        let entry = equipment.entry("sm-under-test").unwrap();

        let close = disconnect_entry(entry, &event_bus, Duration::from_millis(200)).await;

        assert_eq!(close, Close::TimedOut);
        assert_eq!(state.disconnect_calls.load(Ordering::SeqCst), 1);
        assert!(entry.is_held_out());
        assert!(!entry.is_connected());
    }

    #[tokio::test]
    async fn a_failed_close_leaves_the_entry_held_out() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let equipment = registry_with_monitor(connected_entry(&stub.url()).await);
        state.device_off.store(true, Ordering::SeqCst);
        let event_bus = EventBus::from_config(&[], None).unwrap();
        let entry = equipment.entry("sm-under-test").unwrap();

        let close = disconnect_entry(entry, &event_bus, DISCONNECT_DEADLINE).await;

        assert!(
            matches!(&close, Close::Failed(e) if e.contains("the device does not answer")),
            "{close:?}"
        );
        assert!(entry.is_held_out());
    }

    #[tokio::test]
    async fn an_entry_rp_never_reached_has_no_session_to_close() {
        let equipment = registry_with_monitor(SafetyMonitorEntry {
            id: "sm-under-test".to_string(),
            config: monitor_config("http://127.0.0.1:1"),
            session: DeviceSession::disconnected(),
        });
        let event_bus = EventBus::from_config(&[], None).unwrap();
        let entry = equipment.entry("sm-under-test").unwrap();

        assert_eq!(
            disconnect_entry(entry, &event_bus, DISCONNECT_DEADLINE).await,
            Close::NoSession
        );
        assert!(entry.is_held_out());
    }

    /// A connect routine holds the entry's connect lock, so a disconnect
    /// waits for it rather than racing its `Connected = true` to the
    /// device; here the routine never ends, and the disconnect's
    /// deadline passes without a request sent.
    #[tokio::test]
    async fn a_disconnect_waits_out_a_connect_routine_in_flight() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let equipment = registry_with_monitor(connected_entry(&stub.url()).await);
        state.hang_connected.store(true, Ordering::SeqCst);
        let supervisor = supervisor_over(equipment.clone());
        let pass = tokio::spawn(async move { supervisor.pass().await });
        while state.hung_probes.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let event_bus = EventBus::from_config(&[], None).unwrap();
        let entry = equipment.entry("sm-under-test").unwrap();

        let close = disconnect_entry(entry, &event_bus, Duration::from_millis(200)).await;

        assert_eq!(close, Close::TimedOut);
        assert_eq!(state.disconnect_calls.load(Ordering::SeqCst), 0);
        assert!(entry.is_held_out());
        pass.abort();
    }

    #[tokio::test]
    async fn connect_entry_releases_the_hold_out_and_connects_at_once() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let equipment = registry_with_monitor(connected_entry(&stub.url()).await);
        let event_bus = EventBus::from_config(&[], None).unwrap();
        let entry = equipment.entry("sm-under-test").unwrap();
        disconnect_entry(entry, &event_bus, DISCONNECT_DEADLINE).await;

        let outcome = connect_entry(&equipment, entry, &event_bus).await;

        assert!(
            matches!(outcome, Supervised::Reestablished { live: true }),
            "{outcome:?}"
        );
        assert!(!entry.is_held_out());
        assert!(entry.is_connected());
        assert!(state.connected.load(Ordering::SeqCst));
    }

    /// A connect through rp never cycles a session: a healthy one is
    /// confirmed and left as it is.
    #[tokio::test]
    async fn connect_entry_leaves_a_healthy_session_as_it_is() {
        let state = Arc::new(StubState::default());
        let stub = spawn_stub(monitor_router(state.clone())).await;
        let equipment = registry_with_monitor(connected_entry(&stub.url()).await);
        let calls = state.set_connected_calls.load(Ordering::SeqCst);
        let event_bus = EventBus::from_config(&[], None).unwrap();
        let entry = equipment.entry("sm-under-test").unwrap();

        let outcome = connect_entry(&equipment, entry, &event_bus).await;

        assert!(matches!(outcome, Supervised::Healthy), "{outcome:?}");
        assert_eq!(state.set_connected_calls.load(Ordering::SeqCst), calls);
        assert_eq!(state.disconnect_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn entry_names_a_roster_entry_by_its_id_and_the_mount_by_mount() {
        let equipment = registry_with_monitor(SafetyMonitorEntry {
            id: "sm-under-test".to_string(),
            config: monitor_config("http://127.0.0.1:1"),
            session: DeviceSession::disconnected(),
        });
        let entry = equipment.entry("sm-under-test").unwrap();
        assert_eq!(
            (entry.kind(), entry.id()),
            ("safety_monitor", "sm-under-test")
        );
        assert!(equipment.entry(config::MOUNT_ID).is_none());
        assert!(equipment.entry("unknown").is_none());
    }

    /// One Alpaca server hosting every supported device kind at index
    /// 0, each accepting `Connected = true`. The camera's invariant
    /// property endpoints are deliberately absent — those reads degrade
    /// to `None` per field, they never fail the session.
    fn all_kinds_router() -> Router {
        let kinds = [
            "Camera",
            "FilterWheel",
            "CoverCalibrator",
            "Focuser",
            "SafetyMonitor",
            "Switch",
            "Rotator",
            "ObservingConditions",
            "Dome",
            "Telescope",
        ];
        let devices: Vec<serde_json::Value> = kinds
            .iter()
            .map(|kind| {
                serde_json::json!({
                    "DeviceName": kind, "DeviceType": kind,
                    "DeviceNumber": 0, "UniqueID": format!("uid-{kind}")
                })
            })
            .collect();
        let mut router = Router::new().route(
            "/management/v1/configureddevices",
            get(move || {
                let devices = devices.clone();
                async move { alpaca_ok(serde_json::Value::Array(devices)) }
            }),
        );
        for path in [
            "camera",
            "filterwheel",
            "covercalibrator",
            "focuser",
            "safetymonitor",
            "switch",
            "rotator",
            "observingconditions",
            "dome",
            "telescope",
        ] {
            router = router.route(
                &format!("/api/v1/{path}/0/connected"),
                axum::routing::put(|| async {
                    Json(serde_json::json!({"ErrorNumber": 0, "ErrorMessage": ""}))
                }),
            );
        }
        router
    }

    /// One pass establishes a session for every device kind — proving
    /// each kind's block in the pass is wired to the right establish
    /// routine. All ten entries start with no session (the
    /// down-at-startup shape) against a server hosting them all.
    #[tokio::test]
    async fn pass_establishes_every_device_kind() {
        use crate::equipment::{
            CameraEntry, CoverCalibratorEntry, DomeEntry, FilterWheelEntry, FocuserEntry,
            MountEntry, ObservingConditionsEntry, RotatorEntry, SwitchEntry,
        };

        let stub = spawn_stub(all_kinds_router()).await;
        let url = stub.url();
        let id = |kind: &str| format!("{kind}-under-test");

        let equipment = Arc::new(EquipmentRegistry {
            cameras: vec![CameraEntry::new(
                id("camera"),
                config::CameraConfig {
                    id: id("camera"),
                    name: "test".to_string(),
                    alpaca_url: url.clone(),
                    device_type: String::new(),
                    device_number: 0,
                    unique_id: None,
                    cooler_targets_c: Vec::new(),
                    gain: None,
                    offset: None,
                    readout_time_estimate: None,
                    auth: None,
                },
                DeviceSession::disconnected(),
            )],
            filter_wheels: vec![FilterWheelEntry {
                id: id("filter-wheel"),
                config: config::FilterWheelConfig {
                    id: id("filter-wheel"),
                    alpaca_url: url.clone(),
                    device_number: 0,
                    unique_id: None,
                    filters: Vec::new(),
                    auth: None,
                },
                session: DeviceSession::disconnected(),
                slot: crate::equipment::DeviceSlot::default(),
            }],
            cover_calibrators: vec![CoverCalibratorEntry {
                id: id("cover-calibrator"),
                config: config::CoverCalibratorConfig {
                    id: id("cover-calibrator"),
                    alpaca_url: url.clone(),
                    device_number: 0,
                    unique_id: None,
                    poll_interval: Duration::from_secs(1),
                    auth: None,
                },
                session: DeviceSession::disconnected(),
            }],
            focusers: vec![FocuserEntry {
                id: id("focuser"),
                config: config::FocuserConfig {
                    microns_per_step: None,
                    id: id("focuser"),
                    alpaca_url: url.clone(),
                    device_number: 0,
                    unique_id: None,
                    min_position: None,
                    max_position: None,
                    steps_per_sec: config::focuser::FocuserStepsPerSec::default(),
                    backlash: None,
                    auth: None,
                },
                session: DeviceSession::disconnected(),
                slot: crate::equipment::DeviceSlot::default(),
            }],
            safety_monitors: vec![SafetyMonitorEntry {
                id: id("safety-monitor"),
                config: config::SafetyMonitorConfig {
                    id: id("safety-monitor"),
                    alpaca_url: url.clone(),
                    device_number: 0,
                    unique_id: None,
                    auth: None,
                },
                session: DeviceSession::disconnected(),
            }],
            switches: vec![SwitchEntry {
                id: id("switch"),
                config: config::SwitchConfig {
                    id: id("switch"),
                    name: None,
                    alpaca_url: url.clone(),
                    device_number: 0,
                    unique_id: None,
                    auth: None,
                    outputs: Vec::new(),
                },
                session: DeviceSession::disconnected(),
            }],
            rotators: vec![RotatorEntry {
                id: id("rotator"),
                config: config::RotatorConfig {
                    id: id("rotator"),
                    name: None,
                    alpaca_url: url.clone(),
                    device_number: 0,
                    unique_id: None,
                    auth: None,
                },
                session: DeviceSession::disconnected(),
                slot: crate::equipment::DeviceSlot::default(),
            }],
            observing_conditions: vec![ObservingConditionsEntry {
                id: id("observing-conditions"),
                config: config::ObservingConditionsConfig {
                    id: id("observing-conditions"),
                    name: None,
                    alpaca_url: url.clone(),
                    device_number: 0,
                    unique_id: None,
                    auth: None,
                },
                session: DeviceSession::disconnected(),
            }],
            domes: vec![DomeEntry {
                id: id("dome"),
                config: config::DomeConfig {
                    id: id("dome"),
                    name: None,
                    alpaca_url: url.clone(),
                    device_number: 0,
                    unique_id: None,
                    auth: None,
                },
                session: DeviceSession::disconnected(),
            }],
            mount: Some(MountEntry {
                config: config::MountConfig {
                    alpaca_url: url,
                    device_number: 0,
                    unique_id: None,
                    settle_after_slew: None,
                    slew_rate_arcsec_per_sec: config::mount::SlewRateArcsecPerSec::default(),
                    guiding: None,
                    auth: None,
                },
                session: DeviceSession::disconnected(),
            }),
            ..EquipmentRegistry::default()
        });

        let supervisor = supervisor_over(equipment.clone());
        let mut events = supervisor.event_bus.subscribe();
        supervisor.pass().await;

        assert!(equipment.cameras[0].is_connected(), "camera");
        assert!(equipment.filter_wheels[0].is_connected(), "filter wheel");
        assert!(
            equipment.cover_calibrators[0].is_connected(),
            "cover calibrator"
        );
        assert!(equipment.focusers[0].is_connected(), "focuser");
        assert!(
            equipment.safety_monitors[0].is_connected(),
            "safety monitor"
        );
        assert!(equipment.switches[0].is_connected(), "switch");
        assert!(equipment.rotators[0].is_connected(), "rotator");
        assert!(
            equipment.observing_conditions[0].is_connected(),
            "observing conditions"
        );
        assert!(equipment.domes[0].is_connected(), "dome");
        assert!(
            equipment
                .mount
                .as_ref()
                .is_some_and(MountEntry::is_connected),
            "mount"
        );

        let mut kinds = Vec::new();
        while let Ok(event) = events.try_recv() {
            assert_eq!(event.event, "equipment_changed");
            assert_eq!(event.payload["connected"], true);
            kinds.push(event.payload["kind"].as_str().unwrap_or("?").to_owned());
        }
        kinds.sort_unstable();
        assert_eq!(
            kinds,
            [
                "camera",
                "cover_calibrator",
                "dome",
                "filter_wheel",
                "focuser",
                "mount",
                "observing_conditions",
                "rotator",
                "safety_monitor",
                "switch",
            ],
            "every kind must emit exactly one establishment event"
        );
    }
}
