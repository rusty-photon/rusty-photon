//! Safety enforcement (rp.md § Safety).
//!
//! Polls every configured ASCOM
//! `SafetyMonitor`, and on the overall safe → unsafe transition closes
//! the safety gate (the [`SafetyStatus`] the MCP dispatch reads to
//! refuse gated tools with `SafetyUnsafe`), cancels every in-flight
//! *gated* tool call and every in-flight `capture` through the
//! in-flight registry (rp.md § Safety → In-Flight Tool Calls — a slew
//! aborts, an exposure aborts, both answer `cancelled: safety`; an
//! in-flight park completes), interrupts the active session, aborts any
//! exposure left without a body, stops guiding (emitting `guide_stopped`
//! with `reason: "safety"`), and parks the mount. On unsafe → safe,
//! opens the gate and resumes the interrupted session by re-invoking
//! the orchestrator with recovery context.
//!
//! Readings are **fail-unsafe**: a monitor that is disconnected or
//! errors counts as unsafe, and conditions are safe only while *all*
//! monitors report safe. Every per-monitor change emits a
//! `safety_changed` event; the assumed baseline is safe, so a monitor
//! that starts out safe emits nothing at startup while one that starts
//! out unsafe announces itself.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::equipment::EquipmentRegistry;
use crate::events::EventBus;
use crate::mcp::inflight::InFlight;

/// The safety state the gate and `get_safety_status` read (rp.md
/// § Safety → In-Flight Tool Calls). Written by the enforcer on every
/// poll; shared with `McpHandler` behind an `Arc`.
///
/// `overall` is the gate flag proper — an atomic, because the dispatch
/// reads it on every gated call. The per-monitor readings and the
/// `since` stamps sit behind a mutex that is never held across an
/// await.
pub struct SafetyStatus {
    overall: AtomicBool,
    inner: Mutex<StatusInner>,
}

struct StatusInner {
    /// When `overall` last changed (process start until it does).
    since: DateTime<Utc>,
    monitors: BTreeMap<String, MonitorReading>,
}

#[derive(Clone, Copy, Debug)]
struct MonitorReading {
    safe: bool,
    /// When this monitor's reading last changed (its first poll until
    /// it does).
    since: DateTime<Utc>,
}

/// A point-in-time copy of [`SafetyStatus`], for `get_safety_status`.
#[derive(Clone, Debug)]
pub struct SafetySnapshot {
    pub overall: bool,
    pub since: DateTime<Utc>,
    /// Every polled monitor, in id order.
    pub monitors: Vec<MonitorSnapshot>,
}

/// One monitor's last reading.
#[derive(Clone, Debug)]
pub struct MonitorSnapshot {
    pub id: String,
    pub safe: bool,
    pub since: DateTime<Utc>,
}

impl Default for SafetyStatus {
    /// Safe, no monitors polled yet — the assumed baseline (and what a
    /// deployment with no safety monitors reports forever).
    fn default() -> Self {
        Self {
            overall: AtomicBool::new(true),
            inner: Mutex::new(StatusInner {
                since: Utc::now(),
                monitors: BTreeMap::new(),
            }),
        }
    }
}

impl SafetyStatus {
    fn lock(&self) -> std::sync::MutexGuard<'_, StatusInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The gate flag: `false` while conditions are unsafe.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        self.overall.load(Ordering::SeqCst)
    }

    /// Record one monitor's reading (a failed read is recorded as
    /// unsafe by the caller). The `since` stamp moves only when the
    /// reading changes.
    pub fn record_monitor(&self, id: &str, safe: bool) {
        let mut inner = self.lock();
        match inner.monitors.get_mut(id) {
            Some(reading) if reading.safe == safe => {}
            Some(reading) => {
                reading.safe = safe;
                reading.since = Utc::now();
            }
            None => {
                inner.monitors.insert(
                    id.to_owned(),
                    MonitorReading {
                        safe,
                        since: Utc::now(),
                    },
                );
            }
        }
    }

    /// Set the overall state (the gate). The `since` stamp moves only
    /// on a change.
    pub fn set_overall(&self, safe: bool) {
        let mut inner = self.lock();
        if self.overall.swap(safe, Ordering::SeqCst) != safe {
            inner.since = Utc::now();
        }
    }

    /// An unsafe monitor to name on a refusal — the first in id order
    /// when several are — or `None` when no polled monitor is unsafe.
    #[must_use]
    pub fn unsafe_monitor(&self) -> Option<String> {
        self.lock()
            .monitors
            .iter()
            .find(|(_, reading)| !reading.safe)
            .map(|(id, _)| id.clone())
    }

    /// A point-in-time copy for `get_safety_status`.
    #[must_use]
    pub fn snapshot(&self) -> SafetySnapshot {
        let inner = self.lock();
        SafetySnapshot {
            overall: self.is_safe(),
            since: inner.since,
            monitors: inner
                .monitors
                .iter()
                .map(|(id, reading)| MonitorSnapshot {
                    id: id.clone(),
                    safe: reading.safe,
                    since: reading.since,
                })
                .collect(),
        }
    }
}

