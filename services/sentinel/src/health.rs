//! Service health supervision: periodic HTTP health probes with autonomous
//! restart, universal over the discovered services.
//!
//! The [`DiscoverySupervisor`] (an [`EventMonitor`]) re-enumerates the
//! platform service manager every discovery interval, maintains the shared
//! [`ServiceRegistry`] and the dashboard snapshots, and spawns one
//! [`ServiceHealthSupervisor`] task per service in a supervised run state
//! (`running` or `failed`) — reaping the task when the service stops, is
//! removed, or leaves supervision. There is no opt-in: every discovered
//! service in a supervised state is supervised (plan D3s).
//!
//! A supervisor probes `GET {derived health URL}` every poll interval, with
//! HTTP Basic credentials when the doctor-written `service_auth` credential
//! is configured. Alive means 200 — or 401/403, an auth challenge being
//! proof of life (the target may hold a hand-set credential) — or 503,
//! alive but degraded: the service deliberately reports an external
//! dependency unavailable, which no restart can cure, so a 503 resets the
//! outage like a healthy probe and publishes `Degraded` with the body's
//! optional opaque `message` for display (issue #595). Any other
//! status, a timeout, or a connection error is a failed probe; a `failed`
//! unit counts as a failed probe without HTTP. After the failure threshold
//! the supervisor restarts the service through the shared [`RestartManager`]
//! (inheriting the one-restart-per-service gate), then backs off — doubling
//! up to the ceiling — before any further attempt. Probing never stops and
//! the supervisor never gives up; one successful probe resets the whole
//! outage. All policy values are constants ([`SupervisionPolicy`]). See
//! `docs/services/sentinel.md` §Service Health Supervision.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::discovery::{
    discover, DiscoveredService, RunState, ServiceManager, ServiceRegistry, SupervisionPolicy,
};
use crate::io::HttpClient;
use crate::notifier::{Notification, NotificationRecord, Notifier};
use crate::restart::{RestartError, RestartManager, RestartReport};
use crate::state::{ServiceHealth, StateHandle};
use crate::watchdog::EventMonitor;

/// Probe timeout — the same bound as the corrective ladder's health rung, so
/// a wedged service can never stall the probe loop.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// What one supervision tick observed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TickObservation {
    /// The probe answered alive (200/401/403).
    Up,
    /// The probe answered 503: alive, but an external dependency is
    /// unavailable. Carries the body's optional `message` — opaque display
    /// text, never interpreted (see [`degraded_message`]).
    Degraded(Option<String>),
    /// The probe failed, or the unit is in the `failed` run state.
    Down,
    /// Nothing to conclude: no derivable probe URL, or the service is no
    /// longer in a supervised run state (the reconciler will reap us).
    Unknown,
}

/// The everything-shared bundle a supervisor needs; the discovery loop holds
/// one and hands clones to each per-service task.
#[derive(Clone, derive_more::Debug)]
pub struct SupervisionContext {
    pub policy: SupervisionPolicy,
    pub registry: ServiceRegistry,
    #[debug(skip)]
    pub http: Arc<dyn HttpClient>,
    pub restarts: Arc<RestartManager>,
    #[debug(skip)]
    pub notifiers: Vec<Arc<dyn Notifier>>,
    pub state: StateHandle,
}

/// Supervises one service: probe loop + failure counting + autonomous
/// restart with backoff. One task per supervised service, so an in-flight
/// restart of one service never delays the probes of another.
#[derive(Debug)]
pub struct ServiceHealthSupervisor {
    name: String,
    ctx: SupervisionContext,
}

impl ServiceHealthSupervisor {
    #[must_use]
    pub const fn new(name: String, ctx: SupervisionContext) -> Self {
        Self { name, ctx }
    }

    /// One probe: alive iff `GET {url}` answers 200 — or 401/403, an auth
    /// challenge being proof of life (the shared client carries
    /// `service_auth` when configured, but the target may hold a hand-set
    /// credential) — or 503, alive but degraded. The body is never
    /// interpreted; a 503's is handed to [`degraded_message`] for the
    /// display-only pass-through.
    async fn probe(&self, url: &str) -> TickObservation {
        match tokio::time::timeout(PROBE_TIMEOUT, self.ctx.http.get(url)).await {
            Ok(Ok(resp)) if matches!(resp.status, 200 | 401 | 403) => TickObservation::Up,
            Ok(Ok(resp)) if resp.status == 503 => {
                debug!("health probe {url} -> 503 (degraded)");
                TickObservation::Degraded(degraded_message(&resp.body))
            }
            Ok(Ok(resp)) => {
                debug!("health probe {url} -> {} (down)", resp.status);
                TickObservation::Down
            }
            Ok(Err(e)) => {
                debug!("health probe {url} failed: {e}");
                TickObservation::Down
            }
            Err(_) => {
                debug!("health probe {url} timed out after {PROBE_TIMEOUT:?}");
                TickObservation::Down
            }
        }
    }

    /// Observe one tick for the service's current discovery snapshot.
    async fn observe(&self, snapshot: &DiscoveredService) -> TickObservation {
        match snapshot.state {
            RunState::Failed => TickObservation::Down,
            RunState::Running => match &snapshot.probe {
                Some(spec) => self.probe(&spec.health_url).await,
                None => TickObservation::Unknown,
            },
            _ => TickObservation::Unknown,
        }
    }

