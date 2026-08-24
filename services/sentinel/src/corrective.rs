//! Corrective-action ladder for the operation watchdog.
//!
//! When an `abort_then_restart` operation misses its deadline the watchdog
//! escalates through an *escalating* ladder against the service that owns the
//! family: health-check → abort → restart → (always) notify. Each rung is a
//! small trait with an HTTP or shell default impl, composed behind the single
//! [`Corrective`] seam ([`CorrectiveLadder`]) the watchdog calls. The least
//! invasive action that can clear the stall wins: a responsive service is
//! aborted (and the ladder stops); an unresponsive one — or a failed abort, or
//! a family with no abort verb — falls through to restart.
//!
//! See `docs/services/sentinel.md` §Operation Watchdog → Escalation.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::discovery::{DiscoveredService, ServiceManager};
use crate::io::HttpClient;
use crate::restart::RestartGate;

/// Health-check probe timeout (the design's 2 s cap).
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

/// How many times the ladder re-checks health after a restart before giving up
/// on confirming recovery. The total wait is bounded by `max_restart_duration`.
const RECOVERY_ATTEMPTS: u32 = 5;

/// The ASCOM device type and abort verb for an operation family.
///
/// `None` for families with no single Alpaca device to abort — compound
/// operations like `centering`, or non-device operations like
/// `plate_solve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlpacaBinding {
    pub device_type: &'static str,
    pub abort_verb: &'static str,
}

/// Map an operation family to the Alpaca device it drives. Park aborts the same
/// slew-to-park motion as a slew, so it shares `telescope`/`abortslew`.
#[must_use]
pub fn alpaca_binding(family: &str) -> Option<AlpacaBinding> {
    match family {
        "slew" | "park" => Some(AlpacaBinding {
            device_type: "telescope",
            abort_verb: "abortslew",
        }),
        "exposure" => Some(AlpacaBinding {
            device_type: "camera",
            abort_verb: "abortexposure",
        }),
        "move_focuser" => Some(AlpacaBinding {
            device_type: "focuser",
            abort_verb: "halt",
        }),
        _ => None,
    }
}

/// A fully-resolved corrective target: which Alpaca device to probe / abort
/// and which unit restarts the owning service. Built from an operation family
/// plus its owning [`DiscoveredService`].
#[derive(Debug, Clone)]
pub struct CorrectiveTarget {
    pub service_name: String,
    /// Unit name the restart rung hands the service manager.
    pub unit: String,
    /// Derived Alpaca API base (`{scheme}://{host}:{port}/api/v1`). `None`
    /// when the service's `<svc>.json` was unreadable: no probe, no abort —
    /// the ladder falls through to restart.
    pub base_url: Option<String>,
    pub binding: Option<AlpacaBinding>,
}

impl CorrectiveTarget {
    /// Resolve a target from the family and the discovered service that owns
    /// it. The device number is always `0`: one service per device (ADR-014).
    #[must_use]
    pub fn new(family: &str, service: &DiscoveredService) -> Self {
        Self {
            service_name: service.name.clone(),
            unit: service.unit.clone(),
            base_url: service.probe.as_ref().map(|p| p.alpaca_base.clone()),
            binding: alpaca_binding(family),
        }
    }

    /// `{base}/{device-type}/0/connected`, or `None` when the family has no
    /// Alpaca device to probe or the service no derivable base URL.
    fn connected_url(&self) -> Option<String> {
        let base = self.base_url.as_deref()?;
        let b = self.binding?;
        Some(format!("{}/{}/0/connected", base, b.device_type))
    }

    /// `{base}/{device-type}/0/{verb}`, or `None` when the family has no
    /// abort verb or the service no derivable base URL.
    fn abort_url(&self) -> Option<String> {
        let base = self.base_url.as_deref()?;
        let b = self.binding?;
        Some(format!("{}/{}/0/{}", base, b.device_type, b.abort_verb))
    }
}