/// One pollable safety source. A seam over the Alpaca device so the
/// polling loop is unit-testable with scripted probes.
pub trait SafetyProbe: Send + Sync {
    fn id(&self) -> &str;
    fn is_safe(&self) -> impl Future<Output = Result<bool, String>> + Send;
}

/// Production probe over a connected (or not) ASCOM Alpaca `SafetyMonitor`.
///
/// The device handle is looked up through the registry on every poll —
/// never cloned out at construction — so a session the reconnect
/// supervisor re-establishes (rp.md § Device Session Recovery) is
/// picked up by the very next poll.
pub struct AlpacaSafetyProbe {
    id: String,
    equipment: Arc<EquipmentRegistry>,
}

impl SafetyProbe for AlpacaSafetyProbe {
    fn id(&self) -> &str {
        &self.id
    }

    async fn is_safe(&self) -> Result<bool, String> {
        let device = self
            .equipment
            .find_safety_monitor(&self.id)
            .and_then(crate::equipment::SafetyMonitorEntry::device);
        let Some(device) = device else {
            return Err("safety monitor is not connected".to_string());
        };
        device.is_safe().await.map_err(|e| e.to_string())
    }
}

/// The polling loop plus everything it drives on a transition.
pub struct SafetyEnforcer<P: SafetyProbe> {
    probes: Vec<P>,
    poll_interval: Duration,
    event_bus: Arc<EventBus>,
    /// The in-flight tool-call registry shared with `McpHandler`; the
    /// unsafe transition cancels its gated entries and in-flight captures.
    in_flight: Arc<InFlight>,
    equipment: Arc<EquipmentRegistry>,
    /// Guider-service client shared with `McpHandler`; the unsafe
    /// transition stops guiding through it. `None` when no `guider`
    /// block is configured — the step is skipped.
    guider: Option<Arc<dyn rp_guider::GuiderClient>>,
    /// The safety state shared with `McpHandler`: the gate the MCP
    /// dispatch reads to refuse gated tools while conditions are
    /// unsafe, and what `get_safety_status` reports.
    status: Arc<SafetyStatus>,
}

impl SafetyEnforcer<AlpacaSafetyProbe> {
    /// Build the enforcer over the registry's connected safety monitors.
    /// Returns `None` when none are configured — the loop never starts
    /// and every tool runs ungated.
    pub fn from_registry(
        equipment: Arc<EquipmentRegistry>,
        event_bus: Arc<EventBus>,
        in_flight: Arc<InFlight>,
        status: Arc<SafetyStatus>,
        guider: Option<Arc<dyn rp_guider::GuiderClient>>,
        poll_interval: Duration,
    ) -> Option<Self> {
        if equipment.safety_monitors.is_empty() {
            debug!("no safety monitors configured; safety polling disabled");
            return None;
        }
        let probes = equipment
            .safety_monitors
            .iter()
            .map(|entry| AlpacaSafetyProbe {
                id: entry.id.clone(),
                equipment: equipment.clone(),
            })
            .collect();
        Some(Self {
            probes,
            poll_interval,
            event_bus,
            in_flight,
            equipment,
            guider,
            status,
        })
    }
}

impl<P: SafetyProbe> SafetyEnforcer<P> {
    /// Poll until cancelled (rp shutdown).
    pub async fn run(self, cancel: CancellationToken) {
        // Assumed-safe baselines: transitions are relative to these, so
        // a monitor that starts out safe is quiet and one that starts
        // out unsafe announces itself on the first poll. Poll before
        // sleeping: a monitor that is unsafe (or unreadable) at startup
        // must gate immediately, not after the first interval elapses.
        // (The production path in `BoundServer::start` runs this first
        // poll inline instead — before startup recovery — and continues
        // via `run_from`.)
        let mut per_monitor: HashMap<String, bool> = HashMap::new();
        let overall = self.poll_once(&mut per_monitor, true).await;
        self.run_from(cancel, per_monitor, overall).await;
    }

