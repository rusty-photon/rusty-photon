//! The Service Restart API's engine: `POST /api/services/{name}/restart`.
//!
//! Sentinel owns *process* restart for the rest of the stack (drivers reload
//! themselves via `config.apply`; sentinel restarts processes). `{name}` is a
//! [discovered service](crate::discovery); the [`RestartManager`] hands its
//! unit to the platform service manager and then polls the platform's
//! recovery check until it passes or the constant restart budget elapses.
//! The command's outcome is a *domain* result ([`RestartReport`], HTTP 200
//! either way); addressing errors ([`RestartError`]) map to 4xx. See
//! `docs/services/sentinel.md` §Service Restart API.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::discovery::{ServiceManager, ServiceRegistry};

/// How many times recovery re-runs the platform check before giving up. The
/// total wait is bounded by the restart budget regardless.
const RECOVERY_ATTEMPTS: u32 = 5;

/// Addressing errors — the dashboard handler maps these to 4xx.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RestartError {
    /// `{name}` is not a discovered service (404).
    #[error("no discovered service named '{0}'")]
    UnknownService(String),
    /// Another restart of this service is still running (409).
    #[error("a restart of '{0}' is already in flight")]
    AlreadyInFlight(String),
}

/// Recovery-confirmation outcome, reported in the response body.
///
/// `Display` yields the lowercase label used in operator-facing notification
/// text; serde yields the same lowercase strings as the `recovery` field of
/// [`RestartReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, derive_more::Display)]
#[serde(rename_all = "lowercase")]
pub enum Recovery {
    /// The platform's recovery check passed within the remaining budget.
    #[display("healthy")]
    Healthy,
    /// The recovery check never passed before the budget elapsed.
    #[display("timeout")]
    Timeout,
    /// The platform has no recovery check (Homebrew) — confirmation skipped.
    #[display("skipped")]
    Skipped,
}

/// Domain outcome of a restart request. `status:"failed"` is still HTTP 200 —
/// the *action* ran and this is its result (mirroring `config.apply`'s
/// `status:"invalid"` convention).
#[derive(Debug, Clone, Serialize)]
pub struct RestartReport {
    pub service: String,
    /// `"ok"` (command exited 0) or `"failed"`.
    pub status: &'static str,
    /// Present on `"ok"`: whether recovery was confirmed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<Recovery>,
    /// Present on `"failed"`: what went wrong with the command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One-restart-per-service gate, shared by every restart path — the REST
/// endpoint, the watchdog corrective ladder, and health supervision. Clones
/// share the same in-flight set, so whichever path acquires a service's slot
/// first excludes the others until the slot drops.
#[derive(Debug, Clone, Default)]
pub struct RestartGate(Arc<Mutex<HashSet<String>>>);

impl RestartGate {
    /// Claim `name`'s slot. `None` means a restart of that service is already
    /// in flight somewhere.
    #[must_use]
    pub fn try_acquire(&self, name: &str) -> Option<RestartSlot> {
        let mut in_flight = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !in_flight.insert(name.to_string()) {
            return None;
        }
        Some(RestartSlot {
            set: Arc::clone(&self.0),
            name: name.to_string(),
        })
    }
}

/// RAII slot in the gate's in-flight set: dropping releases it, so a restart
/// that panics or is cancelled never wedges its service's slot.
#[derive(Debug)]
pub struct RestartSlot {
    set: Arc<Mutex<HashSet<String>>>,
    name: String,
}

impl Drop for RestartSlot {
    fn drop(&mut self) {
        let mut in_flight = self
            .set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        in_flight.remove(&self.name);
    }
}

/// The restart engine over the discovered-service registry, the platform
/// service manager, and the shared [`RestartGate`].
#[derive(derive_more::Debug)]
pub struct RestartManager {
    /// The registry the discovery loop maintains; `{name}` resolves here.
    registry: ServiceRegistry,
    #[debug(skip)]
    manager: Arc<dyn ServiceManager>,
    /// The constant restart budget — one budget for the platform command
    /// *and* the recovery wait together.
    budget: Duration,
    gate: RestartGate,
}

impl RestartManager {
    pub fn new(
        registry: ServiceRegistry,
        manager: Arc<dyn ServiceManager>,
        budget: Duration,
    ) -> Self {
        Self {
            registry,
            manager,
            budget,
            gate: RestartGate::default(),
        }
    }