/// Whether a service answered its health probe.
///
/// `Display` yields the lowercase label that appears in the ladder's
/// `health=<label>` rung, which reaches operators verbatim in escalation
/// notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, derive_more::Display)]
pub enum Healthiness {
    /// A clean `200` — the service is alive (the *operation* is stuck).
    #[display("responsive")]
    Responsive,
    /// Non-200, timeout, or transport error — the service itself is down.
    #[display("unresponsive")]
    Unresponsive,
    /// No probe was possible (the family has no Alpaca device).
    #[display("unknown")]
    Unknown,
}

// ---- rung traits -------------------------------------------------------

/// Rung 1 — probe whether a service is alive.
#[async_trait]
pub trait HealthChecker: Send + Sync + fmt::Debug {
    async fn check(&self, target: &CorrectiveTarget) -> Healthiness;
}

/// Rung 2 — abort the in-flight operation on a responsive service.
#[async_trait]
pub trait Aborter: Send + Sync + fmt::Debug {
    /// `Ok` on a clean abort; `Err` otherwise (no verb, non-200, transport).
    async fn abort(&self, target: &CorrectiveTarget) -> crate::Result<()>;
}

/// Rung 3 — restart an unresponsive (or un-abortable) service.
#[async_trait]
pub trait Restarter: Send + Sync + fmt::Debug {
    /// Restart `unit`, bounded by `budget`. `Ok` iff the platform command
    /// exits 0 in time.
    async fn restart(&self, unit: &str, budget: Duration) -> crate::Result<()>;
}

// ---- default impls -----------------------------------------------------

/// HTTP health checker: `GET {connected_url}` with a 2 s timeout. A clean 200
/// is `Responsive`; anything else (non-200, timeout, refused) is
/// `Unresponsive`.
#[derive(derive_more::Debug)]
pub struct HttpHealthChecker {
    #[debug(skip)]
    http: Arc<dyn HttpClient>,
}

impl HttpHealthChecker {
    pub fn new(http: Arc<dyn HttpClient>) -> Self {
        Self { http }
    }
}

#[async_trait]
impl HealthChecker for HttpHealthChecker {
    async fn check(&self, target: &CorrectiveTarget) -> Healthiness {
        let Some(url) = target.connected_url() else {
            return Healthiness::Unknown;
        };
        match tokio::time::timeout(HEALTH_TIMEOUT, self.http.get(&url)).await {
            Ok(Ok(resp)) if resp.status == 200 => Healthiness::Responsive,
            Ok(Ok(resp)) => {
                debug!("health check {url} -> {} (unresponsive)", resp.status);
                Healthiness::Unresponsive
            }
            Ok(Err(e)) => {
                debug!("health check {url} failed: {e}");
                Healthiness::Unresponsive
            }
            Err(_) => {
                debug!("health check {url} timed out after {HEALTH_TIMEOUT:?}");
                Healthiness::Unresponsive
            }
        }
    }
}

/// HTTP aborter: `PUT {abort_url}` with the ASCOM `ClientID` form param.
#[derive(derive_more::Debug)]
pub struct HttpAborter {
    #[debug(skip)]
    http: Arc<dyn HttpClient>,
}