    /// Publish this service's snapshot to the shared state. The supervisor's
    /// locals are authoritative for the health fields; the epoch-ms
    /// conversion of the monotonic next-restart deadline happens only here,
    /// at the state boundary. `last_probe_epoch_ms` is stamped by the caller
    /// when the probe completes — a publish after a slow restart must not
    /// refresh it.
    #[allow(clippy::too_many_arguments)]
    async fn publish(
        &self,
        snapshot: &DiscoveredService,
        health: ServiceHealth,
        health_message: Option<String>,
        consecutive_failures: u32,
        restarts_in_outage: u32,
        total_restarts: u64,
        next_restart_at: Option<Instant>,
        last_probe_epoch_ms: u64,
    ) {
        let now_ms = current_epoch_ms();
        let next_restart_epoch_ms = next_restart_at.map(|at| {
            let remaining = at.saturating_duration_since(Instant::now());
            project_epoch_ms(now_ms, remaining)
        });
        self.ctx
            .state
            .write()
            .await
            .set_service_health(crate::state::ServiceHealthStatus {
                name: self.name.clone(),
                unit: snapshot.unit.clone(),
                run_state: snapshot.state,
                health,
                health_message,
                last_probe_epoch_ms,
                consecutive_failures,
                restarts_in_outage,
                total_restarts,
                next_restart_epoch_ms,
                probe_port: snapshot.probe.as_ref().map(|p| p.port),
                poll_interval: self.ctx.policy.poll_interval,
            });
    }

    /// Dispatch through every configured notifier and record each attempt in
    /// the dashboard history (the watchdog's escalation pattern; manual REST
    /// restarts stay silent by design, autonomous ones never do).
    async fn notify(&self, message: String, priority: i8) {
        warn!("health supervision '{}': {}", self.name, message);
        let notification = Notification {
            title: "Observatory Service Health".to_string(),
            message: message.clone(),
            priority,
            sound: None,
        };
        let now_ms = current_epoch_ms();
        for notifier in &self.ctx.notifiers {
            let result = notifier.notify(&notification).await;
            if let Err(e) = &result {
                warn!(
                    "health notification via '{}' failed: {}",
                    notifier.type_name(),
                    e
                );
            }
            let record = NotificationRecord {
                monitor_name: self.name.clone(),
                notifier_type: notifier.type_name().to_string(),
                message: message.clone(),
                success: result.is_ok(),
                error: result.as_ref().err().map(std::string::ToString::to_string),
                timestamp_epoch_ms: now_ms,
            };
            self.ctx.state.write().await.add_notification(record);
        }
    }

    /// Notification for a completed autonomous restart attempt. The first
    /// attempt of an outage is priority 0; every later one escalates to
    /// priority 1 with a "still unhealthy" message (the phrase the dashboard
    /// history is asserted on — records carry no priority).
    async fn notify_restart(
        &self,
        report: &RestartReport,
        consecutive_failures: u32,
        restarts_in_outage: u32,
        next_wait: Duration,
    ) {
        let outcome = restart_outcome_text(report);
        if restarts_in_outage <= 1 {
            self.notify(
                format!(
                    "Service '{}' unhealthy ({consecutive_failures} consecutive failed \
                     probes); restarted autonomously — {outcome}",
                    self.name
                ),
                0,
            )
            .await;
        } else {
            self.notify(
                format!(
                    "Service '{}' still unhealthy after {restarts_in_outage} autonomous \
                     restarts — {outcome}; next attempt not before {}",
                    self.name,
                    humantime::format_duration(next_wait)
                ),
                1,
            )
            .await;
        }
    }