    /// A clone of the shared gate, for wiring into the other restart paths
    /// (the corrective ladder) so they exclude this manager's restarts.
    #[must_use]
    pub fn gate(&self) -> RestartGate {
        self.gate.clone()
    }

    /// Restart `name`: hand its unit to the platform, then confirm recovery
    /// via the platform's check. Blocks until done — bounded by the budget.
    /// Works for any discovered service regardless of run state (the manual
    /// restart is the operator's recovery hammer).
    pub async fn restart(&self, name: &str) -> Result<RestartReport, RestartError> {
        let unit = self
            .registry
            .read()
            .await
            .get(name)
            .map(|s| s.unit.clone())
            .ok_or_else(|| RestartError::UnknownService(name.to_string()))?;
        let _slot = self
            .gate
            .try_acquire(name)
            .ok_or_else(|| RestartError::AlreadyInFlight(name.to_string()))?;

        let budget = self.budget;
        info!(
            "restarting service '{name}' (unit {unit}, budget {})",
            humantime::format_duration(budget)
        );
        let started = Instant::now();
        match self.manager.restart(&unit, budget).await {
            Ok(()) => {
                let remaining = budget.saturating_sub(started.elapsed());
                let recovery = self.await_recovered(name, &unit, remaining).await;
                info!("service '{name}' restarted (recovery: {recovery:?})");
                Ok(RestartReport {
                    service: name.to_string(),
                    status: "ok",
                    recovery: Some(recovery),
                    detail: None,
                })
            }
            Err(e) => {
                warn!("service '{name}' restart failed: {e}");
                Ok(RestartReport {
                    service: name.to_string(),
                    status: "failed",
                    recovery: None,
                    detail: Some(e.to_string()),
                })
            }
        }
    }

    /// Poll the platform's recovery check until it passes or the remaining
    /// `budget` runs out. The whole phase — every check run, the first one
    /// included, *and* the sleeps between them — sits under one outer
    /// timeout, so even a wedged platform check cannot escape the budget.
    /// (tokio's timeout polls the inner future once before the deadline, so
    /// a zero remaining budget still lets an instantly-resolving check
    /// report Skipped/Healthy instead of blindly timing out.)
    async fn await_recovered(&self, name: &str, unit: &str, budget: Duration) -> Recovery {
        let interval = budget.checked_div(RECOVERY_ATTEMPTS).unwrap_or(budget);
        let poll = async {
            for attempt in 0..RECOVERY_ATTEMPTS {
                match self.manager.recovery_check(unit).await {
                    None => return Recovery::Skipped,
                    Some(true) => return Recovery::Healthy,
                    Some(false) => debug!("service '{name}' not yet recovered"),
                }
                if attempt.saturating_add(1) < RECOVERY_ATTEMPTS {
                    tokio::time::sleep(interval).await;
                }
            }
            Recovery::Timeout
        };
        tokio::time::timeout(budget, poll)
            .await
            .unwrap_or(Recovery::Timeout)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};

    use async_trait::async_trait;

    use crate::discovery::{DiscoveredService, DiscoveredUnit, RunState};

    /// Scripted service manager: records restarts, answers a fixed recovery
    /// sequence (then repeats the last answer).
    #[derive(Debug)]
    struct ScriptedManager {
        restart_ok: bool,
        restarts: Mutex<Vec<String>>,
        recovery: Mutex<Vec<Option<bool>>>,
        recovery_calls: AtomicU32,
        /// When set, the restart blocks on this notification before returning
        /// (used to hold a restart in flight).
        hold: Option<Arc<tokio::sync::Notify>>,
    }