impl HttpAborter {
    pub fn new(http: Arc<dyn HttpClient>) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Aborter for HttpAborter {
    async fn abort(&self, target: &CorrectiveTarget) -> crate::Result<()> {
        let Some(url) = target.abort_url() else {
            return Err(crate::SentinelError::Monitor(format!(
                "service '{}' has no abort verb for its family",
                target.service_name
            )));
        };
        debug!("abort PUT {url}");
        let resp = self.http.put_form(&url, &[("ClientID", "1")]).await?;
        if resp.status != 200 {
            return Err(crate::SentinelError::Http(format!(
                "abort {url} -> {}",
                resp.status
            )));
        }
        // Alpaca reports a rejected method via a non-zero `ErrorNumber` on an
        // HTTP 200, so a 200 alone does not mean the abort took. A body that
        // doesn't carry the envelope is treated leniently as success.
        if let Ok(parsed) = serde_json::from_str::<AlpacaResponse>(&resp.body) {
            if parsed.error_number != 0 {
                return Err(crate::SentinelError::Http(format!(
                    "abort {url} rejected: ASCOM error {} ({})",
                    parsed.error_number, parsed.error_message
                )));
            }
        }
        Ok(())
    }
}

/// The error fields every Alpaca method response carries (`Value`, when
/// present, is ignored — the abort verbs return no value).
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AlpacaResponse {
    error_number: i32,
    error_message: String,
}

/// The production restart rung: hands the unit to the platform
/// [`ServiceManager`], which derives and runs the actual command.
#[derive(derive_more::Debug)]
pub struct ManagerRestarter {
    #[debug(skip)]
    manager: Arc<dyn ServiceManager>,
}

impl ManagerRestarter {
    pub fn new(manager: Arc<dyn ServiceManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Restarter for ManagerRestarter {
    async fn restart(&self, unit: &str, budget: Duration) -> crate::Result<()> {
        self.manager.restart(unit, budget).await
    }
}

/// Build the platform shell invocation for `command`.
#[cfg(unix)]
pub(crate) fn shell_command(command: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c").arg(command);
    c
}

#[cfg(windows)]
pub(crate) fn shell_command(command: &str) -> tokio::process::Command {
    use std::os::windows::process::CommandExt;
    // `cmd` does not understand backslash-escaped quotes, so std's default
    // argv encoding (quote the whole argument, escape its inner quotes as
    // `\"`) mangles any command containing quotes — e.g. a redirect target
    // like `echo ok > "C:\path\marker.txt"` exits 1. Hand `cmd` the line
    // verbatim instead.
    let mut c = std::process::Command::new("cmd");
    c.raw_arg(format!("/C {command}"));
    tokio::process::Command::from(c)
}

/// Run `command` through the platform shell, bounded by `budget`. `Ok` iff it
/// exits 0 in time — the execution primitive behind every platform service
/// manager's derived commands.
pub(crate) async fn run_shell(command: &str, budget: Duration) -> crate::Result<()> {
    debug!("running `{command}` (budget {budget:?})");
    let mut child = shell_command(command)
        .spawn()
        .map_err(|e| crate::SentinelError::Monitor(format!("failed to spawn `{command}`: {e}")))?;
    match tokio::time::timeout(budget, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(crate::SentinelError::Monitor(format!(
            "`{command}` exited with {status}"
        ))),
        Ok(Err(e)) => Err(crate::SentinelError::Monitor(format!(
            "`{command}` wait failed: {e}"
        ))),
        Err(_) => {
            let _ = child.start_kill();
            Err(crate::SentinelError::Monitor(format!(
                "`{command}` exceeded {}",
                humantime::format_duration(budget)
            )))
        }
    }
}

// ---- ladder ------------------------------------------------------------

/// Human-readable summary of which ladder rungs ran and how they resolved.
/// Rendered into the escalation message's `%action%` placeholder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LadderOutcome {
    pub rungs: Vec<String>,
}

impl LadderOutcome {
    fn push(&mut self, rung: impl Into<String>) {
        self.rungs.push(rung.into());
    }

    /// Render as a message suffix (`" — corrective action: …"`), or `""` when
    /// no rung ran (notify-only).
    #[must_use]
    pub fn action_suffix(&self) -> String {
        if self.rungs.is_empty() {
            String::new()
        } else {
            format!(" — corrective action: {}", self.rungs.join(", "))
        }
    }
}

/// The single seam the watchdog calls to run the corrective ladder. Abstracted
/// so the watchdog's policy branching can be unit-tested against a recording
/// mock without HTTP or subprocesses.
#[async_trait]
pub trait Corrective: Send + Sync + fmt::Debug {
    async fn run(&self, target: &CorrectiveTarget) -> LadderOutcome;
}

/// The escalating health → abort → restart ladder.
#[derive(Debug)]
pub struct CorrectiveLadder {
    health: Arc<dyn HealthChecker>,
    aborter: Arc<dyn Aborter>,
    restarter: Arc<dyn Restarter>,
    /// Shared one-restart-per-service gate: the restart rung holds the
    /// service's slot across the command and the recovery wait, so it never
    /// races the REST endpoint or health supervision.
    gate: RestartGate,
    /// The constant restart budget shared by every restart path — one budget
    /// for the platform command *and* the recovery wait.
    restart_budget: Duration,
}

impl CorrectiveLadder {
    pub fn new(
        health: Arc<dyn HealthChecker>,
        aborter: Arc<dyn Aborter>,
        restarter: Arc<dyn Restarter>,
        gate: RestartGate,
        restart_budget: Duration,
    ) -> Self {
        Self {
            health,
            aborter,
            restarter,
            gate,
            restart_budget,
        }
    }