    /// The supervision loop, driven until `cancel` fires (the discovery loop
    /// cancels it when the service leaves the supervised set).
    pub async fn run(&self, cancel: CancellationToken) {
        let policy = self.ctx.policy;
        let mut consecutive_failures: u32 = 0;
        let mut restarts_in_outage: u32 = 0;
        let mut total_restarts: u64 = 0;
        let initial_backoff = policy.restart_backoff.min(policy.restart_backoff_max);
        let mut backoff = initial_backoff;
        let mut next_restart_at: Option<Instant> = None;

        debug!(
            "health supervisor '{}' starting: every {:?}, threshold {}",
            self.name, policy.poll_interval, policy.failure_threshold
        );

        loop {
            if cancel.is_cancelled() {
                return;
            }

            let snapshot = self.ctx.registry.read().await.get(&self.name).cloned();
            let Some(snapshot) = snapshot else {
                // Removed from the registry; the reconciler will cancel us on
                // its next pass. Idle until then.
                tokio::select! {
                    () = tokio::time::sleep(policy.poll_interval) => continue,
                    () = cancel.cancelled() => return,
                }
            };

            let observation = self.observe(&snapshot).await;
            let probed_at_ms = current_epoch_ms();
            match observation {
                TickObservation::Up => {
                    if restarts_in_outage > 0 || consecutive_failures > 0 {
                        info!(
                            "service '{}' is healthy again ({} autonomous restart(s) this outage)",
                            self.name, restarts_in_outage
                        );
                    }
                    consecutive_failures = 0;
                    restarts_in_outage = 0;
                    backoff = initial_backoff;
                    next_restart_at = None;
                    self.publish(
                        &snapshot,
                        ServiceHealth::Up,
                        None,
                        0,
                        0,
                        total_restarts,
                        None,
                        probed_at_ms,
                    )
                    .await;
                }
                TickObservation::Degraded(message) => {
                    // Alive but waiting on an external dependency no restart
                    // can supply: reset the outage like a recovery, publish
                    // amber, never restart or notify (degraded is a normal
                    // operating state — a parked rig's guider all day).
                    if restarts_in_outage > 0 || consecutive_failures > 0 {
                        debug!(
                            "service '{}' answers again but reports itself degraded \
                             ({} autonomous restart(s) this outage)",
                            self.name, restarts_in_outage
                        );
                    }
                    consecutive_failures = 0;
                    restarts_in_outage = 0;
                    backoff = initial_backoff;
                    next_restart_at = None;
                    self.publish(
                        &snapshot,
                        ServiceHealth::Degraded,
                        message,
                        0,
                        0,
                        total_restarts,
                        None,
                        probed_at_ms,
                    )
                    .await;
                }
                TickObservation::Unknown => {
                    // Nothing conclusive — an outage must not accumulate (or
                    // persist) on a service that cannot be probed.
                    consecutive_failures = 0;
                    restarts_in_outage = 0;
                    backoff = initial_backoff;
                    next_restart_at = None;
                    self.publish(
                        &snapshot,
                        ServiceHealth::Unknown,
                        None,
                        0,
                        0,
                        total_restarts,
                        None,
                        probed_at_ms,
                    )
                    .await;
                }
                TickObservation::Down => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    debug!(
                        "service '{}' check failed ({consecutive_failures} consecutive)",
                        self.name
                    );
                    // Publish before any restart so the dashboard shows Down
                    // (with current counters) while a slow restart is running.
                    self.publish(
                        &snapshot,
                        ServiceHealth::Down,
                        None,
                        consecutive_failures,
                        restarts_in_outage,
                        total_restarts,
                        next_restart_at,
                        probed_at_ms,
                    )
                    .await;

                    if consecutive_failures >= policy.failure_threshold
                        && next_restart_at.is_none_or(|at| Instant::now() >= at)
                    {
                        let attempt = tokio::select! {
                            result = self.ctx.restarts.restart(&self.name) => result,
                            // A cancelled restart drops its gate slot; the
                            // platform command runs to completion detached.
                            () = cancel.cancelled() => return,
                        };
                        match attempt {
                            Ok(report) => {
                                restarts_in_outage = restarts_in_outage.saturating_add(1);
                                total_restarts = total_restarts.saturating_add(1);
                                let wait = backoff;
                                // `checked_add` keeps the same `Option`
                                // shape. Overflow is unreachable (backoff
                                // is capped at restart_backoff_max); if it
                                // ever fired, `None` leaves the gate open
                                // rather than panicking the health loop.
                                next_restart_at = Instant::now().checked_add(wait);
                                backoff = backoff.saturating_mul(2).min(policy.restart_backoff_max);
                                self.notify_restart(
                                    &report,
                                    consecutive_failures,
                                    restarts_in_outage,
                                    wait,
                                )
                                .await;
                                self.publish(
                                    &snapshot,
                                    ServiceHealth::Down,
                                    None,
                                    consecutive_failures,
                                    restarts_in_outage,
                                    total_restarts,
                                    next_restart_at,
                                    probed_at_ms,
                                )
                                .await;
                            }
                            Err(RestartError::AlreadyInFlight(_)) => {
                                // Another path (REST endpoint, watchdog
                                // ladder) is restarting this service right
                                // now — its effect shows up in the next
                                // probes. Nothing to count, schedule, or
                                // notify.
                                debug!(
                                    "service '{}': restart already in flight elsewhere; \
                                     continuing to probe",
                                    self.name
                                );
                            }
                            Err(e) => {
                                // UnknownService: the service left the
                                // registry between the snapshot and the
                                // restart; the reconciler will reap us.
                                debug!("service '{}': autonomous restart rejected: {e}", self.name);
                            }
                        }
                    }
                }
            }

            tokio::select! {
                () = tokio::time::sleep(policy.poll_interval) => {}
                () = cancel.cancelled() => return,
            }
        }
    }
}

/// `status ok, recovery healthy` / `status failed (…detail…)` — the report
/// rendered for notification messages.
fn restart_outcome_text(report: &RestartReport) -> String {
    match (&report.recovery, &report.detail) {
        (Some(recovery), _) => format!("status {}, recovery {recovery}", report.status),
        (None, Some(detail)) => format!("status {} ({detail})", report.status),
        (None, None) => format!("status {}", report.status),
    }
}

/// The discovery loop: re-enumerates the platform, maintains the shared
/// registry and the dashboard snapshots, and spawns/reaps one
/// [`ServiceHealthSupervisor`] per supervised service.
#[derive(derive_more::Debug)]
pub struct DiscoverySupervisor {
    #[debug(skip)]
    manager: Arc<dyn ServiceManager>,
    config_dir: Option<PathBuf>,
    probe_domain: Option<String>,
    ctx: SupervisionContext,
}

impl DiscoverySupervisor {
    pub fn new(
        manager: Arc<dyn ServiceManager>,
        config_dir: Option<PathBuf>,
        probe_domain: Option<String>,
        ctx: SupervisionContext,
    ) -> Self {
        Self {
            manager,
            config_dir,
            probe_domain,
            ctx,
        }
    }

    /// One discovery pass: refresh the registry and the dashboard snapshots.
    /// An enumeration failure keeps the previous registry (and is retried on
    /// the next cycle) — a transient platform hiccup must not tear down
    /// supervision. The builder runs one pass synchronously before the
    /// dashboard binds, so the restart endpoint never races an empty
    /// registry at startup.
    pub(crate) async fn refresh(&self) -> Option<HashMap<String, DiscoveredService>> {
        match discover(
            &self.manager,
            self.config_dir.as_deref(),
            self.probe_domain.as_deref(),
        )
        .await
        {
            Ok(services) => {
                *self.ctx.registry.write().await = services.clone();
                self.ctx
                    .state
                    .write()
                    .await
                    .sync_discovered_services(&services, self.ctx.policy.poll_interval);
                Some(services)
            }
            Err(e) => {
                warn!("service discovery failed (keeping previous registry): {e}");
                None
            }
        }
    }
}