    /// Continue polling from a known state — the per-monitor baselines
    /// and the overall reading an inline first poll produced. Sleeps
    /// before each pass (the first pass already happened).
    pub async fn run_from(
        self,
        cancel: CancellationToken,
        mut per_monitor: HashMap<String, bool>,
        mut overall: bool,
    ) {
        info!(
            monitors = self.probes.len(),
            interval = ?self.poll_interval,
            "safety monitoring started"
        );
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    debug!("safety monitoring stopped");
                    return;
                }
                () = tokio::time::sleep(self.poll_interval) => {}
            }
            overall = self.poll_once(&mut per_monitor, overall).await;
        }
    }

    /// One polling pass: read every probe, emit per-monitor
    /// `safety_changed` events, and act when the overall state flips.
    /// Returns the new overall state.
    pub(crate) async fn poll_once(
        &self,
        per_monitor: &mut HashMap<String, bool>,
        prev_overall: bool,
    ) -> bool {
        let mut overall = true;
        for probe in &self.probes {
            let reading = match probe.is_safe().await {
                Ok(reading) => reading,
                Err(e) => {
                    warn!(monitor = probe.id(), error = %e,
                          "safety monitor read failed; treating as unsafe");
                    false
                }
            };
            overall &= reading;
            self.status.record_monitor(probe.id(), reading);
            if !reading {
                // Close the gate at the first unsafe reading, not after
                // the whole pass: a later probe that is slow to answer
                // (a hung device read runs to the Alpaca client's
                // timeout) must not keep gated tools dispatching while
                // an unsafe reading is already in hand. Idempotent when
                // already closed; the rest of the transition — cancel,
                // stop, park — runs after the pass, and only `on_safe`
                // ever opens the gate, after a pass every monitor read
                // safe.
                self.status.set_overall(false);
            }
            let prev = per_monitor
                .insert(probe.id().to_owned(), reading)
                .unwrap_or(true);
            if prev != reading {
                let new_state = if reading { "safe" } else { "unsafe" };
                debug!(monitor = probe.id(), new_state, "safety monitor transition");
                self.event_bus.emit(
                    "safety_changed",
                    serde_json::json!({
                        "monitor": probe.id(),
                        "new_state": new_state,
                    }),
                );
            }
        }
        if overall != prev_overall {
            if overall {
                self.on_safe();
            } else {
                self.on_unsafe().await;
            }
        }
        overall
    }

    /// Overall safe → unsafe: the gate is already closed (the poll pass
    /// closes it at the first unsafe reading; the store here is the
    /// idempotent backstop) so nothing gated gets in — the dispatch
    /// registers before it checks the gate, so a call racing the close
    /// is either refused there or swept below. Cancel the in-flight
    /// gated tool calls and captures and wait for them to acknowledge
    /// (a cancelled slew's abort must not land on the park below), then
    /// stop the hardware — abort any exposure left without a body, stop
    /// guiding, park the mount, in that order (the mount must not move
    /// under an exposing camera or an active guide loop).
    async fn on_unsafe(&self) {
        warn!("conditions unsafe; cancelling in-flight gated work");
        self.status.set_overall(false);
        let cancelled = self.in_flight.cancel_for_safety().await;
        if cancelled > 0 {
            info!(
                cancelled,
                "in-flight tool calls cancelled on unsafe transition"
            );
        }
        abort_exposures(&self.equipment).await;
        stop_guiding(self.guider.as_ref(), &self.event_bus).await;
        park_mount(&self.equipment).await;
    }

    /// Overall unsafe → safe: open the gate. Nothing else — rp keeps no
    /// record of who was driving and re-invokes nobody; the per-monitor
    /// `safety_changed` event is the signal an orchestrator resumes on
    /// (rp.md § Safety).
    fn on_safe(&self) {
        info!("conditions safe again; gated tools answer again");
        self.status.set_overall(true);
    }
}

/// Best-effort `AbortExposure` on every connected camera. The registry
/// already cancelled every registered `capture` (each aborted its own
/// exposure and answered `cancelled: safety`); this catches a camera
/// left exposing with no body to answer for it, since the park that
/// follows would ruin the frame either way and a camera left exposing
/// would keep going into the (unsafe) night.
async fn abort_exposures(equipment: &EquipmentRegistry) {
    for camera in &equipment.cameras {
        let Some(device) = camera.device() else {
            continue;
        };
        match device.abort_exposure().await {
            Ok(()) => debug!(camera = %camera.id, "aborted in-progress exposure"),
            Err(e) => {
                // Usually just "no exposure in progress" — worth a debug
                // line, not an operator-facing warning.
                debug!(camera = %camera.id, error = %e, "abort_exposure failed");
            }
        }
    }
}

/// Upper bound on how long the unsafe transition waits for stop-guiding
/// to confirm before moving on to parking regardless. The guider
/// service's own `stop_timeout` config defaults to 10 s
/// (`phd2-guider.md` § "POST /api/v1/guiding/stop"), so this leaves
/// margin for a normal confirmed stop to land without letting a wedged
/// guider service (or an operator-configured `guider.timeout` far
/// longer than the client call would otherwise honor) delay parking —
/// the safety-critical step — for the client's full HTTP timeout.
const SAFETY_STOP_GUIDING_TIMEOUT: Duration = Duration::from_secs(15);

/// Best-effort stop-guiding through the shared guider client — the
/// guide loop must not keep dragging the mount while conditions are
/// unsafe. Emits `guide_stopped` with `reason: "safety"` on a
/// confirmed stop; a failure (service down, PHD2 gone) or a stop that
/// doesn't confirm within [`SAFETY_STOP_GUIDING_TIMEOUT`] is logged and
/// swallowed so the park below still runs promptly.
async fn stop_guiding(guider: Option<&Arc<dyn rp_guider::GuiderClient>>, event_bus: &EventBus) {
    let Some(client) = guider else {
        return;
    };
    match tokio::time::timeout(SAFETY_STOP_GUIDING_TIMEOUT, client.stop_guiding()).await {
        Ok(Ok(())) => {
            debug!("guiding stopped on unsafe transition");
            event_bus.emit("guide_stopped", serde_json::json!({ "reason": "safety" }));
        }
        Ok(Err(e)) => {
            debug!(error = %e, "stop_guiding failed during unsafe transition");
        }
        Err(_) => {
            debug!(
                timeout = ?SAFETY_STOP_GUIDING_TIMEOUT,
                "stop_guiding did not confirm in time during unsafe transition; proceeding to park"
            );
        }
    }
}