    /// Production ladder: HTTP health-check + abort over a shared client, the
    /// platform service manager as the restart rung, restarts coordinated
    /// through the shared `gate`.
    pub fn http(
        http: Arc<dyn HttpClient>,
        gate: RestartGate,
        manager: Arc<dyn ServiceManager>,
        restart_budget: Duration,
    ) -> Self {
        Self::new(
            Arc::new(HttpHealthChecker::new(Arc::clone(&http))),
            Arc::new(HttpAborter::new(http)),
            Arc::new(ManagerRestarter::new(manager)),
            gate,
            restart_budget,
        )
    }

    /// Restart rung: restart the unit, then confirm recovery.
    async fn run_restart(&self, target: &CorrectiveTarget, outcome: &mut LadderOutcome) {
        let Some(_slot) = self.gate.try_acquire(&target.service_name) else {
            debug!(
                "ladder '{}': restart already in flight elsewhere, skipping",
                target.service_name
            );
            outcome.push("restart=skipped(already in flight)");
            return;
        };
        // One budget for the command *and* the recovery wait — measure what
        // the command spends so recovery gets only the remainder (rather than
        // a second full budget).
        let budget = self.restart_budget;
        let started = Instant::now();
        match self.restarter.restart(&target.unit, budget).await {
            Ok(()) => {
                outcome.push("restart=ran");
                let remaining = budget.saturating_sub(started.elapsed());
                if self.await_recovery(target, remaining).await {
                    outcome.push("recovery=responsive");
                } else {
                    outcome.push("recovery=timeout");
                }
            }
            Err(e) => {
                warn!("ladder '{}': restart failed: {e}", target.service_name);
                outcome.push("restart=failed");
            }
        }
    }