    impl ScriptedManager {
        fn new(restart_ok: bool, recovery: Vec<Option<bool>>) -> Self {
            Self {
                restart_ok,
                restarts: Mutex::new(Vec::new()),
                recovery: Mutex::new(recovery),
                recovery_calls: AtomicU32::new(0),
                hold: None,
            }
        }
    }

    #[async_trait]
    impl ServiceManager for ScriptedManager {
        async fn enumerate(&self) -> crate::Result<Vec<DiscoveredUnit>> {
            Ok(Vec::new())
        }

        async fn restart(&self, unit: &str, _budget: Duration) -> crate::Result<()> {
            self.restarts.lock().unwrap().push(unit.to_string());
            if let Some(hold) = &self.hold {
                hold.notified().await;
            }
            if self.restart_ok {
                Ok(())
            } else {
                Err(crate::SentinelError::Monitor(
                    "`systemctl restart` exited with 1".to_string(),
                ))
            }
        }

        async fn recovery_check(&self, _unit: &str) -> Option<bool> {
            self.recovery_calls.fetch_add(1, Ordering::SeqCst);
            let scripted = self.recovery.lock().unwrap();
            let idx = (self.recovery_calls.load(Ordering::SeqCst) as usize - 1)
                .min(scripted.len().saturating_sub(1));
            scripted.get(idx).copied().unwrap_or(Some(true))
        }
    }

    fn registry_with(names: &[&str]) -> ServiceRegistry {
        let map: HashMap<String, DiscoveredService> = names
            .iter()
            .map(|n| {
                (
                    n.to_string(),
                    DiscoveredService {
                        name: n.to_string(),
                        unit: format!("rusty-photon-{n}"),
                        state: RunState::Running,
                        probe: None,
                    },
                )
            })
            .collect();
        Arc::new(tokio::sync::RwLock::new(map))
    }

    fn manager(
        names: &[&str],
        scripted: ScriptedManager,
    ) -> (RestartManager, Arc<ScriptedManager>) {
        let scripted = Arc::new(scripted);
        (
            RestartManager::new(
                registry_with(names),
                Arc::<ScriptedManager>::clone(&scripted),
                Duration::from_secs(1),
            ),
            scripted,
        )
    }

    #[test]
    fn recovery_serde_form_is_lowercase() {
        assert_eq!(
            serde_json::to_string(&Recovery::Healthy).unwrap(),
            "\"healthy\""
        );
        assert_eq!(
            serde_json::to_string(&Recovery::Timeout).unwrap(),
            "\"timeout\""
        );
        assert_eq!(
            serde_json::to_string(&Recovery::Skipped).unwrap(),
            "\"skipped\""
        );
    }

    #[tokio::test]
    async fn unknown_service_is_rejected() {
        let (m, _) = manager(&[], ScriptedManager::new(true, vec![Some(true)]));
        let err = m.restart("nope").await.unwrap_err();
        assert_eq!(err, RestartError::UnknownService("nope".to_string()));
        assert!(err.to_string().contains("no discovered service"), "{err}");
    }

    #[tokio::test]
    async fn restart_hands_the_unit_to_the_platform() {
        let (m, scripted) = manager(&["dsd-fp2"], ScriptedManager::new(true, vec![Some(true)]));
        let report = m.restart("dsd-fp2").await.unwrap();
        assert_eq!(report.status, "ok");
        assert_eq!(report.recovery, Some(Recovery::Healthy));
        assert_eq!(
            *scripted.restarts.lock().unwrap(),
            vec!["rusty-photon-dsd-fp2".to_string()],
            "the platform receives the unit name, not the service name"
        );
    }

