//! Shared state for monitor statuses and notification history

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::discovery::{DiscoveredService, RunState};
use crate::monitor::MonitorState;
use crate::notifier::NotificationRecord;

/// Status of a single monitor
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorStatus {
    pub name: String,
    pub state: MonitorState,
    pub last_poll_epoch_ms: u64,
    pub last_change_epoch_ms: Option<u64>,
    pub consecutive_errors: u32,
    #[serde(with = "humantime_serde")]
    pub polling_interval: Duration,
}

/// Health of a discovered service as seen by its health supervisor.
///
/// The two casings below are deliberately different and must not be unified:
/// `Display` yields the `PascalCase` variant name, which is the operator-facing
/// text (the dashboard badge), while serde yields lowercase, which is the
/// JSON wire form served by `GET /api/services`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, derive_more::Display)]
#[serde(rename_all = "lowercase")]
pub enum ServiceHealth {
    /// Never probed, no derivable probe URL, or not in a probed run state.
    Unknown,
    /// Last probe answered alive (200, or an auth challenge — 401/403).
    Up,
    /// Last probe answered 503: the service is alive but reports an
    /// external dependency unavailable — a state a restart cannot cure,
    /// so it counts as alive for supervision (issue #595).
    Degraded,
    /// Last probe failed (other status, timeout, or connection error), or
    /// the unit is in the `failed` run state.
    Down,
}

/// Snapshot of one discovered service.
///
/// Set membership, `unit`, and `run_state` are maintained by the
/// discovery loop; the health fields are published by the service's
/// health supervisor after every probe (single writer per service).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceHealthStatus {
    pub name: String,
    /// The platform unit name (`rusty-photon-<name>`).
    pub unit: String,
    /// The discovery classification.
    pub run_state: RunState,
    pub health: ServiceHealth,
    /// The opaque `message` string from a degraded probe's 503 JSON body
    /// (truncated to 200 chars), passed through for display only — sentinel
    /// never interprets it. Always `None` unless `health` is `Degraded`.
    #[serde(default)]
    pub health_message: Option<String>,
    /// 0 until the first probe completes.
    pub last_probe_epoch_ms: u64,
    pub consecutive_failures: u32,
    /// Autonomous restarts in the current outage; resets on recovery.
    pub restarts_in_outage: u32,
    /// Autonomous restarts since sentinel started.
    pub total_restarts: u64,
    /// Backoff-scheduled earliest next autonomous restart; `None` when none
    /// is scheduled.
    pub next_restart_epoch_ms: Option<u64>,
    /// The service's listening port, derived by discovery from its config's
    /// `server` block (`None` when no probe is derivable). Exposed through
    /// `GET /api/services` so clients can match a device URL to its service.
    #[serde(default)]
    pub probe_port: Option<u16>,
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
}

impl ServiceHealthStatus {
    /// The unprobed snapshot a newly discovered service starts from.
    #[must_use]
    pub const fn unknown(
        name: String,
        unit: String,
        run_state: RunState,
        probe_port: Option<u16>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            name,
            unit,
            run_state,
            health: ServiceHealth::Unknown,
            health_message: None,
            last_probe_epoch_ms: 0,
            consecutive_failures: 0,
            restarts_in_outage: 0,
            total_restarts: 0,
            next_restart_epoch_ms: None,
            probe_port,
            poll_interval,
        }
    }
}

/// Shared state accessible by engine and dashboard
#[derive(Debug)]
pub struct SharedState {
    pub monitors: Vec<MonitorStatus>,
    pub services: Vec<ServiceHealthStatus>,
    pub history: VecDeque<NotificationRecord>,
    pub history_max_size: usize,
    pub started_at: Instant,
}

impl SharedState {
    #[must_use]
    pub fn new(monitors_with_intervals: Vec<(String, Duration)>, history_max_size: usize) -> Self {
        let monitors = monitors_with_intervals
            .into_iter()
            .map(|(name, polling_interval)| MonitorStatus {
                name,
                state: MonitorState::Unknown,
                last_poll_epoch_ms: 0,
                last_change_epoch_ms: None,
                consecutive_errors: 0,
                polling_interval,
            })
            .collect();

        Self {
            monitors,
            // Populated by the discovery loop; empty until the first pass.
            services: Vec::new(),
            history: VecDeque::with_capacity(history_max_size),
            history_max_size,
            started_at: Instant::now(),
        }
    }