/// Best-effort park on the configured mount — fire-and-forget like
/// [`abort_exposures`]: the Alpaca `Park` is issued and logged, but
/// the enforcer does not block on `AtPark` (Sentinel's watchdog owns
/// escalation if the mount never gets there).
async fn park_mount(equipment: &EquipmentRegistry) {
    let Some(mount) = &equipment.mount else {
        return;
    };
    let Some(device) = mount.device() else {
        debug!("mount not connected; skipping park on unsafe transition");
        return;
    };
    match device.park().await {
        Ok(()) => debug!("mount park commanded on unsafe transition"),
        Err(e) => debug!(error = %e, "mount park failed during unsafe transition"),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unreachable)]
mod tests {
    use std::sync::Mutex;

    use rmcp::model::RequestId;

    use super::*;
    use crate::mcp::gate::ToolClass;

    /// Probe whose readings are scripted: pops the front of the queue,
    /// repeating the last entry once drained.
    struct ScriptedProbe {
        id: String,
        readings: Mutex<Vec<Result<bool, String>>>,
    }

    impl ScriptedProbe {
        fn new(id: &str, readings: Vec<Result<bool, String>>) -> Self {
            Self {
                id: id.to_string(),
                readings: Mutex::new(readings),
            }
        }
    }

    impl SafetyProbe for ScriptedProbe {
        fn id(&self) -> &str {
            &self.id
        }

        async fn is_safe(&self) -> Result<bool, String> {
            let mut readings = self.readings.lock().unwrap();
            if readings.len() > 1 {
                readings.remove(0)
            } else {
                readings[0].clone()
            }
        }
    }

    fn empty_registry() -> Arc<EquipmentRegistry> {
        Arc::new(EquipmentRegistry {
            cameras: vec![],
            filter_wheels: vec![],
            cover_calibrators: vec![],
            focusers: vec![],
            safety_monitors: vec![],
            mount: None,
            ..Default::default()
        })
    }

    fn enforcer_with(probes: Vec<ScriptedProbe>) -> SafetyEnforcer<ScriptedProbe> {
        enforcer_over(probes)
    }

    fn enforcer_over<P: SafetyProbe>(probes: Vec<P>) -> SafetyEnforcer<P> {
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        SafetyEnforcer {
            probes,
            poll_interval: Duration::from_millis(1),
            event_bus,
            in_flight: Arc::new(InFlight::default()),
            equipment: empty_registry(),
            guider: None,
            status: Arc::new(SafetyStatus::default()),
        }
    }

    /// [`enforcer_with`] plus a mock guider client on the enforcer.
    fn enforcer_with_guider(
        probes: Vec<ScriptedProbe>,
        configure: impl FnOnce(&mut rp_guider::MockGuiderClient),
    ) -> SafetyEnforcer<ScriptedProbe> {
        let mut mock = rp_guider::MockGuiderClient::new();
        configure(&mut mock);
        let mut enforcer = enforcer_with(probes);
        enforcer.guider = Some(Arc::new(mock));
        enforcer
    }

    #[tokio::test]
    async fn quiet_when_monitors_start_out_safe() {
        let enforcer = enforcer_with(vec![ScriptedProbe::new("sm", vec![Ok(true)])]);
        let mut events = enforcer.event_bus.subscribe();
        let mut state = HashMap::new();

        let overall = enforcer.poll_once(&mut state, true).await;
        assert!(overall);
        assert!(enforcer.status.is_safe());
        assert!(
            events.try_recv().is_err(),
            "a safe first reading matches the assumed baseline; no event expected"
        );
        // The status still records the reading, for `get_safety_status`.
        let snapshot = enforcer.status.snapshot();
        assert!(snapshot.overall);
        assert_eq!(snapshot.monitors.len(), 1);
        assert_eq!(snapshot.monitors[0].id, "sm");
        assert!(snapshot.monitors[0].safe);
        assert_eq!(enforcer.status.unsafe_monitor(), None);
    }

    #[tokio::test]
    async fn unsafe_transition_gates_and_cancels_gated_tool_calls() {
        let enforcer = enforcer_with(vec![ScriptedProbe::new("sm", vec![Ok(false)])]);
        let mut events = enforcer.event_bus.subscribe();

        // Three in-flight calls: the gated slew and the ungated capture
        // must be cancelled with the safety reason, the ungated park
        // left alone.
        let parent = CancellationToken::new();
        let (slew_guard, slew) =
            enforcer
                .in_flight
                .register(&RequestId::Number(1), "slew", ToolClass::Gated, &parent);
        let (_park_guard, park) =
            enforcer
                .in_flight
                .register(&RequestId::Number(2), "park", ToolClass::Ungated, &parent);
        let (capture_guard, capture) = enforcer.in_flight.register(
            &RequestId::Number(3),
            "capture",
            ToolClass::Ungated,
            &parent,
        );
        // The cancelled bodies acknowledge by returning (dropping
        // their guards).
        let slew_body = tokio::spawn(async move {
            slew.cancelled().await;
            let error = slew.error();
            drop(slew_guard);
            error
        });
        let capture_body = tokio::spawn(async move {
            capture.cancelled().await;
            let error = capture.error();
            drop(capture_guard);
            error
        });

        let mut state = HashMap::new();
        let overall = enforcer.poll_once(&mut state, true).await;

        assert!(!overall);
        assert!(!enforcer.status.is_safe(), "gate must close");
        assert_eq!(
            enforcer.status.unsafe_monitor().as_deref(),
            Some("sm"),
            "the refusal names the unsafe monitor"
        );
        assert_eq!(slew_body.await.unwrap(), "cancelled: safety");
        assert_eq!(capture_body.await.unwrap(), "cancelled: safety");
        assert!(!park.is_cancelled(), "the ungated park must keep running");
        assert_eq!(
            enforcer.in_flight.len(),
            1,
            "only the park is still in flight"
        );

        // The safety transition is the only event.
        let changed = events.recv().await.unwrap();
        assert_eq!(changed.event, "safety_changed");
        assert_eq!(changed.payload["monitor"], "sm");
        assert_eq!(changed.payload["new_state"], "unsafe");
    }