    #[tokio::test]
    async fn platform_without_recovery_check_skips() {
        let (m, _) = manager(&["svc"], ScriptedManager::new(true, vec![None]));
        let report = m.restart("svc").await.unwrap();
        assert_eq!(report.status, "ok");
        assert_eq!(report.recovery, Some(Recovery::Skipped));
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_that_never_passes_reports_timeout() {
        let (m, scripted) = manager(&["svc"], ScriptedManager::new(true, vec![Some(false)]));
        let report = m.restart("svc").await.unwrap();
        assert_eq!(report.status, "ok");
        assert_eq!(report.recovery, Some(Recovery::Timeout));
        assert!(
            scripted.recovery_calls.load(Ordering::SeqCst) >= 2,
            "recovery must be re-checked, not decided on one probe"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_passing_on_a_later_check_is_healthy() {
        let (m, _) = manager(
            &["svc"],
            ScriptedManager::new(true, vec![Some(false), Some(false), Some(true)]),
        );
        let report = m.restart("svc").await.unwrap();
        assert_eq!(report.recovery, Some(Recovery::Healthy));
    }

    #[tokio::test]
    async fn failing_restart_reports_failed_with_detail() {
        let (m, scripted) = manager(&["svc"], ScriptedManager::new(false, vec![Some(true)]));
        let report = m.restart("svc").await.unwrap();
        assert_eq!(report.status, "failed");
        assert_eq!(report.recovery, None);
        assert!(
            report.detail.as_deref().unwrap_or("").contains("exited"),
            "{report:?}"
        );
        assert_eq!(
            scripted.recovery_calls.load(Ordering::SeqCst),
            0,
            "a failed restart must not poll recovery"
        );
    }

    #[test]
    fn gate_second_acquire_fails_until_slot_dropped() {
        let gate = RestartGate::default();
        let slot = gate.try_acquire("svc").expect("first acquire must succeed");
        assert!(gate.try_acquire("svc").is_none(), "slot is held");
        assert!(
            gate.try_acquire("other").is_some(),
            "different services are independent"
        );
        drop(slot);
        assert!(gate.try_acquire("svc").is_some(), "drop releases the slot");
    }

    #[test]
    fn gate_clones_share_the_set() {
        let gate = RestartGate::default();
        let clone = gate.clone();
        let _slot = clone.try_acquire("svc").unwrap();
        assert!(
            gate.try_acquire("svc").is_none(),
            "a slot held via a clone must block the original"
        );
    }

    #[tokio::test]
    async fn manager_gate_accessor_shares_in_flight_set() {
        let (m, scripted) = manager(&["svc"], ScriptedManager::new(true, vec![Some(true)]));
        let slot = m.gate().try_acquire("svc").unwrap();
        let err = m.restart("svc").await.unwrap_err();
        assert_eq!(err, RestartError::AlreadyInFlight("svc".to_string()));
        assert!(
            scripted.restarts.lock().unwrap().is_empty(),
            "no command may run while the gate slot is held externally"
        );
        drop(slot);
        assert_eq!(m.restart("svc").await.unwrap().status, "ok");
    }

    #[tokio::test]
    async fn concurrent_restart_of_same_service_is_rejected() {
        let notify = Arc::new(tokio::sync::Notify::new());
        let scripted = ScriptedManager {
            hold: Some(Arc::clone(&notify)),
            ..ScriptedManager::new(true, vec![Some(true)])
        };
        let (m, _) = manager(&["svc"], scripted);
        let m = Arc::new(m);

        // Hold the first restart mid-command, then race a second one.
        let first = tokio::spawn({
            let m = Arc::clone(&m);
            async move { m.restart("svc").await }
        });
        tokio::task::yield_now().await;
        let err = m.restart("svc").await.unwrap_err();
        assert_eq!(err, RestartError::AlreadyInFlight("svc".to_string()));

        // Release the held command; the first restart completes and frees the
        // slot, so a follow-up restart is accepted again.
        notify.notify_one();
        first.await.unwrap().unwrap();
        notify.notify_one();
        let report = m.restart("svc").await.unwrap();
        assert_eq!(report.status, "ok");
    }
}