    /// Replace a discovered service's snapshot (matched by name). The
    /// supervisor's local state machine is authoritative; this is push-only.
    /// An unseeded name is inserted defensively, keeping name order.
    pub fn set_service_health(&mut self, status: ServiceHealthStatus) {
        if let Some(slot) = self.services.iter_mut().find(|s| s.name == status.name) {
            *slot = status;
        } else {
            self.services.push(status);
            self.services.sort_by(|a, b| a.name.cmp(&b.name));
        }
    }

    /// Reconcile the snapshot list with a discovery pass: insert a fresh
    /// `unknown` slot for every newly discovered service, refresh `unit` and
    /// `run_state` on existing ones (their health fields belong to their
    /// supervisor while one runs), drop services that left the discovered
    /// set, and keep the list name-sorted for deterministic dashboard/API
    /// order. A service that leaves the supervised run states also has its
    /// outage state reset here: its supervisor may be reaped before its next
    /// tick, and a stood-down service displayed as `down` with a scheduled
    /// restart would contradict what supervision is actually doing. The
    /// lifetime restart counter and the last-probe stamp are history and
    /// survive.
    pub fn sync_discovered_services(
        &mut self,
        services: &std::collections::HashMap<String, DiscoveredService>,
        poll_interval: Duration,
    ) {
        self.services.retain(|s| services.contains_key(&s.name));
        for svc in services.values() {
            match self.services.iter_mut().find(|s| s.name == svc.name) {
                Some(slot) => {
                    slot.unit.clone_from(&svc.unit);
                    slot.run_state = svc.state;
                    slot.probe_port = svc.probe.as_ref().map(|p| p.port);
                    if !svc.state.supervised() {
                        slot.health = ServiceHealth::Unknown;
                        slot.health_message = None;
                        slot.consecutive_failures = 0;
                        slot.restarts_in_outage = 0;
                        slot.next_restart_epoch_ms = None;
                    }
                }
                None => self.services.push(ServiceHealthStatus::unknown(
                    svc.name.clone(),
                    svc.unit.clone(),
                    svc.state,
                    svc.probe.as_ref().map(|p| p.port),
                    poll_interval,
                )),
            }
        }
        self.services.sort_by(|a, b| a.name.cmp(&b.name));
    }

    /// Update a monitor's state, returning true if the state changed
    pub fn update_monitor(&mut self, name: &str, new_state: MonitorState, now_ms: u64) -> bool {
        if let Some(status) = self.monitors.iter_mut().find(|m| m.name == name) {
            let changed = status.state != new_state;
            status.state = new_state;
            status.last_poll_epoch_ms = now_ms;
            if new_state == MonitorState::Unknown {
                status.consecutive_errors = status.consecutive_errors.saturating_add(1);
            } else {
                status.consecutive_errors = 0;
            }
            if changed {
                status.last_change_epoch_ms = Some(now_ms);
            }
            changed
        } else {
            false
        }
    }

    /// Get a monitor's current state
    #[must_use]
    pub fn get_monitor_state(&self, name: &str) -> Option<MonitorState> {
        self.monitors
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.state)
    }

    /// Get a monitor's previous state (before last update) — uses current state as proxy
    #[must_use]
    pub fn get_monitor_consecutive_errors(&self, name: &str) -> u32 {
        self.monitors
            .iter()
            .find(|m| m.name == name)
            .map_or(0, |m| m.consecutive_errors)
    }

    /// Add a notification to history
    pub fn add_notification(&mut self, record: NotificationRecord) {
        if self.history.len() >= self.history_max_size {
            self.history.pop_front();
        }
        self.history.push_back(record);
    }
}

/// Thread-safe shared state handle
pub type StateHandle = Arc<RwLock<SharedState>>;