#[async_trait]
impl EventMonitor for DiscoverySupervisor {
    fn name(&self) -> &'static str {
        "Service Discovery"
    }

    async fn run(&self, cancel: CancellationToken) {
        let mut tasks: HashMap<String, (CancellationToken, tokio::task::JoinHandle<()>)> =
            HashMap::new();

        loop {
            if let Some(services) = self.refresh().await {
                // Reap supervisors whose service left the supervised set.
                for (name, (token, _)) in &tasks {
                    if !services.get(name).is_some_and(|s| s.state.supervised()) {
                        debug!("reaping supervisor for '{name}'");
                        token.cancel();
                    }
                }
                tasks.retain(|_, (token, _)| !token.is_cancelled());

                // Spawn supervisors for newly supervised services.
                for (name, service) in &services {
                    if service.state.supervised() && !tasks.contains_key(name) {
                        debug!("spawning supervisor for '{name}'");
                        let supervisor =
                            ServiceHealthSupervisor::new(name.clone(), self.ctx.clone());
                        let token = cancel.child_token();
                        let task_token = token.clone();
                        let handle = tokio::spawn(async move {
                            supervisor.run(task_token).await;
                        });
                        tasks.insert(name.clone(), (token, handle));
                    }
                }
            }

            tokio::select! {
                () = tokio::time::sleep(self.ctx.policy.discovery_interval) => {}
                () = cancel.cancelled() => break,
            }
        }

        // Shut down: cancel every supervisor and wait for the tasks so their
        // in-progress publishes finish before the engine tears down.
        for (token, _) in tasks.values() {
            token.cancel();
        }
        for (_, (_, handle)) in tasks {
            let _ = handle.await;
        }
    }
}

fn current_epoch_ms() -> u64 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    // Overflows u64 in the year 584556019. Saturate rather than wrap.
    u64::try_from(ms).unwrap_or(u64::MAX)
}

/// Epoch-ms projection of a deadline `remaining` away from `now_ms`,
/// saturating so absurdly large backoffs clamp instead of wrapping.
fn project_epoch_ms(now_ms: u64, remaining: Duration) -> u64 {
    now_ms.saturating_add(u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX))
}

/// Display cap for the degraded pass-through message.
const DEGRADED_MESSAGE_MAX_CHARS: usize = 200;

/// Bodies larger than this are not even parsed: a well-behaved health
/// endpoint's body is tens of bytes, and the supervision loop must not
/// spend CPU/memory JSON-parsing a misbehaving service's megabytes.
const DEGRADED_BODY_MAX_BYTES: usize = 4096;