    /// Poll health until the service is responsive again or the remaining
    /// restart `budget` runs out. The whole phase — health checks (each up to
    /// `HEALTH_TIMEOUT`) *and* the sleeps between them — is bounded by `budget`
    /// via an outer timeout, so the command and the recovery wait together
    /// never exceed `max_restart_duration`.
    async fn await_recovery(&self, target: &CorrectiveTarget, budget: Duration) -> bool {
        if budget.is_zero() {
            return false;
        }
        let interval = budget.checked_div(RECOVERY_ATTEMPTS).unwrap_or(budget);
        let poll = async {
            for attempt in 0..RECOVERY_ATTEMPTS {
                if self.health.check(target).await == Healthiness::Responsive {
                    return true;
                }
                if attempt.saturating_add(1) < RECOVERY_ATTEMPTS {
                    tokio::time::sleep(interval).await;
                }
            }
            false
        };
        tokio::time::timeout(budget, poll).await.unwrap_or(false)
    }
}

#[async_trait]
impl Corrective for CorrectiveLadder {
    async fn run(&self, target: &CorrectiveTarget) -> LadderOutcome {
        let mut outcome = LadderOutcome::default();

        // Rung 1 — health check.
        let health = self.health.check(target).await;
        debug!("ladder '{}': health {:?}", target.service_name, health);
        outcome.push(format!("health={health}"));

        // Rung 2 — abort, but only a responsive service with an abort verb.
        let aborted = if health == Healthiness::Responsive && target.binding.is_some() {
            match self.aborter.abort(target).await {
                Ok(()) => {
                    debug!("ladder '{}': abort ok", target.service_name);
                    outcome.push("abort=ok");
                    true
                }
                Err(e) => {
                    warn!("ladder '{}': abort failed: {e}", target.service_name);
                    outcome.push("abort=failed");
                    false
                }
            }
        } else {
            false
        };

        // A clean abort is the gentle fix — stop here.
        if aborted {
            return outcome;
        }

        // Rung 3 — restart (unresponsive, abort failed, or no abort verb).
        self.run_restart(target, &mut outcome).await;
        outcome
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    use crate::discovery::{ProbeSpec, RunState};
    use crate::io::{HttpResponse, MockHttpClient};

    /// The tight restart budget every mock-ladder test carries, so recovery
    /// polling resolves in milliseconds.
    const TEST_BUDGET: Duration = Duration::from_millis(20);

    fn service(name: &str) -> DiscoveredService {
        DiscoveredService {
            name: name.to_string(),
            unit: format!("rusty-photon-{name}"),
            state: RunState::Running,
            probe: Some(ProbeSpec {
                health_url: "http://svc/management/v1/configureddevices".to_string(),
                alpaca_base: "http://svc/api/v1".to_string(),
                port: 80,
            }),
        }
    }

    fn target() -> CorrectiveTarget {
        CorrectiveTarget::new("slew", &service("mount"))
    }

    fn compound_target() -> CorrectiveTarget {
        // `centering` has no Alpaca binding.
        CorrectiveTarget::new("centering", &service("rp"))
    }

    // ---- mock rungs ----------------------------------------------------

    #[derive(Debug)]
    struct MockHealth {
        results: Mutex<VecDeque<Healthiness>>,
        default: Healthiness,
        calls: AtomicU32,
    }
    impl MockHealth {
        fn new(default: Healthiness, scripted: Vec<Healthiness>) -> Arc<Self> {
            Arc::new(Self {
                results: Mutex::new(scripted.into()),
                default,
                calls: AtomicU32::new(0),
            })
        }
    }
    #[async_trait]
    impl HealthChecker for MockHealth {
        async fn check(&self, _target: &CorrectiveTarget) -> Healthiness {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(self.default)
        }
    }

    #[derive(Debug)]
    struct MockAbort {
        ok: bool,
        calls: AtomicU32,
    }
    impl MockAbort {
        fn new(ok: bool) -> Arc<Self> {
            Arc::new(Self {
                ok,
                calls: AtomicU32::new(0),
            })
        }
    }
    #[async_trait]
    impl Aborter for MockAbort {
        async fn abort(&self, _target: &CorrectiveTarget) -> crate::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.ok {
                Ok(())
            } else {
                Err(crate::SentinelError::Http("abort boom".to_string()))
            }
        }
    }

    #[derive(Debug)]
    struct MockRestart {
        ok: bool,
        calls: AtomicU32,
        last_unit: Mutex<Option<String>>,
    }
    impl MockRestart {
        fn new(ok: bool) -> Arc<Self> {
            Arc::new(Self {
                ok,
                calls: AtomicU32::new(0),
                last_unit: Mutex::new(None),
            })
        }
    }
    #[async_trait]
    impl Restarter for MockRestart {
        async fn restart(&self, unit: &str, _budget: Duration) -> crate::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_unit.lock().unwrap() = Some(unit.to_string());
            if self.ok {
                Ok(())
            } else {
                Err(crate::SentinelError::Monitor("restart boom".to_string()))
            }
        }
    }

    fn ladder(
        health: Arc<MockHealth>,
        aborter: Arc<MockAbort>,
        restarter: Arc<MockRestart>,
    ) -> CorrectiveLadder {
        CorrectiveLadder::new(
            health,
            aborter,
            restarter,
            RestartGate::default(),
            TEST_BUDGET,
        )
    }

    // ---- ladder orchestration -----------------------------------------