#[must_use]
pub fn new_state_handle(
    monitors_with_intervals: Vec<(String, Duration)>,
    history_max_size: usize,
) -> StateHandle {
    Arc::new(RwLock::new(SharedState::new(
        monitors_with_intervals,
        history_max_size,
    )))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn service_health_display_is_pascal_case() {
        assert_eq!(ServiceHealth::Unknown.to_string(), "Unknown");
        assert_eq!(ServiceHealth::Up.to_string(), "Up");
        assert_eq!(ServiceHealth::Degraded.to_string(), "Degraded");
        assert_eq!(ServiceHealth::Down.to_string(), "Down");
    }

    #[test]
    fn service_health_serde_form_is_lowercase_and_differs_from_display() {
        assert_eq!(
            serde_json::to_string(&ServiceHealth::Unknown).unwrap(),
            "\"unknown\""
        );
        assert_eq!(serde_json::to_string(&ServiceHealth::Up).unwrap(), "\"up\"");
        assert_eq!(
            serde_json::to_string(&ServiceHealth::Degraded).unwrap(),
            "\"degraded\""
        );
        assert_eq!(
            serde_json::to_string(&ServiceHealth::Down).unwrap(),
            "\"down\""
        );
    }

    #[test]
    fn new_state_has_unknown_monitors() {
        let state = SharedState::new(
            vec![
                ("m1".to_string(), Duration::from_secs(30)),
                ("m2".to_string(), Duration::from_mins(1)),
            ],
            10,
        );
        assert_eq!(state.monitors.len(), 2);
        assert_eq!(state.monitors[0].state, MonitorState::Unknown);
        assert_eq!(state.monitors[0].polling_interval, Duration::from_secs(30));
        assert_eq!(state.monitors[1].state, MonitorState::Unknown);
        assert_eq!(state.monitors[1].polling_interval, Duration::from_mins(1));
    }

    #[test]
    fn update_monitor_returns_true_on_change() {
        let mut state = SharedState::new(vec![("m1".to_string(), Duration::from_secs(30))], 10);
        let changed = state.update_monitor("m1", MonitorState::Safe, 1000);
        assert!(changed);
        assert_eq!(state.monitors[0].state, MonitorState::Safe);
        assert_eq!(state.monitors[0].last_change_epoch_ms, Some(1000));
    }

    #[test]
    fn update_monitor_returns_false_on_same_state() {
        let mut state = SharedState::new(vec![("m1".to_string(), Duration::from_secs(30))], 10);
        state.update_monitor("m1", MonitorState::Safe, 1000);
        let changed = state.update_monitor("m1", MonitorState::Safe, 2000);
        assert!(!changed);
        assert_eq!(state.monitors[0].last_poll_epoch_ms, 2000);
        assert_eq!(state.monitors[0].last_change_epoch_ms, Some(1000));
    }

    #[test]
    fn update_unknown_increments_error_count() {
        let mut state = SharedState::new(vec![("m1".to_string(), Duration::from_secs(30))], 10);
        state.update_monitor("m1", MonitorState::Unknown, 1000);
        assert_eq!(state.monitors[0].consecutive_errors, 1);
        state.update_monitor("m1", MonitorState::Unknown, 2000);
        assert_eq!(state.monitors[0].consecutive_errors, 2);
    }

    #[test]
    fn update_resets_error_count_on_recovery() {
        let mut state = SharedState::new(vec![("m1".to_string(), Duration::from_secs(30))], 10);
        state.update_monitor("m1", MonitorState::Unknown, 1000);
        state.update_monitor("m1", MonitorState::Unknown, 2000);
        assert_eq!(state.monitors[0].consecutive_errors, 2);
        state.update_monitor("m1", MonitorState::Safe, 3000);
        assert_eq!(state.monitors[0].consecutive_errors, 0);
    }

    #[test]
    fn update_unknown_monitor_returns_false() {
        let mut state = SharedState::new(vec![("m1".to_string(), Duration::from_secs(30))], 10);
        let changed = state.update_monitor("nonexistent", MonitorState::Safe, 1000);
        assert!(!changed);
    }

    #[test]
    fn history_respects_max_size() {
        let mut state = SharedState::new(vec![], 2);
        for i in 0..5 {
            state.add_notification(NotificationRecord {
                monitor_name: format!("m{i}"),
                notifier_type: "pushover".to_string(),
                message: format!("msg{i}"),
                success: true,
                error: None,
                timestamp_epoch_ms: i * 1000,
            });
        }
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history[0].monitor_name, "m3");
        assert_eq!(state.history[1].monitor_name, "m4");
    }

    #[test]
    fn get_monitor_state() {
        let mut state = SharedState::new(vec![("m1".to_string(), Duration::from_secs(30))], 10);
        assert_eq!(state.get_monitor_state("m1"), Some(MonitorState::Unknown));
        state.update_monitor("m1", MonitorState::Safe, 1000);
        assert_eq!(state.get_monitor_state("m1"), Some(MonitorState::Safe));
        assert_eq!(state.get_monitor_state("nonexistent"), None);
    }

    #[test]
    fn get_consecutive_errors_for_unknown_monitor_returns_zero() {
        let state = SharedState::new(vec![("m1".to_string(), Duration::from_secs(30))], 10);
        assert_eq!(state.get_monitor_consecutive_errors("nonexistent"), 0);
    }

    fn discovered(name: &str, state: RunState) -> DiscoveredService {
        DiscoveredService {
            name: name.to_string(),
            unit: format!("rusty-photon-{name}"),
            state,
            probe: None,
        }
    }

    fn pass(services: &[(&str, RunState)]) -> std::collections::HashMap<String, DiscoveredService> {
        services
            .iter()
            .map(|(n, s)| (n.to_string(), discovered(n, *s)))
            .collect()
    }

    #[test]
    fn sync_seeds_new_services_unknown_and_sorted() {
        let mut state = SharedState::new(vec![], 10);
        state.sync_discovered_services(
            &pass(&[
                ("plate-solver", RunState::Inert),
                ("dsd-fp2", RunState::Running),
            ]),
            Duration::from_secs(30),
        );
        assert_eq!(state.services.len(), 2);
        assert_eq!(state.services[0].name, "dsd-fp2", "name-sorted");
        let service = &state.services[1];
        assert_eq!(service.name, "plate-solver");
        assert_eq!(service.unit, "rusty-photon-plate-solver");
        assert_eq!(service.run_state, RunState::Inert);
        assert_eq!(service.health, ServiceHealth::Unknown);
        assert_eq!(service.last_probe_epoch_ms, 0);
        assert_eq!(service.total_restarts, 0);
        assert_eq!(service.poll_interval, Duration::from_secs(30));
    }

    #[test]
    fn sync_refreshes_run_state_but_preserves_health_fields() {
        let mut state = SharedState::new(vec![], 10);
        state.sync_discovered_services(
            &pass(&[("svc", RunState::Running)]),
            Duration::from_secs(30),
        );
        let mut status = state.services[0].clone();
        status.health = ServiceHealth::Down;
        status.consecutive_failures = 2;
        status.restarts_in_outage = 1;
        status.next_restart_epoch_ms = Some(999);
        status.total_restarts = 3;
        status.last_probe_epoch_ms = 12_345;
        state.set_service_health(status);

        // A supervised-state refresh leaves the supervisor's fields alone.
        state
            .sync_discovered_services(&pass(&[("svc", RunState::Failed)]), Duration::from_secs(30));
        let service = &state.services[0];
        assert_eq!(service.run_state, RunState::Failed, "run state refreshed");
        assert_eq!(
            service.health,
            ServiceHealth::Down,
            "the supervisor's health fields survive a supervised refresh"
        );
        assert_eq!(service.consecutive_failures, 2);

        // Leaving the supervised states resets the outage — the supervisor
        // may be reaped before its next tick, and a stood-down service must
        // not stay displayed as down with a scheduled restart.
        state.sync_discovered_services(
            &pass(&[("svc", RunState::Stopped)]),
            Duration::from_secs(30),
        );
        let service = &state.services[0];
        assert_eq!(service.run_state, RunState::Stopped);
        assert_eq!(service.health, ServiceHealth::Unknown);
        assert_eq!(service.consecutive_failures, 0);
        assert_eq!(service.restarts_in_outage, 0);
        assert_eq!(service.next_restart_epoch_ms, None);
        assert_eq!(service.total_restarts, 3, "lifetime counter is history");
        assert_eq!(
            service.last_probe_epoch_ms, 12_345,
            "probe stamp is history"
        );
    }

    #[test]
    fn sync_drops_services_that_left_the_discovered_set() {
        let mut state = SharedState::new(vec![], 10);
        state.sync_discovered_services(
            &pass(&[("a", RunState::Running), ("b", RunState::Running)]),
            Duration::from_secs(30),
        );
        assert_eq!(state.services.len(), 2);
        state.sync_discovered_services(&pass(&[("b", RunState::Running)]), Duration::from_secs(30));
        assert_eq!(state.services.len(), 1);
        assert_eq!(state.services[0].name, "b");
    }

    #[test]
    fn set_service_health_replaces_by_name() {
        let mut state = SharedState::new(vec![], 10);
        state.sync_discovered_services(
            &pass(&[("svc", RunState::Running)]),
            Duration::from_secs(30),
        );
        let mut status = ServiceHealthStatus::unknown(
            "svc".to_string(),
            "rusty-photon-svc".to_string(),
            RunState::Running,
            None,
            Duration::from_secs(30),
        );
        status.health = ServiceHealth::Down;
        status.consecutive_failures = 2;
        state.set_service_health(status);
        assert_eq!(state.services.len(), 1, "replace, not append");
        assert_eq!(state.services[0].health, ServiceHealth::Down);
        assert_eq!(state.services[0].consecutive_failures, 2);

        // An unseeded name is inserted defensively, keeping name order.
        let other = ServiceHealthStatus::unknown(
            "another".to_string(),
            "rusty-photon-another".to_string(),
            RunState::Running,
            None,
            Duration::from_secs(5),
        );
        state.set_service_health(other);
        assert_eq!(state.services.len(), 2);
        assert_eq!(state.services[0].name, "another");
    }
}