/// The optional `message` string of a degraded (503) health body — the one
/// place a probe response body is ever read. The string is opaque display
/// text for the dashboard: truncated, passed through verbatim, never
/// interpreted, and no supervision decision depends on it. An oversized,
/// missing, non-JSON, or non-string `message` yields `None`.
fn degraded_message(body: &str) -> Option<String> {
    if body.len() > DEGRADED_BODY_MAX_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let message = value.get("message")?.as_str()?;
    Some(message.chars().take(DEGRADED_MESSAGE_MAX_CHARS).collect())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unreachable)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    use crate::discovery::{DiscoveredUnit, ProbeSpec};
    use crate::io::HttpResponse;
    use crate::state::new_state_handle;

    /// What every `GET` answers; flippable mid-test.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ProbeAnswer {
        Ok200,
        Status(u16),
        StatusWithBody(u16, &'static str),
        TransportError,
        /// Never resolves — exercises the probe timeout.
        Hang,
    }

    #[derive(Debug)]
    struct ScriptedHttp {
        answer: Mutex<ProbeAnswer>,
        probes: AtomicU32,
    }

    impl ScriptedHttp {
        fn new(answer: ProbeAnswer) -> Arc<Self> {
            Arc::new(Self {
                answer: Mutex::new(answer),
                probes: AtomicU32::new(0),
            })
        }

        fn set(&self, answer: ProbeAnswer) {
            *self.answer.lock().unwrap() = answer;
        }

        fn probes(&self) -> u32 {
            self.probes.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl HttpClient for ScriptedHttp {
        async fn get(&self, _url: &str) -> crate::Result<HttpResponse> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            let answer = *self.answer.lock().unwrap();
            match answer {
                ProbeAnswer::Ok200 => Ok(HttpResponse {
                    status: 200,
                    body: r#"{"status":"ok"}"#.to_string(),
                }),
                ProbeAnswer::Status(status) => Ok(HttpResponse {
                    status,
                    body: String::new(),
                }),
                ProbeAnswer::StatusWithBody(status, body) => Ok(HttpResponse {
                    status,
                    body: body.to_string(),
                }),
                ProbeAnswer::TransportError => {
                    Err(crate::SentinelError::Http("connection refused".to_string()))
                }
                ProbeAnswer::Hang => std::future::pending().await,
            }
        }

        async fn put_form(
            &self,
            _url: &str,
            _params: &[(&str, &str)],
        ) -> crate::Result<HttpResponse> {
            unreachable!("health probes never PUT")
        }

        async fn post_form(
            &self,
            _url: &str,
            _params: &[(&str, &str)],
        ) -> crate::Result<HttpResponse> {
            unreachable!("health probes never POST")
        }
    }

    /// Records the (virtual) instant of every restart; scriptable outcome.
    #[derive(Debug, Default)]
    struct RecordingManager {
        fails: bool,
        calls: Mutex<Vec<Instant>>,
        units: Mutex<Vec<String>>,
    }

    impl RecordingManager {
        fn call_instants(&self) -> Vec<Instant> {
            self.calls.lock().unwrap().clone()
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl ServiceManager for RecordingManager {
        async fn enumerate(&self) -> crate::Result<Vec<DiscoveredUnit>> {
            Ok(Vec::new())
        }

        async fn restart(&self, unit: &str, _budget: Duration) -> crate::Result<()> {
            self.calls.lock().unwrap().push(Instant::now());
            self.units.lock().unwrap().push(unit.to_string());
            if self.fails {
                Err(crate::SentinelError::Monitor(
                    "`restart` exited with 1".to_string(),
                ))
            } else {
                Ok(())
            }
        }

        async fn recovery_check(&self, _unit: &str) -> Option<bool> {
            None
        }
    }

    #[derive(Debug, Default)]
    struct RecordingNotifier {
        sent: Mutex<Vec<(i8, String)>>,
    }

    impl RecordingNotifier {
        fn sent(&self) -> Vec<(i8, String)> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Notifier for RecordingNotifier {
        fn type_name(&self) -> &'static str {
            "recorder"
        }

        async fn notify(&self, notification: &Notification) -> crate::Result<()> {
            self.sent
                .lock()
                .unwrap()
                .push((notification.priority, notification.message.clone()));
            Ok(())
        }
    }

    const SVC: &str = "svc";

    /// Fast timings: probe every 100ms, threshold 3, backoff 1s doubling to
    /// a 4s cap. With paused time the loop's schedule is exact: probes at
    /// t = 0, 100ms, 200ms, …; the first restart fires on the probe at
    /// t = 200ms (third consecutive failure).
    fn fast_policy() -> SupervisionPolicy {
        SupervisionPolicy {
            discovery_interval: Duration::from_millis(100),
            poll_interval: Duration::from_millis(100),
            failure_threshold: 3,
            restart_backoff: Duration::from_secs(1),
            restart_backoff_max: Duration::from_secs(4),
            restart_budget: Duration::from_secs(1),
        }
    }

    fn discovered(state: RunState, probe: bool) -> DiscoveredService {
        DiscoveredService {
            name: SVC.to_string(),
            unit: format!("rusty-photon-{SVC}"),
            state,
            probe: probe.then(|| ProbeSpec {
                health_url: format!("http://localhost:1/{SVC}/health"),
                alpaca_base: "http://localhost:1/api/v1".to_string(),
                port: 1,
            }),
        }
    }

    struct Fixture {
        http: Arc<ScriptedHttp>,
        manager: Arc<RecordingManager>,
        notifier: Arc<RecordingNotifier>,
        restarts: Arc<RestartManager>,
        registry: ServiceRegistry,
        state: StateHandle,
        cancel: CancellationToken,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        /// Spawn a supervisor over a scripted probe answer and a registry
        /// seeded with one service in `run_state`.
        fn spawn(answer: ProbeAnswer, service: DiscoveredService, fails: bool) -> Self {
            let http = ScriptedHttp::new(answer);
            let manager = Arc::new(RecordingManager {
                fails,
                ..RecordingManager::default()
            });
            let registry: ServiceRegistry = Arc::new(tokio::sync::RwLock::new(HashMap::from([(
                SVC.to_string(),
                service,
            )])));
            let restarts = Arc::new(RestartManager::new(
                Arc::clone(&registry),
                Arc::<RecordingManager>::clone(&manager),
                fast_policy().restart_budget,
            ));
            let notifier = Arc::new(RecordingNotifier::default());
            let state = new_state_handle(vec![], 100);
            let ctx = SupervisionContext {
                policy: fast_policy(),
                registry: Arc::clone(&registry),
                http: Arc::<ScriptedHttp>::clone(&http),
                restarts: Arc::clone(&restarts),
                notifiers: vec![Arc::<RecordingNotifier>::clone(&notifier)],
                state: Arc::clone(&state),
            };
            let supervisor = ServiceHealthSupervisor::new(SVC.to_string(), ctx);
            let cancel = CancellationToken::new();
            let handle = tokio::spawn({
                let cancel = cancel.clone();
                async move { supervisor.run(cancel).await }
            });
            Self {
                http,
                manager,
                notifier,
                restarts,
                registry,
                state,
                cancel,
                handle,
            }
        }

        async fn stop(self) {
            self.cancel.cancel();
            self.handle.await.unwrap();
        }

        async fn service_status(&self) -> crate::state::ServiceHealthStatus {
            self.state.read().await.services[0].clone()
        }
    }

    /// Advance virtual time; with `start_paused` the clock jumps timer to
    /// timer, so the supervisor's schedule stays exact.
    async fn run_until(offset: Duration) {
        tokio::time::sleep(offset).await;
    }

    #[tokio::test(start_paused = true)]
    async fn healthy_probe_publishes_up() {
        let f = Fixture::spawn(
            ProbeAnswer::Ok200,
            discovered(RunState::Running, true),
            false,
        );
        run_until(Duration::from_millis(50)).await;
        let status = f.service_status().await;
        assert_eq!(status.health, ServiceHealth::Up);
        assert_eq!(status.run_state, RunState::Running);
        assert_eq!(status.unit, "rusty-photon-svc");
        assert_eq!(status.consecutive_failures, 0);
        assert!(status.last_probe_epoch_ms > 0);
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn auth_challenge_counts_as_alive() {
        let f = Fixture::spawn(
            ProbeAnswer::Status(401),
            discovered(RunState::Running, true),
            false,
        );
        run_until(Duration::from_millis(350)).await;
        assert_eq!(f.service_status().await.health, ServiceHealth::Up);
        assert_eq!(
            f.manager.call_count(),
            0,
            "401 must never trigger a restart"
        );
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn degraded_503_counts_as_alive_and_never_restarts() {
        let f = Fixture::spawn(
            ProbeAnswer::Status(503),
            discovered(RunState::Running, true),
            false,
        );
        run_until(Duration::from_millis(350)).await;
        let status = f.service_status().await;
        assert_eq!(status.health, ServiceHealth::Degraded);
        assert_eq!(status.consecutive_failures, 0);
        assert_eq!(status.health_message, None, "empty body carries no message");
        assert_eq!(
            f.manager.call_count(),
            0,
            "503 must never trigger a restart"
        );
        assert!(f.notifier.sent().is_empty(), "degraded must never notify");
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn degraded_message_passes_through_verbatim() {
        let f = Fixture::spawn(
            ProbeAnswer::StatusWithBody(
                503,
                r#"{"status":"unavailable","message":"no connection to PHD2 on localhost:4400"}"#,
            ),
            discovered(RunState::Running, true),
            false,
        );
        run_until(Duration::from_millis(50)).await;
        let status = f.service_status().await;
        assert_eq!(status.health, ServiceHealth::Degraded);
        assert_eq!(
            status.health_message.as_deref(),
            Some("no connection to PHD2 on localhost:4400")
        );
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn degraded_resets_an_outage_like_a_recovery() {
        let f = Fixture::spawn(
            ProbeAnswer::Status(500),
            discovered(RunState::Running, true),
            false,
        );
        // One restart (t=0.2s), then the service starts answering 503.
        run_until(Duration::from_millis(250)).await;
        assert_eq!(f.manager.call_count(), 1);
        f.http.set(ProbeAnswer::Status(503));
        run_until(Duration::from_millis(100)).await; // degraded probe at t=300ms
        let status = f.service_status().await;
        assert_eq!(status.health, ServiceHealth::Degraded);
        assert_eq!(status.restarts_in_outage, 0, "outage counter resets");
        assert_eq!(status.total_restarts, 1, "lifetime counter does not");
        assert_eq!(status.next_restart_epoch_ms, None);
        run_until(Duration::from_millis(600)).await;
        assert_eq!(
            f.manager.call_count(),
            1,
            "a degraded service must not keep restarting"
        );
        f.stop().await;
    }

    #[test]
    fn degraded_message_extraction_is_strictly_bounded() {
        assert_eq!(
            degraded_message(r#"{"status":"unavailable","message":"PHD2 is off"}"#).as_deref(),
            Some("PHD2 is off")
        );
        assert_eq!(degraded_message(""), None, "empty body");
        assert_eq!(degraded_message("not json"), None, "non-JSON body");
        assert_eq!(
            degraded_message(r#"{"status":"unavailable"}"#),
            None,
            "no message field"
        );
        assert_eq!(
            degraded_message(r#"{"message":42}"#),
            None,
            "non-string message"
        );
        let long = format!(r#"{{"message":"{}"}}"#, "x".repeat(500));
        assert_eq!(
            degraded_message(&long).map(|m| m.chars().count()),
            Some(DEGRADED_MESSAGE_MAX_CHARS),
            "message is truncated for display"
        );
        let oversized = format!(r#"{{"message":"{}"}}"#, "x".repeat(DEGRADED_BODY_MAX_BYTES));
        assert_eq!(
            degraded_message(&oversized),
            None,
            "an oversized body is not even parsed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn non_200_probe_publishes_down() {
        let f = Fixture::spawn(
            ProbeAnswer::Status(500),
            discovered(RunState::Running, true),
            false,
        );
        run_until(Duration::from_millis(50)).await;
        let status = f.service_status().await;
        assert_eq!(status.health, ServiceHealth::Down);
        assert_eq!(status.consecutive_failures, 1);
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn transport_error_probe_publishes_down() {
        let f = Fixture::spawn(
            ProbeAnswer::TransportError,
            discovered(RunState::Running, true),
            false,
        );
        run_until(Duration::from_millis(50)).await;
        assert_eq!(f.service_status().await.health, ServiceHealth::Down);
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_probe_times_out_as_down() {
        let f = Fixture::spawn(
            ProbeAnswer::Hang,
            discovered(RunState::Running, true),
            false,
        );
        // The probe blocks until the 2s PROBE_TIMEOUT fires.
        run_until(Duration::from_millis(2050)).await;
        let status = f.service_status().await;
        assert_eq!(status.health, ServiceHealth::Down);
        assert_eq!(f.http.probes(), 1, "one probe, timed out");
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn missing_probe_url_is_unknown_and_never_restarts() {
        let f = Fixture::spawn(
            ProbeAnswer::Ok200,
            discovered(RunState::Running, false),
            false,
        );
        run_until(Duration::from_millis(650)).await;
        let status = f.service_status().await;
        assert_eq!(status.health, ServiceHealth::Unknown);
        assert_eq!(f.http.probes(), 0, "no URL, no probe");
        assert_eq!(f.manager.call_count(), 0);
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn failed_unit_is_restarted_without_http() {
        let f = Fixture::spawn(
            ProbeAnswer::Ok200,
            discovered(RunState::Failed, true),
            false,
        );
        // Third consecutive down tick at t=200ms triggers the restart.
        run_until(Duration::from_millis(250)).await;
        assert_eq!(f.http.probes(), 0, "a failed unit has no HTTP to probe");
        assert_eq!(f.manager.call_count(), 1);
        assert_eq!(
            *f.manager.units.lock().unwrap(),
            vec!["rusty-photon-svc".to_string()]
        );
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn below_threshold_never_restarts() {
        let f = Fixture::spawn(
            ProbeAnswer::Status(500),
            discovered(RunState::Running, true),
            false,
        );
        // Probes at t=0 and t=100ms — two failures, threshold is three.
        run_until(Duration::from_millis(150)).await;
        assert_eq!(f.manager.call_count(), 0);
        assert_eq!(f.service_status().await.consecutive_failures, 2);
        assert!(f.notifier.sent().is_empty());
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn threshold_triggers_restart_and_notifies() {
        let f = Fixture::spawn(
            ProbeAnswer::Status(500),
            discovered(RunState::Running, true),
            false,
        );
        // Third failure at t=200ms triggers the restart.
        run_until(Duration::from_millis(250)).await;
        assert_eq!(f.manager.call_count(), 1);

        let sent = f.notifier.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, 0, "first restart of an outage is priority 0");
        assert!(
            sent[0].1.contains("restarted autonomously"),
            "{}",
            sent[0].1
        );
        assert!(
            sent[0].1.contains("status ok, recovery skipped"),
            "{}",
            sent[0].1
        );

        let status = f.service_status().await;
        assert_eq!(status.restarts_in_outage, 1);
        assert_eq!(status.total_restarts, 1);
        assert!(
            status.next_restart_epoch_ms.is_some(),
            "a next attempt must be scheduled"
        );

        let state = f.state.read().await;
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].monitor_name, SVC);
        assert!(state.history[0].success);
        drop(state);
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn restart_attempts_double_backoff_and_cap() {
        let f = Fixture::spawn(
            ProbeAnswer::Status(500),
            discovered(RunState::Running, true),
            false,
        );
        // Restarts at t=0.2s, then after 1s, 2s, 4s, 4s (cap): 3.2s → 7.2s.
        run_until(Duration::from_millis(7300)).await;
        let calls = f.manager.call_instants();
        assert_eq!(calls.len(), 4, "restarts at 0.2s, 1.2s, 3.2s, 7.2s");
        let spacings: Vec<Duration> = calls.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(
            spacings,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ],
            "backoff doubles from 1s and caps at 4s"
        );
        // Probing continued at the poll cadence throughout: one probe per
        // 100ms tick from t=0 through t=7.2s inclusive, +1 by 7.3s.
        assert!(
            f.http.probes() >= 73,
            "probing must not pause during backoff (saw {})",
            f.http.probes()
        );
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn second_restart_escalates_priority() {
        let f = Fixture::spawn(
            ProbeAnswer::Status(500),
            discovered(RunState::Running, true),
            false,
        );
        // First restart at t=0.2s, second at t=1.2s.
        run_until(Duration::from_millis(1250)).await;
        let sent = f.notifier.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1].0, 1, "later restarts escalate to priority 1");
        assert!(
            sent[1]
                .1
                .contains("still unhealthy after 2 autonomous restarts"),
            "{}",
            sent[1].1
        );
        assert!(
            sent[1].1.contains("next attempt not before 2s"),
            "{}",
            sent[1].1
        );
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn failed_restart_command_still_counts_and_notifies() {
        let f = Fixture::spawn(
            ProbeAnswer::Status(500),
            discovered(RunState::Running, true),
            true,
        );
        run_until(Duration::from_millis(250)).await;
        let sent = f.notifier.sent();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].1.contains("status failed"), "{}", sent[0].1);
        assert!(sent[0].1.contains("exited with 1"), "{}", sent[0].1);
        let status = f.service_status().await;
        assert_eq!(
            status.restarts_in_outage, 1,
            "a failed attempt still counts"
        );
        assert!(status.next_restart_epoch_ms.is_some());
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_resets_counters_and_backoff_without_notifying() {
        let f = Fixture::spawn(
            ProbeAnswer::Status(500),
            discovered(RunState::Running, true),
            false,
        );
        // One restart (t=0.2s), then the service comes back.
        run_until(Duration::from_millis(250)).await;
        f.http.set(ProbeAnswer::Ok200);
        run_until(Duration::from_millis(100)).await; // healthy probe at t=300ms
        let status = f.service_status().await;
        assert_eq!(status.health, ServiceHealth::Up);
        assert_eq!(status.restarts_in_outage, 0, "outage counter resets");
        assert_eq!(status.total_restarts, 1, "lifetime counter does not");
        assert_eq!(status.next_restart_epoch_ms, None);
        assert_eq!(
            f.notifier.sent().len(),
            1,
            "recovery itself must not notify"
        );

        // A fresh outage starts from scratch: threshold anew, initial backoff,
        // and its first restart is priority 0 again.
        f.http.set(ProbeAnswer::Status(500));
        run_until(Duration::from_millis(350)).await; // fails at 0.4/0.5/0.6s → restart
        assert_eq!(f.manager.call_count(), 2);
        let sent = f.notifier.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1].0, 0, "new outage restarts at priority 0");
        assert!(
            sent[1].1.contains("restarted autonomously"),
            "{}",
            sent[1].1
        );
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn already_in_flight_is_silent() {
        let f = Fixture::spawn(
            ProbeAnswer::Status(500),
            discovered(RunState::Running, true),
            false,
        );
        // Another restart path holds the service's slot across the threshold.
        let slot = f.restarts.gate().try_acquire(SVC).unwrap();
        run_until(Duration::from_millis(250)).await;
        assert_eq!(f.manager.call_count(), 0);
        assert!(f.notifier.sent().is_empty(), "AlreadyInFlight is silent");
        assert_eq!(f.service_status().await.restarts_in_outage, 0);

        // Slot released: the next failing probe restarts normally (nothing
        // was scheduled, so no backoff wait applies).
        drop(slot);
        run_until(Duration::from_millis(100)).await;
        assert_eq!(f.manager.call_count(), 1);
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_mid_outage_stands_down() {
        let f = Fixture::spawn(
            ProbeAnswer::Status(500),
            discovered(RunState::Running, true),
            false,
        );
        run_until(Duration::from_millis(150)).await; // two failures banked
        f.registry.write().await.get_mut(SVC).unwrap().state = RunState::Stopped;
        run_until(Duration::from_millis(400)).await;
        assert_eq!(
            f.manager.call_count(),
            0,
            "an operator-stopped service must not be restarted"
        );
        let status = f.service_status().await;
        assert_eq!(status.health, ServiceHealth::Unknown);
        assert_eq!(status.consecutive_failures, 0, "the outage is abandoned");
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_stops_the_loop() {
        let f = Fixture::spawn(
            ProbeAnswer::Ok200,
            discovered(RunState::Running, true),
            false,
        );
        run_until(Duration::from_millis(50)).await;
        f.stop().await; // stop() awaits the join handle — hangs if run() leaks
    }

    // ---- the discovery loop ---------------------------------------------

    /// A manager whose enumeration is a mutable script of passes.
    #[derive(Debug)]
    struct ScriptedDiscovery {
        units: Mutex<Vec<DiscoveredUnit>>,
        fail: std::sync::atomic::AtomicBool,
    }

    impl ScriptedDiscovery {
        fn new(units: Vec<(&str, RunState)>) -> Arc<Self> {
            Arc::new(Self {
                units: Mutex::new(
                    units
                        .into_iter()
                        .map(|(u, s)| DiscoveredUnit {
                            unit: u.to_string(),
                            state: s,
                        })
                        .collect(),
                ),
                fail: std::sync::atomic::AtomicBool::new(false),
            })
        }

        fn set(&self, units: Vec<(&str, RunState)>) {
            *self.units.lock().unwrap() = units
                .into_iter()
                .map(|(u, s)| DiscoveredUnit {
                    unit: u.to_string(),
                    state: s,
                })
                .collect();
        }
    }

    #[async_trait]
    impl ServiceManager for ScriptedDiscovery {
        async fn enumerate(&self) -> crate::Result<Vec<DiscoveredUnit>> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(crate::SentinelError::Monitor("enumeration boom".into()));
            }
            Ok(self.units.lock().unwrap().clone())
        }

        async fn restart(&self, _unit: &str, _budget: Duration) -> crate::Result<()> {
            Ok(())
        }

        async fn recovery_check(&self, _unit: &str) -> Option<bool> {
            None
        }
    }

    struct LoopFixture {
        registry: ServiceRegistry,
        state: StateHandle,
        cancel: CancellationToken,
        handle: tokio::task::JoinHandle<()>,
    }

    impl LoopFixture {
        fn spawn(manager: Arc<ScriptedDiscovery>) -> Self {
            // Coerced once up front: `Arc::clone` takes its type parameter
            // from the expected type, so it cannot do the unsizing itself.
            let manager: Arc<dyn ServiceManager> = manager;
            let registry: ServiceRegistry = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
            let restarts = Arc::new(RestartManager::new(
                Arc::clone(&registry),
                Arc::clone(&manager),
                fast_policy().restart_budget,
            ));
            let state = new_state_handle(vec![], 100);
            let ctx = SupervisionContext {
                policy: fast_policy(),
                registry: Arc::clone(&registry),
                http: ScriptedHttp::new(ProbeAnswer::Ok200),
                restarts,
                notifiers: vec![],
                state: Arc::clone(&state),
            };
            let supervisor = DiscoverySupervisor::new(Arc::clone(&manager), None, None, ctx);
            let cancel = CancellationToken::new();
            let handle = tokio::spawn({
                let cancel = cancel.clone();
                async move { supervisor.run(cancel).await }
            });
            Self {
                registry,
                state,
                cancel,
                handle,
            }
        }

        async fn stop(self) {
            self.cancel.cancel();
            self.handle.await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_populates_registry_and_state() {
        let manager = ScriptedDiscovery::new(vec![
            ("rusty-photon-dsd-fp2", RunState::Running),
            ("rusty-photon-plate-solver", RunState::Inert),
            ("rusty-photon-sentinel", RunState::Running),
        ]);
        let f = LoopFixture::spawn(manager);
        run_until(Duration::from_millis(50)).await;
        let registry = f.registry.read().await;
        assert_eq!(registry.len(), 2, "sentinel's own unit is excluded");
        assert!(registry.contains_key("dsd-fp2"));
        drop(registry);
        let state = f.state.read().await;
        assert_eq!(state.services.len(), 2);
        assert_eq!(state.services[0].name, "dsd-fp2");
        assert_eq!(state.services[1].run_state, RunState::Inert);
        drop(state);
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_picks_up_and_drops_services_across_cycles() {
        let manager = ScriptedDiscovery::new(vec![("rusty-photon-a", RunState::Running)]);
        let f = LoopFixture::spawn(Arc::clone(&manager));
        run_until(Duration::from_millis(50)).await;
        assert_eq!(f.registry.read().await.len(), 1);

        manager.set(vec![("rusty-photon-b", RunState::Running)]);
        run_until(Duration::from_millis(150)).await; // next cycle at t=100ms
        let registry = f.registry.read().await;
        assert!(!registry.contains_key("a"), "removed service is dropped");
        assert!(registry.contains_key("b"), "new service is picked up");
        drop(registry);
        assert_eq!(f.state.read().await.services[0].name, "b");
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_failure_keeps_the_previous_registry() {
        let manager = ScriptedDiscovery::new(vec![("rusty-photon-a", RunState::Running)]);
        let f = LoopFixture::spawn(Arc::clone(&manager));
        run_until(Duration::from_millis(50)).await;
        assert_eq!(f.registry.read().await.len(), 1);

        manager.fail.store(true, Ordering::SeqCst);
        run_until(Duration::from_millis(200)).await;
        assert_eq!(
            f.registry.read().await.len(),
            1,
            "a transient enumeration failure must not clear the registry"
        );
        f.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_shutdown_reaps_supervisors() {
        let manager = ScriptedDiscovery::new(vec![("rusty-photon-a", RunState::Running)]);
        let f = LoopFixture::spawn(manager);
        run_until(Duration::from_millis(50)).await;
        // stop() awaits the loop's join handle, which awaits the reaped
        // supervisor tasks — a leak hangs the test.
        f.stop().await;
    }

    #[test]
    fn restart_outcome_text_labels_every_recovery_lowercase() {
        let report = |recovery| RestartReport {
            service: "svc".to_string(),
            status: "ok",
            recovery: Some(recovery),
            detail: None,
        };
        assert_eq!(
            restart_outcome_text(&report(crate::restart::Recovery::Healthy)),
            "status ok, recovery healthy"
        );
        assert_eq!(
            restart_outcome_text(&report(crate::restart::Recovery::Timeout)),
            "status ok, recovery timeout"
        );
        assert_eq!(
            restart_outcome_text(&report(crate::restart::Recovery::Skipped)),
            "status ok, recovery skipped"
        );
    }

    #[test]
    fn project_epoch_ms_adds_remaining() {
        assert_eq!(project_epoch_ms(1_000, Duration::from_millis(500)), 1_500);
    }

    #[test]
    fn project_epoch_ms_saturates_instead_of_wrapping() {
        // A remaining duration whose millisecond count exceeds u64::MAX.
        let huge = Duration::new(u64::MAX, 0);
        assert_eq!(project_epoch_ms(1_000, huge), u64::MAX);
        assert_eq!(
            project_epoch_ms(u64::MAX, Duration::from_millis(1)),
            u64::MAX
        );
    }
}