    /// A probe with a fixed reading that, when given a `release`,
    /// does not answer until it is notified — a device read that hangs
    /// to its timeout.
    struct HoldableProbe {
        id: String,
        reading: bool,
        release: Option<Arc<tokio::sync::Notify>>,
    }

    impl SafetyProbe for HoldableProbe {
        fn id(&self) -> &str {
            &self.id
        }

        async fn is_safe(&self) -> Result<bool, String> {
            if let Some(release) = &self.release {
                release.notified().await;
            }
            Ok(self.reading)
        }
    }

    /// The gate closes at the first unsafe reading of a pass, not after
    /// the pass: while the second probe is still hanging, gated tools
    /// are already refused.
    #[tokio::test]
    async fn the_gate_closes_at_the_first_unsafe_reading_before_a_slow_probe_answers() {
        let release = Arc::new(tokio::sync::Notify::new());
        let enforcer = enforcer_over(vec![
            HoldableProbe {
                id: "fast-unsafe".to_string(),
                reading: false,
                release: None,
            },
            HoldableProbe {
                id: "slow-safe".to_string(),
                reading: true,
                release: Some(release.clone()),
            },
        ]);
        let status = enforcer.status.clone();
        let pass = tokio::spawn(async move {
            let mut state = HashMap::new();
            enforcer.poll_once(&mut state, true).await
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while status.is_safe() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !status.is_safe(),
            "the gate must close before the slow probe answers"
        );
        assert_eq!(status.unsafe_monitor().as_deref(), Some("fast-unsafe"));
        assert!(
            !pass.is_finished(),
            "the pass must still be waiting on the slow probe"
        );

        release.notify_one();
        let overall = tokio::time::timeout(Duration::from_secs(5), pass)
            .await
            .expect("the pass must finish once the slow probe answers")
            .unwrap();
        assert!(!overall);
        assert!(!status.is_safe(), "the transition keeps the gate closed");
    }

    #[tokio::test]
    async fn read_errors_count_as_unsafe() {
        let enforcer = enforcer_with(vec![ScriptedProbe::new(
            "sm",
            vec![Err("boom".to_string())],
        )]);
        let mut events = enforcer.event_bus.subscribe();

        let mut state = HashMap::new();
        let overall = enforcer.poll_once(&mut state, true).await;

        assert!(!overall, "a failed read must be treated as unsafe");
        let changed = events.recv().await.unwrap();
        assert_eq!(changed.payload["new_state"], "unsafe");
    }

    #[tokio::test]
    async fn safe_transition_lifts_the_gate_and_emits_only_safety_changed() {
        let enforcer = enforcer_with(vec![ScriptedProbe::new("sm", vec![Ok(false), Ok(true)])]);
        let mut events = enforcer.event_bus.subscribe();

        let mut state = HashMap::new();
        let overall = enforcer.poll_once(&mut state, true).await;
        assert!(!overall);
        assert!(!enforcer.status.is_safe(), "gate must close");

        let overall = enforcer.poll_once(&mut state, overall).await;
        assert!(overall);
        assert!(enforcer.status.is_safe(), "gate must lift");
        assert_eq!(enforcer.status.unsafe_monitor(), None);

        // Two transitions, two events — rp re-invokes nobody, so
        // nothing else is emitted or done on the safe side.
        assert_eq!(events.recv().await.unwrap().payload["new_state"], "unsafe");
        assert_eq!(events.recv().await.unwrap().payload["new_state"], "safe");
        assert!(
            events.try_recv().is_err(),
            "no further event on the safe transition"
        );
    }

    #[tokio::test]
    async fn any_unsafe_monitor_makes_the_overall_state_unsafe() {
        let enforcer = enforcer_with(vec![
            ScriptedProbe::new("one", vec![Ok(true)]),
            ScriptedProbe::new("two", vec![Ok(false)]),
        ]);
        let mut events = enforcer.event_bus.subscribe();

        let mut state = HashMap::new();
        let overall = enforcer.poll_once(&mut state, true).await;

        assert!(!overall);
        // Only the flipping monitor emits.
        let changed = events.recv().await.unwrap();
        assert_eq!(changed.payload["monitor"], "two");
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn unsafe_transition_stops_guiding_and_emits_the_safety_stop_event() {
        let enforcer =
            enforcer_with_guider(vec![ScriptedProbe::new("sm", vec![Ok(false)])], |mock| {
                mock.expect_stop_guiding().times(1).returning(|| Ok(()));
            });
        let mut events = enforcer.event_bus.subscribe();

        let mut state = HashMap::new();
        enforcer.poll_once(&mut state, true).await;

        let changed = events.recv().await.unwrap();
        assert_eq!(changed.event, "safety_changed");
        let stopped = events.recv().await.unwrap();
        assert_eq!(stopped.event, "guide_stopped");
        assert_eq!(stopped.payload["reason"], "safety");
    }

    /// A guider that cannot be stopped (service down, PHD2 gone) must
    /// not derail the rest of the unsafe handling — and must not
    /// pretend guiding stopped by emitting the event.
    #[tokio::test]
    async fn stop_guiding_failure_is_swallowed_without_an_event() {
        let enforcer =
            enforcer_with_guider(vec![ScriptedProbe::new("sm", vec![Ok(false)])], |mock| {
                mock.expect_stop_guiding().times(1).returning(|| {
                    Err(rp_guider::GuiderError::ServiceUnreachable(
                        "connection refused".to_string(),
                    ))
                });
            });
        let mut events = enforcer.event_bus.subscribe();

        let mut state = HashMap::new();
        enforcer.poll_once(&mut state, true).await;

        assert!(
            !enforcer.status.is_safe(),
            "gate must close regardless of the guider outcome"
        );
        let changed = events.recv().await.unwrap();
        assert_eq!(changed.event, "safety_changed");
        assert!(
            events.try_recv().is_err(),
            "no guide_stopped event when the stop was not confirmed"
        );
    }

    /// No configured guider ⇒ the stop step is skipped silently (no
    /// event, no error) and the transition still gates.
    #[tokio::test]
    async fn unsafe_without_a_guider_skips_the_stop_step() {
        let enforcer = enforcer_with(vec![ScriptedProbe::new("sm", vec![Ok(false)])]);
        let mut events = enforcer.event_bus.subscribe();

        let mut state = HashMap::new();
        enforcer.poll_once(&mut state, true).await;

        assert!(!enforcer.status.is_safe());
        let changed = events.recv().await.unwrap();
        assert_eq!(changed.event, "safety_changed");
        assert!(events.try_recv().is_err());
    }

    /// A `GuiderClient` whose `stop_guiding` never resolves — stands in
    /// for a wedged guider service (process hung, not just PHD2) to
    /// prove `stop_guiding`'s timeout wrapper actually engages instead
    /// of blocking the unsafe transition indefinitely.
    struct HangingGuiderClient;

    #[async_trait::async_trait]
    impl rp_guider::GuiderClient for HangingGuiderClient {
        async fn start_guiding(
            &self,
            _request: rp_guider::StartGuidingRequest,
        ) -> Result<rp_guider::SettledOutcome, rp_guider::GuiderError> {
            unreachable!("not exercised by this test")
        }

        async fn stop_guiding(&self) -> Result<(), rp_guider::GuiderError> {
            std::future::pending().await
        }

        async fn pause_guiding(&self, _full: bool) -> Result<(), rp_guider::GuiderError> {
            unreachable!("not exercised by this test")
        }

        async fn resume_guiding(&self) -> Result<(), rp_guider::GuiderError> {
            unreachable!("not exercised by this test")
        }

        async fn dither(
            &self,
            _request: rp_guider::DitherRequest,
        ) -> Result<rp_guider::SettledOutcome, rp_guider::GuiderError> {
            unreachable!("not exercised by this test")
        }

        async fn guiding_stats(&self) -> Result<rp_guider::GuidingStats, rp_guider::GuiderError> {
            unreachable!("not exercised by this test")
        }

        async fn guiding_metrics(
            &self,
        ) -> Result<rp_guider::GuidingMetrics, rp_guider::GuiderError> {
            unreachable!("not exercised by this test")
        }

        async fn current_equipment(
            &self,
        ) -> Result<rp_guider::PhdEquipment, rp_guider::GuiderError> {
            unreachable!("not exercised by this test")
        }

        async fn clear_calibration(&self) -> Result<(), rp_guider::GuiderError> {
            unreachable!("not exercised by this test")
        }

        async fn reselect_star(&self) -> Result<(), rp_guider::GuiderError> {
            unreachable!("not exercised by this test")
        }
    }

    /// A guider service that never confirms the stop must not delay
    /// parking indefinitely — `SAFETY_STOP_GUIDING_TIMEOUT` bounds the
    /// wait, and no `guide_stopped` event fires since the stop was
    /// never confirmed. Paused time makes the 15 s bound resolve
    /// instantly instead of slowing down the test suite.
    #[tokio::test(start_paused = true)]
    async fn stop_guiding_gives_up_after_the_safety_timeout_when_the_service_is_wedged() {
        let mut enforcer = enforcer_with(vec![ScriptedProbe::new("sm", vec![Ok(false)])]);
        enforcer.guider = Some(Arc::new(HangingGuiderClient));
        let mut events = enforcer.event_bus.subscribe();

        let mut state = HashMap::new();
        enforcer.poll_once(&mut state, true).await;

        assert!(!enforcer.status.is_safe());
        let changed = events.recv().await.unwrap();
        assert_eq!(changed.event, "safety_changed");
        assert!(
            events.try_recv().is_err(),
            "no guide_stopped event when the stop never confirmed"
        );
    }

    #[tokio::test]
    async fn unsafe_with_nothing_in_flight_still_gates() {
        let enforcer = enforcer_with(vec![ScriptedProbe::new("sm", vec![Ok(false)])]);
        let mut state = HashMap::new();

        enforcer.poll_once(&mut state, true).await;

        assert!(!enforcer.status.is_safe());
        assert_eq!(enforcer.in_flight.len(), 0);
    }

    #[tokio::test]
    async fn run_stops_on_cancellation() {
        let enforcer = enforcer_with(vec![ScriptedProbe::new("sm", vec![Ok(true)])]);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(enforcer.run(cancel.clone()));

        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("run() did not stop on cancellation")
            .unwrap();
    }

    /// The first poll happens immediately, not after the first interval:
    /// a monitor that is unsafe at startup must gate `/mcp` right away.
    /// The interval here is far longer than the wait, so a pass proves
    /// the gate closed on the immediate poll.
    #[tokio::test]
    async fn run_polls_immediately_at_startup() {
        let mut enforcer = enforcer_with(vec![ScriptedProbe::new("sm", vec![Ok(false)])]);
        enforcer.poll_interval = Duration::from_hours(1);
        let status = enforcer.status.clone();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(enforcer.run(cancel.clone()));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while status.is_safe() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !status.is_safe(),
            "the gate never closed — the loop slept a full interval before its first poll"
        );
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("run() did not stop on cancellation")
            .unwrap();
    }

    /// An unsafe transition with nothing in flight must not wait for
    /// acknowledgements it will never get: the poll returns well inside
    /// `CANCEL_ACK_TIMEOUT`.
    #[tokio::test(start_paused = true)]
    async fn unsafe_transition_with_nothing_in_flight_does_not_wait() {
        let enforcer = enforcer_with(vec![ScriptedProbe::new("sm", vec![Ok(false)])]);
        let started = tokio::time::Instant::now();

        let mut state = HashMap::new();
        enforcer.poll_once(&mut state, true).await;

        assert!(enforcer.in_flight.is_empty());
        assert!(
            started.elapsed() < crate::mcp::inflight::CANCEL_ACK_TIMEOUT,
            "the transition waited on an empty registry"
        );
    }

    #[tokio::test]
    async fn from_registry_is_none_without_monitors() {
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let enforcer = SafetyEnforcer::from_registry(
            empty_registry(),
            event_bus,
            Arc::new(InFlight::default()),
            Arc::new(SafetyStatus::default()),
            None,
            Duration::from_secs(10),
        );
        assert!(enforcer.is_none());
    }

    /// The unsafe transition parks a connected mount: a registry with
    /// a real (stubbed-Alpaca) mount must receive `PUT park`.
    #[tokio::test]
    async fn unsafe_transition_parks_the_connected_mount() {
        use axum::routing::{get, put};
        use axum::{Json, Router};

        let park_called = Arc::new(AtomicBool::new(false));
        let park_flag = park_called.clone();
        let app = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [
                            {
                                "DeviceName": "Telescope 0",
                                "DeviceType": "Telescope",
                                "DeviceNumber": 0,
                                "UniqueID": "test-scope-uid"
                            }
                        ],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/telescope/0/connected",
                put(|| async { Json(serde_json::json!({"ErrorNumber": 0, "ErrorMessage": ""})) }),
            )
            .route(
                "/api/v1/telescope/0/park",
                put(move || {
                    let park_flag = park_flag.clone();
                    async move {
                        park_flag.store(true, Ordering::SeqCst);
                        Json(serde_json::json!({"ErrorNumber": 0, "ErrorMessage": ""}))
                    }
                }),
            );
        let stub = crate::equipment::test_support::spawn_stub(app).await;

        let equipment_cfg = crate::config::EquipmentConfig {
            mount: Some(crate::config::MountConfig {
                alpaca_url: stub.url(),
                device_number: 0,
                settle_after_slew: None,
                slew_rate_arcsec_per_sec: crate::config::mount::SlewRateArcsecPerSec::default(),
                guiding: None,
                auth: None,
            }),
            ..Default::default()
        };
        let equipment = Arc::new(EquipmentRegistry::new(&equipment_cfg, None).await);
        assert!(
            equipment
                .mount
                .as_ref()
                .is_some_and(crate::equipment::MountEntry::is_connected),
            "test setup: the stubbed mount must connect"
        );

        let mut enforcer = enforcer_with(vec![ScriptedProbe::new("sm", vec![Ok(false)])]);
        enforcer.equipment = equipment;

        let mut state = HashMap::new();
        enforcer.poll_once(&mut state, true).await;

        assert!(
            park_called.load(Ordering::SeqCst),
            "the unsafe transition must command Park on the connected mount"
        );
    }