    #[tokio::test(start_paused = true)]
    async fn responsive_service_is_aborted_not_restarted() {
        let health = MockHealth::new(Healthiness::Responsive, vec![]);
        let aborter = MockAbort::new(true);
        let restarter = MockRestart::new(true);
        let l = ladder(health.clone(), aborter.clone(), restarter.clone());

        let outcome = l.run(&target()).await;

        assert_eq!(aborter.calls.load(Ordering::SeqCst), 1, "abort must run");
        assert_eq!(
            restarter.calls.load(Ordering::SeqCst),
            0,
            "a clean abort must stop the ladder before restart"
        );
        assert_eq!(
            outcome.rungs,
            vec!["health=responsive".to_string(), "abort=ok".to_string()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unresponsive_service_is_restarted_then_recovers() {
        // Initial probe unresponsive; recovery probe responsive.
        let health = MockHealth::new(
            Healthiness::Responsive,
            vec![Healthiness::Unresponsive, Healthiness::Responsive],
        );
        let aborter = MockAbort::new(true);
        let restarter = MockRestart::new(true);
        let l = ladder(health.clone(), aborter.clone(), restarter.clone());

        let outcome = l.run(&target()).await;

        assert_eq!(
            aborter.calls.load(Ordering::SeqCst),
            0,
            "no abort when down"
        );
        assert_eq!(restarter.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            restarter.last_unit.lock().unwrap().as_deref(),
            Some("rusty-photon-mount"),
            "the restart rung hands the manager the unit name"
        );
        assert!(outcome.rungs.contains(&"restart=ran".to_string()));
        assert!(outcome.rungs.contains(&"recovery=responsive".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn restart_without_recovery_reports_timeout() {
        // Every probe unresponsive -> recovery never confirmed.
        let health = MockHealth::new(Healthiness::Unresponsive, vec![]);
        let aborter = MockAbort::new(true);
        let restarter = MockRestart::new(true);
        let l = ladder(health, aborter, restarter);

        let outcome = l.run(&target()).await;

        assert!(outcome.rungs.contains(&"restart=ran".to_string()));
        assert!(outcome.rungs.contains(&"recovery=timeout".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn ladder_skips_restart_when_gate_held() {
        let health = MockHealth::new(Healthiness::Unresponsive, vec![]);
        let aborter = MockAbort::new(true);
        let restarter = MockRestart::new(true);
        let gate = RestartGate::default();
        let l = CorrectiveLadder::new(
            health,
            aborter,
            restarter.clone(),
            gate.clone(),
            TEST_BUDGET,
        );

        // Another restart path (REST endpoint / health supervision) holds the
        // target service's slot.
        let _slot = gate.try_acquire("mount").unwrap();
        let outcome = l.run(&target()).await;

        assert_eq!(
            restarter.calls.load(Ordering::SeqCst),
            0,
            "the restart rung must not run while the slot is held"
        );
        assert!(
            outcome
                .rungs
                .contains(&"restart=skipped(already in flight)".to_string()),
            "{:?}",
            outcome.rungs
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ladder_releases_gate_after_run() {
        let health = MockHealth::new(Healthiness::Unresponsive, vec![]);
        let aborter = MockAbort::new(true);
        let restarter = MockRestart::new(true);
        let gate = RestartGate::default();
        let l = CorrectiveLadder::new(health, aborter, restarter, gate.clone(), TEST_BUDGET);

        l.run(&target()).await;

        assert!(
            gate.try_acquire("mount").is_some(),
            "the slot must be free once the ladder finishes"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn abort_failure_falls_through_to_restart() {
        let health = MockHealth::new(Healthiness::Responsive, vec![Healthiness::Responsive]);
        let aborter = MockAbort::new(false);
        let restarter = MockRestart::new(true);
        let l = ladder(health, aborter.clone(), restarter.clone());

        let outcome = l.run(&target()).await;

        assert_eq!(aborter.calls.load(Ordering::SeqCst), 1);
        assert_eq!(restarter.calls.load(Ordering::SeqCst), 1);
        assert!(outcome.rungs.contains(&"abort=failed".to_string()));
        assert!(outcome.rungs.contains(&"restart=ran".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn compound_family_skips_abort_and_restarts() {
        let health = MockHealth::new(Healthiness::Responsive, vec![Healthiness::Responsive]);
        let aborter = MockAbort::new(true);
        let restarter = MockRestart::new(true);
        let l = ladder(health, aborter.clone(), restarter.clone());

        let outcome = l.run(&compound_target()).await;

        assert_eq!(
            aborter.calls.load(Ordering::SeqCst),
            0,
            "a family with no abort verb must skip abort"
        );
        assert_eq!(restarter.calls.load(Ordering::SeqCst), 1);
        assert!(outcome.rungs.iter().any(|r| r.starts_with("health=")));
    }

    #[tokio::test(start_paused = true)]
    async fn restart_failure_is_reported() {
        let health = MockHealth::new(Healthiness::Unresponsive, vec![]);
        let aborter = MockAbort::new(true);
        let restarter = MockRestart::new(false);
        let l = ladder(health, aborter, restarter);

        let outcome = l.run(&target()).await;

        assert!(outcome.rungs.contains(&"restart=failed".to_string()));
        assert!(!outcome.rungs.iter().any(|r| r.starts_with("recovery=")));
    }

    // ---- target / binding ---------------------------------------------

    #[test]
    fn binding_table_maps_known_families() {
        assert_eq!(alpaca_binding("slew").unwrap().device_type, "telescope");
        assert_eq!(alpaca_binding("park").unwrap().abort_verb, "abortslew");
        assert_eq!(
            alpaca_binding("exposure").unwrap().abort_verb,
            "abortexposure"
        );
        assert_eq!(alpaca_binding("move_focuser").unwrap().abort_verb, "halt");
        assert!(alpaca_binding("centering").is_none());
        assert!(alpaca_binding("plate_solve").is_none());
    }

    #[test]
    fn target_builds_alpaca_urls_at_device_zero() {
        let t = CorrectiveTarget::new("exposure", &service("qhy-camera"));
        assert_eq!(
            t.connected_url().unwrap(),
            "http://svc/api/v1/camera/0/connected"
        );
        assert_eq!(
            t.abort_url().unwrap(),
            "http://svc/api/v1/camera/0/abortexposure"
        );
    }

    #[test]
    fn target_with_no_binding_has_no_urls() {
        let t = compound_target();
        assert!(t.connected_url().is_none());
        assert!(t.abort_url().is_none());
    }

    #[test]
    fn target_without_probe_spec_has_no_urls() {
        // A service whose <svc>.json was unreadable: even a family with an
        // abort verb cannot be probed or aborted.
        let t = CorrectiveTarget::new(
            "slew",
            &DiscoveredService {
                probe: None,
                ..service("mount")
            },
        );
        assert!(t.connected_url().is_none());
        assert!(t.abort_url().is_none());
    }

    #[test]
    fn healthiness_labels_are_lowercase() {
        assert_eq!(Healthiness::Responsive.to_string(), "responsive");
        assert_eq!(Healthiness::Unresponsive.to_string(), "unresponsive");
        assert_eq!(Healthiness::Unknown.to_string(), "unknown");
    }

    #[test]
    fn action_suffix_empty_when_no_rungs() {
        assert_eq!(LadderOutcome::default().action_suffix(), "");
    }

    #[test]
    fn action_suffix_lists_rungs() {
        let mut o = LadderOutcome::default();
        o.push("health=responsive");
        o.push("abort=ok");
        assert_eq!(
            o.action_suffix(),
            " — corrective action: health=responsive, abort=ok"
        );
    }

    // ---- default HTTP impls -------------------------------------------

    fn ok_200() -> HttpResponse {
        HttpResponse {
            status: 200,
            body: r#"{"Value": true, "ErrorNumber": 0, "ErrorMessage": ""}"#.to_string(),
        }
    }

    #[tokio::test]
    async fn http_health_200_is_responsive() {
        let mut mock = MockHttpClient::new();
        mock.expect_get()
            .withf(|url| url.contains("/telescope/0/connected"))
            .returning(|_| Box::pin(async { Ok(ok_200()) }));
        let checker = HttpHealthChecker::new(Arc::new(mock));
        assert_eq!(checker.check(&target()).await, Healthiness::Responsive);
    }

    #[tokio::test]
    async fn http_health_non_200_is_unresponsive() {
        let mut mock = MockHttpClient::new();
        mock.expect_get().returning(|_| {
            Box::pin(async {
                Ok(HttpResponse {
                    status: 500,
                    body: String::new(),
                })
            })
        });
        let checker = HttpHealthChecker::new(Arc::new(mock));
        assert_eq!(checker.check(&target()).await, Healthiness::Unresponsive);
    }

    #[tokio::test]
    async fn http_health_error_is_unresponsive() {
        let mut mock = MockHttpClient::new();
        mock.expect_get()
            .returning(|_| Box::pin(async { Err(crate::SentinelError::Http("refused".into())) }));
        let checker = HttpHealthChecker::new(Arc::new(mock));
        assert_eq!(checker.check(&target()).await, Healthiness::Unresponsive);
    }

    #[tokio::test]
    async fn http_health_no_binding_is_unknown() {
        // No HTTP call expected — the mock has no expectations set.
        let checker = HttpHealthChecker::new(Arc::new(MockHttpClient::new()));
        assert_eq!(
            checker.check(&compound_target()).await,
            Healthiness::Unknown
        );
    }

    #[tokio::test]
    async fn http_abort_puts_verb_and_succeeds() {
        let mut mock = MockHttpClient::new();
        mock.expect_put_form()
            .withf(|url, params| {
                url.ends_with("/telescope/0/abortslew") && params.contains(&("ClientID", "1"))
            })
            .returning(|_, _| {
                Box::pin(async {
                    Ok(HttpResponse {
                        status: 200,
                        body: String::new(),
                    })
                })
            });
        let aborter = HttpAborter::new(Arc::new(mock));
        aborter.abort(&target()).await.unwrap();
    }

    #[tokio::test]
    async fn http_abort_no_verb_errors() {
        let aborter = HttpAborter::new(Arc::new(MockHttpClient::new()));
        let err = aborter.abort(&compound_target()).await.unwrap_err();
        assert!(err.to_string().contains("no abort verb"), "{err}");
    }

    #[tokio::test]
    async fn http_abort_non_200_errors() {
        let mut mock = MockHttpClient::new();
        mock.expect_put_form().returning(|_, _| {
            Box::pin(async {
                Ok(HttpResponse {
                    status: 409,
                    body: String::new(),
                })
            })
        });
        let aborter = HttpAborter::new(Arc::new(mock));
        let err = aborter.abort(&target()).await.unwrap_err();
        assert!(err.to_string().contains("409"), "{err}");
    }

    #[tokio::test]
    async fn http_abort_200_with_ascom_error_is_rejected() {
        // Alpaca signals a rejected abort via a non-zero ErrorNumber on a 200.
        let mut mock = MockHttpClient::new();
        mock.expect_put_form().returning(|_, _| {
            Box::pin(async {
                Ok(HttpResponse {
                    status: 200,
                    body: r#"{"ErrorNumber": 1025, "ErrorMessage": "Invalid while parked"}"#
                        .to_string(),
                })
            })
        });
        let aborter = HttpAborter::new(Arc::new(mock));
        let err = aborter.abort(&target()).await.unwrap_err();
        assert!(err.to_string().contains("1025"), "{err}");
        assert!(err.to_string().contains("Invalid while parked"), "{err}");
    }

    #[tokio::test]
    async fn http_abort_200_with_error_number_zero_succeeds() {
        let mut mock = MockHttpClient::new();
        mock.expect_put_form().returning(|_, _| {
            Box::pin(async {
                Ok(HttpResponse {
                    status: 200,
                    body: r#"{"ErrorNumber": 0, "ErrorMessage": ""}"#.to_string(),
                })
            })
        });
        let aborter = HttpAborter::new(Arc::new(mock));
        aborter.abort(&target()).await.unwrap();
    }

    // ---- shell primitive (exit-code tests unix only — they use `sh`
    //      built-ins; the quoted-path test runs on both platforms) ------

    #[cfg(unix)]
    #[tokio::test]
    async fn run_shell_zero_exit_ok() {
        run_shell("true", Duration::from_secs(5)).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_shell_nonzero_exit_errors() {
        let err = run_shell("false", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exited"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_shell_times_out() {
        let err = run_shell("sleep 5", Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeded"), "{err}");
    }

    // Cross-platform (the command shape the platform managers derive). On
    // Windows this regression-tests the `raw_arg` invocation: std's default
    // argv encoding escapes the inner quotes as `\"`, which `cmd` does not
    // parse — the space in the file name makes the quotes load-bearing.
    #[tokio::test]
    async fn run_shell_preserves_quoted_paths() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("restart marker.txt");
        let command = format!("echo ok > \"{}\"", marker.display());
        run_shell(&command, Duration::from_secs(5)).await.unwrap();
        assert!(marker.exists(), "`{command}` did not write the marker");
    }
}