    /// A configured-but-unreachable mount is skipped without error —
    /// the transition still gates.
    #[tokio::test]
    async fn park_skips_a_disconnected_mount() {
        let equipment_cfg = crate::config::EquipmentConfig {
            mount: Some(crate::config::MountConfig {
                // Client construction fails instantly on a bad URL, so
                // the entry is disconnected without any retry delay.
                alpaca_url: "not-a-url".to_string(),
                device_number: 0,
                settle_after_slew: None,
                slew_rate_arcsec_per_sec: crate::config::mount::SlewRateArcsecPerSec::default(),
                guiding: None,
                auth: None,
            }),
            ..Default::default()
        };
        let equipment = Arc::new(EquipmentRegistry::new(&equipment_cfg, None).await);

        let mut enforcer = enforcer_with(vec![ScriptedProbe::new("sm", vec![Ok(false)])]);
        enforcer.equipment = equipment;

        let mut state = HashMap::new();
        enforcer.poll_once(&mut state, true).await;

        assert!(!enforcer.status.is_safe());
    }

    /// Build the enforcer from a real registry so the production
    /// [`AlpacaSafetyProbe`] is exercised (not the scripted test probe):
    /// a connected monitor reads through the Alpaca `issafe` endpoint,
    /// and a monitor whose connect failed reads as an error → unsafe.
    /// The registry also carries cameras so the unsafe transition's
    /// exposure abort covers all three arms: abort acknowledged, abort
    /// rejected (no exposure in progress), and camera not connected.
    #[tokio::test]
    async fn alpaca_probe_reads_issafe_and_unsafe_aborts_exposures() {
        use axum::routing::{get, put};
        use axum::{Json, Router};

        fn camera_cfg(id: &str, url: &str, device_number: u32) -> crate::config::CameraConfig {
            crate::config::CameraConfig {
                id: id.to_string(),
                name: id.to_string(),
                alpaca_url: url.to_string(),
                device_type: String::new(),
                device_number,
                cooler_targets_c: Vec::new(),
                gain: None,
                offset: None,
                readout_time_estimate: None,
                auth: None,
            }
        }

        let app = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [
                            {
                                "DeviceName": "Safety Monitor 0",
                                "DeviceType": "SafetyMonitor",
                                "DeviceNumber": 0,
                                "UniqueID": "test-sm-uid"
                            },
                            {
                                "DeviceName": "Camera 0",
                                "DeviceType": "Camera",
                                "DeviceNumber": 0,
                                "UniqueID": "test-cam-0"
                            },
                            {
                                "DeviceName": "Camera 1",
                                "DeviceType": "Camera",
                                "DeviceNumber": 1,
                                "UniqueID": "test-cam-1"
                            }
                        ],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/safetymonitor/0/connected",
                put(|| async { Json(serde_json::json!({"ErrorNumber": 0, "ErrorMessage": ""})) }),
            )
            .route(
                "/api/v1/safetymonitor/0/issafe",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": true,
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/camera/0/connected",
                put(|| async { Json(serde_json::json!({"ErrorNumber": 0, "ErrorMessage": ""})) }),
            )
            .route(
                "/api/v1/camera/1/connected",
                put(|| async { Json(serde_json::json!({"ErrorNumber": 0, "ErrorMessage": ""})) }),
            )
            .route(
                "/api/v1/camera/0/abortexposure",
                put(|| async { Json(serde_json::json!({"ErrorNumber": 0, "ErrorMessage": ""})) }),
            )
            .route(
                "/api/v1/camera/1/abortexposure",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 1035,
                        "ErrorMessage": "no exposure in progress"
                    }))
                }),
            );
        let stub = crate::equipment::test_support::spawn_stub(app).await;

        let equipment_cfg = crate::config::EquipmentConfig {
            cameras: vec![
                camera_cfg("aborts-ok", &stub.url(), 0),
                camera_cfg("abort-rejected", &stub.url(), 1),
                // Client construction fails instantly on a bad URL, so
                // this entry is disconnected without any retry delay.
                camera_cfg("never-connected-cam", "not-a-url", 0),
            ],
            safety_monitors: vec![
                crate::config::SafetyMonitorConfig {
                    id: "reachable".to_string(),
                    alpaca_url: stub.url(),
                    device_number: 0,
                    auth: None,
                },
                crate::config::SafetyMonitorConfig {
                    id: "never-connected".to_string(),
                    alpaca_url: "not-a-url".to_string(),
                    device_number: 0,
                    auth: None,
                },
            ],
            ..Default::default()
        };
        let equipment = Arc::new(EquipmentRegistry::new(&equipment_cfg, None).await);
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let enforcer = SafetyEnforcer::from_registry(
            equipment,
            event_bus.clone(),
            Arc::new(InFlight::default()),
            Arc::new(SafetyStatus::default()),
            None,
            Duration::from_millis(1),
        )
        .expect("monitors are configured");
        let mut events = event_bus.subscribe();

        let mut state = HashMap::new();
        let overall = enforcer.poll_once(&mut state, true).await;

        // The reachable monitor reads safe; the disconnected one reads
        // as an error and counts unsafe — overall unsafe, and only the
        // disconnected monitor emits a transition. The unsafe handling
        // ran the exposure abort against all three cameras (asserted
        // implicitly: poll_once returned, no panic on any arm).
        assert!(!overall);
        assert!(!enforcer.status.is_safe());
        let changed = events.recv().await.unwrap();
        assert_eq!(changed.payload["monitor"], "never-connected");
        assert_eq!(changed.payload["new_state"], "unsafe");
    }
}
