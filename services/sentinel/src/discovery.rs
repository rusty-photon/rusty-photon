//! Service discovery: sentinel's registry of rusty-photon services comes from
//! the platform service manager, not from config.
//!
//! The [`ServiceManager`] trait is the seam: enumerate the installed
//! `rusty-photon-*` service units (with run states), restart a unit, and run
//! the platform's recovery check. Implementations: systemd (Linux), the
//! Windows service control manager, Homebrew services (macOS), and a
//! directory-backed test stub selected by the `SENTINEL_SERVICE_MANAGER_DIR`
//! environment variable. The rule (ADR-016 decision 8) is that the source of
//! truth must be alive when the supervised service is not — which the service
//! manager is, and a config map is not.
//!
//! Discovery also derives each running service's health probe URL from the
//! service's own `<svc>.json` (the shared `server` block: port + TLS), read
//! from the directory sentinel's own config file lives in. See
//! `docs/services/sentinel.md` §Service Discovery.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::corrective::run_shell;

/// Every rusty-photon service unit carries this prefix, on all three
/// platforms (systemd units, SCM service names, brew formulas).
///
/// It is also exactly the scope of the polkit rule the sentinel package
/// ships.
pub const UNIT_PREFIX: &str = "rusty-photon-";

/// Sentinel's own service name — excluded from discovery (it cannot
/// meaningfully supervise or restart itself).
const SELF_SERVICE: &str = "sentinel";

/// Scheduled-job units excluded from discovery: these are one-shot jobs,
/// not daemons — supervising one would restart-loop a failed run (a failed
/// 3am renewal would re-run against the same broken hook forever) and a
/// dashboard row for it would be noise. `renew` is the TLS renewal oneshot
/// the sentinel package ships (`rusty-photon-renew.service` + `.timer`).
const JOB_SERVICES: &[&str] = &["renew"];

/// The services that answer `GET /health` instead of the Alpaca
/// management API — exactly the non-Alpaca services.
///
/// Everything else discovered is an Alpaca driver probed at
/// `/management/v1/configureddevices`. A new non-Alpaca service must be
/// added here (a unit test asserts every listed name exists under
/// `services/*/pkg`).
pub const NON_ALPACA_SERVICES: &[&str] = &[
    "rp",
    "plate-solver",
    "session-runner",
    "calibrator-flats",
    "polar-align",
    "phd2-guider",
    "ui-htmx",
];

/// The environment variable selecting the directory-backed stub service
/// manager (a test seam — see `docs/services/sentinel.md` §The test seam).
pub const SERVICE_MANAGER_DIR_ENV: &str = "SENTINEL_SERVICE_MANAGER_DIR";

/// Recovery checks are quick platform queries (`systemctl is-active`); bound
/// each one like the HTTP probes so a wedged query cannot stall recovery.
/// Only the systemd and SCM backends run one — Homebrew has no `is-active`
/// equivalent, so macOS skips recovery confirmation and the bound with it.
#[cfg(any(target_os = "linux", windows))]
const RECOVERY_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// How a discovered unit is classified, deciding what supervision does with
/// it: only `running` and `failed` services are probed/restarted; the rest
/// are displayed and left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    /// Unit active (or activating): health-probed, restarted on hang.
    Running,
    /// The OS supervisor gave up (`Restart=on-failure` exhausted): restarted.
    Failed,
    /// Installed and enabled but a start condition is unmet (the
    /// `ConditionPathExists` config gate): deliberate, not broken.
    Inert,
    /// Inactive without a failed state — the operator stopped it.
    Stopped,
    /// Unit file disabled or masked.
    Disabled,
}

impl RunState {
    /// Whether autonomous supervision probes/restarts this state.
    #[must_use]
    pub const fn supervised(self) -> bool {
        matches!(self, Self::Running | Self::Failed)
    }
}

/// One enumerated unit, as reported by the platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredUnit {
    /// Full unit / service-manager name, e.g. `rusty-photon-dsd-fp2`.
    pub unit: String,
    pub state: RunState,
}

impl DiscoveredUnit {
    /// The service name — the unit minus the `rusty-photon-` prefix.
    #[must_use]
    pub fn service_name(&self) -> Option<&str> {
        self.unit
            .strip_prefix(UNIT_PREFIX)
            .filter(|s| !s.is_empty())
    }
}

/// A discovered service with everything derived for it: run state and — for
/// services whose `<svc>.json` was readable — the probe URLs.
#[derive(Debug, Clone)]
pub struct DiscoveredService {
    /// Service name (`dsd-fp2`) — the key of every API and registry lookup.
    pub name: String,
    /// Unit name (`rusty-photon-dsd-fp2`) — what the platform commands take.
    pub unit: String,
    pub state: RunState,
    /// `None` when the service's config could not be read (never started, so
    /// never self-created): health reports unknown, nothing probe-driven
    /// restarts, and derivation is retried every discovery cycle.
    pub probe: Option<ProbeSpec>,
}

/// The URLs derivable from a service's shared `server` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSpec {
    /// The health probe URL (`{base}/health` or
    /// `{base}/management/v1/configureddevices` by probe class).
    pub health_url: String,
    /// The Alpaca API base (`{scheme}://{host}:{port}/api/v1`) the watchdog
    /// ladder probes and aborts against.
    pub alpaca_base: String,
    /// The service's listening port (the `server.port` the URLs above embed).
    /// Exposed through `GET /api/services` so clients (ui-htmx) can match a
    /// device's `alpaca_url` to the service that serves it.
    pub port: u16,
}

/// The shared registry the discovery loop maintains and every consumer (the
/// restart endpoint, the watchdog ladder, the health supervisors) reads.
pub type ServiceRegistry = Arc<RwLock<HashMap<String, DiscoveredService>>>;

/// The service-manager seam: enumerate, restart, recovery-check.
#[async_trait]
pub trait ServiceManager: Send + Sync + fmt::Debug {
    /// Enumerate the installed `rusty-photon-*` service units with their run
    /// states. Sentinel's own unit is filtered out by the caller.
    async fn enumerate(&self) -> crate::Result<Vec<DiscoveredUnit>>;

    /// Restart `unit`, bounded by `budget`. `Ok` iff the platform command
    /// exits 0 in time.
    async fn restart(&self, unit: &str, budget: Duration) -> crate::Result<()>;

    /// One recovery check: `Some(true)` = the unit reports active,
    /// `Some(false)` = not (yet), `None` = the platform has no such check
    /// (Homebrew) and recovery confirmation is skipped.
    async fn recovery_check(&self, unit: &str) -> Option<bool>;
}

/// The production service manager for this platform, or the directory-backed
/// stub when [`SERVICE_MANAGER_DIR_ENV`] is set.
pub fn service_manager_from_env() -> Arc<dyn ServiceManager> {
    if let Some(dir) = std::env::var_os(SERVICE_MANAGER_DIR_ENV) {
        let dir = PathBuf::from(dir);
        warn!(
            "{SERVICE_MANAGER_DIR_ENV} is set: using the stub service manager at {} \
             (test seam — production installs must not set this)",
            dir.display()
        );
        return Arc::new(StubServiceManager::new(dir));
    }
    platform_service_manager()
}

#[cfg(target_os = "linux")]
fn platform_service_manager() -> Arc<dyn ServiceManager> {
    Arc::new(SystemdServiceManager)
}

#[cfg(windows)]
fn platform_service_manager() -> Arc<dyn ServiceManager> {
    Arc::new(ScmServiceManager)
}

#[cfg(target_os = "macos")]
fn platform_service_manager() -> Arc<dyn ServiceManager> {
    Arc::new(BrewServiceManager)
}

/// Supervision policy — the shipped defaults of the retired per-service
/// config, promoted to constants (plan D3s).
///
/// The stub service manager's optional `policy.json` can tighten them,
/// which exists so BDD scenarios don't need 90-second detection windows;
/// without the stub seam the values are compile-time fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisionPolicy {
    /// How often discovery re-enumerates the platform.
    pub discovery_interval: Duration,
    /// Health probe cadence.
    pub poll_interval: Duration,
    /// Consecutive failed probes before the first autonomous restart.
    pub failure_threshold: u32,
    /// Wait before a second restart attempt, doubling per attempt.
    pub restart_backoff: Duration,
    /// Ceiling for the doubling backoff.
    pub restart_backoff_max: Duration,
    /// Budget for a restart command *and* its recovery wait together.
    pub restart_budget: Duration,
}

impl Default for SupervisionPolicy {
    fn default() -> Self {
        Self {
            discovery_interval: Duration::from_mins(1),
            poll_interval: Duration::from_secs(30),
            failure_threshold: 3,
            restart_backoff: Duration::from_mins(1),
            restart_backoff_max: Duration::from_mins(15),
            restart_budget: Duration::from_mins(5),
        }
    }
}

/// The stub seam's optional timing overrides (`<dir>/policy.json`). Every
/// field is optional; absent fields keep the constant.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyOverrides {
    #[serde(default, with = "humantime_serde")]
    discovery_interval: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    poll_interval: Option<Duration>,
    #[serde(default)]
    failure_threshold: Option<u32>,
    #[serde(default, with = "humantime_serde")]
    restart_backoff: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    restart_backoff_max: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    restart_budget: Option<Duration>,
}

impl SupervisionPolicy {
    /// The constants — overridden only through the stub seam's
    /// `policy.json`, so production timing can never drift via config.
    pub fn resolve_from_env() -> Self {
        let mut policy = Self::default();
        let Some(dir) = std::env::var_os(SERVICE_MANAGER_DIR_ENV) else {
            return policy;
        };
        let path = Path::new(&dir).join("policy.json");
        let Ok(content) = std::fs::read_to_string(&path) else {
            return policy;
        };
        match serde_json::from_str::<PolicyOverrides>(&content) {
            Ok(overrides) => {
                if let Some(v) = overrides.discovery_interval {
                    policy.discovery_interval = v;
                }
                if let Some(v) = overrides.poll_interval {
                    policy.poll_interval = v;
                }
                if let Some(v) = overrides.failure_threshold {
                    policy.failure_threshold = v;
                }
                if let Some(v) = overrides.restart_backoff {
                    policy.restart_backoff = v;
                }
                if let Some(v) = overrides.restart_backoff_max {
                    policy.restart_backoff_max = v;
                }
                if let Some(v) = overrides.restart_budget {
                    policy.restart_budget = v;
                }
            }
            Err(e) => warn!("ignoring unparseable {}: {e}", path.display()),
        }
        policy
    }
}

// ---- probe derivation ---------------------------------------------------

/// The permissive cross-binary view of another service's `server` block.
/// Deliberately the opposite of the strict `deny_unknown_fields` parse a
/// service applies to its *own* config (ADR-016 decision 7): a read across
/// build generations must degrade, not refuse, so only what the probe needs
/// is read and everything else is stepped around.
#[derive(Debug, Deserialize)]
struct ServerView {
    port: u16,
    #[serde(default)]
    bind_address: Option<std::net::IpAddr>,
    #[serde(default)]
    tls: Option<serde_json::Value>,
}

/// Derive `service`'s probe URLs from `<config_dir>/<service>.json`.
///
/// `None` when the file is missing or its `server` block unreadable. A
/// `probe_domain` makes every URL dial `<service>.<probe_domain>`
/// instead of the bind-derived host.
pub fn derive_probe(
    config_dir: &Path,
    service: &str,
    probe_domain: Option<&str>,
) -> Option<ProbeSpec> {
    let path = config_dir.join(format!("{service}.json"));
    let content = std::fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            debug!("cannot parse {}: {e}", path.display());
            return None;
        }
    };
    let server: ServerView = match serde_json::from_value(value.get("server")?.clone()) {
        Ok(s) => s,
        Err(e) => {
            debug!("cannot read server block of {}: {e}", path.display());
            return None;
        }
    };
    if server.port == 0 {
        // Port 0 means OS-assigned at bind: the actual port is unknowable
        // from the config, and probing :0 would report a healthy service as
        // down forever.
        debug!(
            "{} binds an ephemeral port; probe not derivable",
            path.display()
        );
        return None;
    }
    let scheme = if server.tls.is_some() {
        "https"
    } else {
        "http"
    };
    // Same-host-bound by definition: a wildcard bind means localhost; a
    // specific bind address is honored (it may be loopback-only). A
    // configured probe domain overrides both — an ACME wildcard
    // certificate's SANs are DNS names only, so a probe dialing localhost
    // or an IP would fail hostname verification; `<service>.<domain>` is
    // the name the certificate can match, and it must resolve locally.
    let host = match probe_domain {
        Some(domain) => format!("{service}.{domain}"),
        None => match server.bind_address {
            Some(addr) if !addr.is_unspecified() => match addr {
                std::net::IpAddr::V4(v4) => v4.to_string(),
                std::net::IpAddr::V6(v6) => format!("[{v6}]"),
            },
            _ => "localhost".to_string(),
        },
    };
    let base = format!("{scheme}://{host}:{}", server.port);
    let path_suffix = if NON_ALPACA_SERVICES.contains(&service) {
        "/health"
    } else {
        "/management/v1/configureddevices"
    };
    Some(ProbeSpec {
        health_url: format!("{base}{path_suffix}"),
        alpaca_base: format!("{base}/api/v1"),
        port: server.port,
    })
}

/// One full discovery pass: enumerate, drop sentinel's own unit and foreign
/// names, derive probes for what remains.
///
/// # Errors
///
/// Returns the platform service manager's enumeration failure — the
/// only fallible step; a service whose probe cannot be derived is still
/// listed, just probe-less.
pub async fn discover(
    manager: &Arc<dyn ServiceManager>,
    config_dir: Option<&Path>,
    probe_domain: Option<&str>,
) -> crate::Result<HashMap<String, DiscoveredService>> {
    let units = manager.enumerate().await?;
    let mut services = HashMap::with_capacity(units.len());
    for unit in units {
        let Some(name) = unit.service_name() else {
            debug!("ignoring unit '{}' without the expected prefix", unit.unit);
            continue;
        };
        if name == SELF_SERVICE {
            continue;
        }
        if JOB_SERVICES.contains(&name) {
            debug!("ignoring scheduled-job unit '{}'", unit.unit);
            continue;
        }
        let probe = config_dir.and_then(|dir| derive_probe(dir, name, probe_domain));
        services.insert(
            name.to_string(),
            DiscoveredService {
                name: name.to_string(),
                unit: unit.unit.clone(),
                state: unit.state,
                probe,
            },
        );
    }
    Ok(services)
}

// ---- systemd (Linux) ----------------------------------------------------

/// systemd backend: `systemctl list-unit-files` + `systemctl show`.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct SystemdServiceManager;

#[cfg(target_os = "linux")]
#[async_trait]
impl ServiceManager for SystemdServiceManager {
    async fn enumerate(&self) -> crate::Result<Vec<DiscoveredUnit>> {
        let listing = shell_capture(
            "systemctl list-unit-files --type=service --no-legend --plain 'rusty-photon-*.service'",
        )
        .await?;
        let mut units = Vec::new();
        for line in listing.lines() {
            let mut cols = line.split_whitespace();
            let (Some(file), enablement) = (cols.next(), cols.next().unwrap_or("")) else {
                continue;
            };
            let unit = file.trim_end_matches(".service").to_string();
            if matches!(enablement, "disabled" | "masked" | "masked-runtime") {
                units.push(DiscoveredUnit {
                    unit,
                    state: RunState::Disabled,
                });
                continue;
            }
            let show = shell_capture(&format!(
                "systemctl show {unit}.service --property=ActiveState,ConditionResult"
            ))
            .await
            .unwrap_or_default();
            units.push(DiscoveredUnit {
                state: classify_systemd(&show),
                unit,
            });
        }
        Ok(units)
    }

    async fn restart(&self, unit: &str, budget: Duration) -> crate::Result<()> {
        run_shell(&format!("systemctl restart {unit}"), budget).await
    }

    async fn recovery_check(&self, unit: &str) -> Option<bool> {
        Some(
            run_shell(
                &format!("systemctl is-active --quiet {unit}"),
                RECOVERY_CHECK_TIMEOUT,
            )
            .await
            .is_ok(),
        )
    }
}

/// Classify a unit from `systemctl show`'s `Key=Value` lines.
#[cfg(target_os = "linux")]
fn classify_systemd(show_output: &str) -> RunState {
    let mut active_state = "";
    let mut condition_result = "";
    for line in show_output.lines() {
        if let Some(v) = line.strip_prefix("ActiveState=") {
            active_state = v.trim();
        } else if let Some(v) = line.strip_prefix("ConditionResult=") {
            condition_result = v.trim();
        }
    }
    match active_state {
        "active" | "activating" | "reloading" | "deactivating" => RunState::Running,
        "failed" => RunState::Failed,
        _ if condition_result == "no" => RunState::Inert,
        _ => RunState::Stopped,
    }
}

// ---- Windows SCM --------------------------------------------------------

/// Windows service control manager backend, via PowerShell's `Get-Service`.
///
/// The SCM has no failed-vs-stopped or condition-gate distinction: a stopped
/// automatic service reads as `stopped` (the installer's failure actions own
/// crash relaunch, so a "crashed and gave up" service is not auto-restarted
/// by sentinel on Windows).
#[cfg(windows)]
#[derive(Debug)]
pub struct ScmServiceManager;

#[cfg(windows)]
#[async_trait]
impl ServiceManager for ScmServiceManager {
    async fn enumerate(&self) -> crate::Result<Vec<DiscoveredUnit>> {
        let listing = shell_capture(
            "powershell -NoProfile -Command \"Get-Service -Name 'rusty-photon-*' \
             -ErrorAction SilentlyContinue | ForEach-Object { \
             \\\"$($_.Name) $($_.Status) $($_.StartType)\\\" }\"",
        )
        .await?;
        Ok(listing
            .lines()
            .filter_map(|line| {
                let mut cols = line.split_whitespace();
                let name = cols.next()?;
                let status = cols.next().unwrap_or("");
                let start_type = cols.next().unwrap_or("");
                let state = if start_type.eq_ignore_ascii_case("disabled") {
                    RunState::Disabled
                } else if matches!(status, "Running" | "StartPending" | "ContinuePending") {
                    RunState::Running
                } else {
                    RunState::Stopped
                };
                Some(DiscoveredUnit {
                    unit: name.to_string(),
                    state,
                })
            })
            .collect())
    }

    async fn restart(&self, unit: &str, budget: Duration) -> crate::Result<()> {
        run_shell(
            &format!("powershell -NoProfile -Command \"Restart-Service -Name '{unit}'\""),
            budget,
        )
        .await
    }

    async fn recovery_check(&self, unit: &str) -> Option<bool> {
        Some(
            run_shell(
                &format!("sc query \"{unit}\" | findstr RUNNING"),
                RECOVERY_CHECK_TIMEOUT,
            )
            .await
            .is_ok(),
        )
    }
}

// ---- Homebrew (macOS) ---------------------------------------------------

/// Homebrew services backend. `brew services list` reports
/// `started`/`error`/`stopped`/`none`; there is no `is-active` equivalent,
/// so recovery confirmation is skipped.
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub struct BrewServiceManager;

#[cfg(target_os = "macos")]
#[async_trait]
impl ServiceManager for BrewServiceManager {
    async fn enumerate(&self) -> crate::Result<Vec<DiscoveredUnit>> {
        let listing = shell_capture("brew services list").await?;
        Ok(listing
            .lines()
            .filter_map(|line| {
                let mut cols = line.split_whitespace();
                let name = cols.next()?;
                if !name.starts_with(UNIT_PREFIX) {
                    return None;
                }
                let state = match cols.next().unwrap_or("") {
                    "started" | "scheduled" => RunState::Running,
                    "error" => RunState::Failed,
                    _ => RunState::Stopped,
                };
                Some(DiscoveredUnit {
                    unit: name.to_string(),
                    state,
                })
            })
            .collect())
    }

    async fn restart(&self, unit: &str, budget: Duration) -> crate::Result<()> {
        run_shell(&format!("brew services restart {unit}"), budget).await
    }

    async fn recovery_check(&self, _unit: &str) -> Option<bool> {
        None
    }
}

// ---- the directory-backed test stub -------------------------------------

/// The test seam: a service manager backed by plain files, selected by
/// [`SERVICE_MANAGER_DIR_ENV`]. No shell, so it behaves identically on every
/// platform:
///
/// - `<dir>/units.txt` — one `<unit> <run-state>` pair per line;
/// - a restart appends the unit name to `<dir>/restarts.log` and (unless
///   `<dir>/stuck-<unit>` exists) rewrites the unit's `units.txt` state to
///   `running`; `<dir>/restart-fail-<unit>` makes it fail instead;
/// - the recovery check passes iff the unit's state is `running`.
#[derive(Debug)]
pub struct StubServiceManager {
    dir: PathBuf,
    /// Serializes every `units.txt` access: the per-service supervisors run
    /// concurrently, and an unlocked read-modify-write (or a read racing a
    /// truncate-and-rewrite) could lose a state flip or observe a
    /// half-written file. (Tests mutate the file from *outside* the sentinel
    /// process, where whole-file writes are effectively atomic enough for
    /// the scenarios' polling assertions.)
    state_lock: std::sync::Mutex<()>,
}

impl StubServiceManager {
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            state_lock: std::sync::Mutex::new(()),
        }
    }

    fn units_path(&self) -> PathBuf {
        self.dir.join("units.txt")
    }

    fn read_units(&self) -> Vec<DiscoveredUnit> {
        let _guard = self
            .state_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Ok(content) = std::fs::read_to_string(self.units_path()) else {
            return Vec::new();
        };
        content
            .lines()
            .filter_map(|line| {
                let mut cols = line.split_whitespace();
                let unit = cols.next()?;
                let state = match cols.next().unwrap_or("running") {
                    "running" => RunState::Running,
                    "failed" => RunState::Failed,
                    "inert" => RunState::Inert,
                    "stopped" => RunState::Stopped,
                    "disabled" => RunState::Disabled,
                    other => {
                        warn!("stub units.txt: unknown state '{other}' for '{unit}'");
                        return None;
                    }
                };
                Some(DiscoveredUnit {
                    unit: unit.to_string(),
                    state,
                })
            })
            .collect()
    }

    fn set_unit_state(&self, unit: &str, state: &str) {
        let _guard = self
            .state_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Ok(content) = std::fs::read_to_string(self.units_path()) else {
            return;
        };
        let rewritten: String = content
            .lines()
            .map(|line| {
                if line.split_whitespace().next() == Some(unit) {
                    format!("{unit} {state}\n")
                } else {
                    format!("{line}\n")
                }
            })
            .collect();
        let _ = std::fs::write(self.units_path(), rewritten);
    }
}

#[async_trait]
impl ServiceManager for StubServiceManager {
    async fn enumerate(&self) -> crate::Result<Vec<DiscoveredUnit>> {
        Ok(self.read_units())
    }

    async fn restart(&self, unit: &str, _budget: Duration) -> crate::Result<()> {
        use std::io::Write as _;

        if self.dir.join(format!("restart-fail-{unit}")).exists() {
            return Err(crate::SentinelError::Monitor(format!(
                "stub restart of `{unit}` scripted to fail"
            )));
        }
        // Appended, not read-modify-written: concurrent restarts of two
        // different units (two supervisors, or the ladder racing the REST
        // endpoint) must never clobber each other's log lines.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("restarts.log"))
            .and_then(|mut log| writeln!(log, "{unit}"))
            .map_err(|e| {
                crate::SentinelError::Monitor(format!("stub restarts.log write failed: {e}"))
            })?;
        if !self.dir.join(format!("stuck-{unit}")).exists() {
            self.set_unit_state(unit, "running");
        }
        Ok(())
    }

    async fn recovery_check(&self, unit: &str) -> Option<bool> {
        Some(
            self.read_units()
                .iter()
                .any(|u| u.unit == unit && u.state == RunState::Running),
        )
    }
}

// ---- shell capture helper ------------------------------------------------

/// Run a command through the platform shell and capture stdout. Enumeration
/// listings are small and the commands quick; a fixed bound keeps a wedged
/// platform tool from stalling the discovery loop.
#[allow(dead_code)] // unused on platforms whose backend needs no capture
async fn shell_capture(command: &str) -> crate::Result<String> {
    const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
    let output = tokio::time::timeout(
        CAPTURE_TIMEOUT,
        crate::corrective::shell_command(command).output(),
    )
    .await
    .map_err(|_| {
        crate::SentinelError::Monitor(format!("`{command}` exceeded {CAPTURE_TIMEOUT:?}"))
    })?
    .map_err(|e| crate::SentinelError::Monitor(format!("failed to run `{command}`: {e}")))?;
    if !output.status.success() {
        return Err(crate::SentinelError::Monitor(format!(
            "`{command}` exited with {}",
            output.status
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|e| crate::SentinelError::Monitor(format!("`{command}` output not UTF-8: {e}")))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn stub(dir: &tempfile::TempDir) -> StubServiceManager {
        StubServiceManager::new(dir.path().to_path_buf())
    }

    #[test]
    fn non_alpaca_services_exist_in_the_tree() {
        // The probe-class constant must track the services tree — a renamed
        // or removed service would silently flip probe classes otherwise.
        // (session-runner is deliberately unpackaged today; listing it here
        // is future-proofing, so the assertion is on the service dir, not
        // its pkg/.)
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("services/ dir");
        if !root.join("rp").is_dir() {
            // Bazel sandboxes only sentinel's own sources; the sweep runs
            // under cargo (local dev + the nightly safety net).
            eprintln!("skipping: sibling services not materialized");
            return;
        }
        for svc in NON_ALPACA_SERVICES {
            assert!(
                root.join(svc).is_dir(),
                "NON_ALPACA_SERVICES lists '{svc}' but services/{svc} does not exist"
            );
        }
    }

    #[test]
    fn service_name_strips_the_prefix() {
        let unit = DiscoveredUnit {
            unit: "rusty-photon-dsd-fp2".to_string(),
            state: RunState::Running,
        };
        assert_eq!(unit.service_name(), Some("dsd-fp2"));
        let foreign = DiscoveredUnit {
            unit: "nginx".to_string(),
            state: RunState::Running,
        };
        assert_eq!(foreign.service_name(), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn classify_systemd_covers_the_state_table() {
        assert_eq!(
            classify_systemd("ActiveState=active\nConditionResult=yes\n"),
            RunState::Running
        );
        assert_eq!(
            classify_systemd("ActiveState=activating\nConditionResult=yes\n"),
            RunState::Running
        );
        assert_eq!(
            classify_systemd("ActiveState=failed\nConditionResult=yes\n"),
            RunState::Failed
        );
        assert_eq!(
            classify_systemd("ActiveState=inactive\nConditionResult=no\n"),
            RunState::Inert
        );
        assert_eq!(
            classify_systemd("ActiveState=inactive\nConditionResult=yes\n"),
            RunState::Stopped
        );
        assert_eq!(classify_systemd(""), RunState::Stopped);
    }

    #[test]
    fn derive_probe_reads_port_and_class() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("plate-solver.json"),
            r#"{"server":{"port":11131},"astap":{"path":"/usr/bin/astap"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("dsd-fp2.json"),
            r#"{"server":{"port":11119,"discovery_port":null},"device":{}}"#,
        )
        .unwrap();
        let solver = derive_probe(dir.path(), "plate-solver", None).unwrap();
        assert_eq!(solver.health_url, "http://localhost:11131/health");
        assert_eq!(solver.alpaca_base, "http://localhost:11131/api/v1");
        assert_eq!(solver.port, 11131);
        let driver = derive_probe(dir.path(), "dsd-fp2", None).unwrap();
        assert_eq!(
            driver.health_url,
            "http://localhost:11119/management/v1/configureddevices"
        );
    }

    #[test]
    fn derive_probe_tls_means_https() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("rp.json"),
            r#"{"server":{"port":11115,"tls":{"cert":"c.pem","key":"k.pem"}}}"#,
        )
        .unwrap();
        let spec = derive_probe(dir.path(), "rp", None).unwrap();
        assert_eq!(spec.health_url, "https://localhost:11115/health");
    }

    #[test]
    fn derive_probe_explicit_null_tls_stays_http() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("rp.json"),
            r#"{"server":{"port":11115,"tls":null,"auth":null}}"#,
        )
        .unwrap();
        let spec = derive_probe(dir.path(), "rp", None).unwrap();
        assert_eq!(spec.health_url, "http://localhost:11115/health");
    }

    #[test]
    fn derive_probe_honors_a_specific_bind_address() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("rp.json"),
            r#"{"server":{"port":11115,"bind_address":"127.0.0.1"}}"#,
        )
        .unwrap();
        assert_eq!(
            derive_probe(dir.path(), "rp", None).unwrap().health_url,
            "http://127.0.0.1:11115/health"
        );
        std::fs::write(
            dir.path().join("ui-htmx.json"),
            r#"{"server":{"port":11120,"bind_address":"0.0.0.0"}}"#,
        )
        .unwrap();
        assert_eq!(
            derive_probe(dir.path(), "ui-htmx", None)
                .unwrap()
                .health_url,
            "http://localhost:11120/health",
            "a wildcard bind probes localhost"
        );
    }

    #[test]
    fn derive_probe_brackets_an_ipv6_bind_address() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("rp.json"),
            r#"{"server":{"port":11115,"bind_address":"::1"}}"#,
        )
        .unwrap();
        assert_eq!(
            derive_probe(dir.path(), "rp", None).unwrap().health_url,
            "http://[::1]:11115/health"
        );
    }

    #[test]
    fn derive_probe_domain_replaces_the_host_in_every_url() {
        // The override wins over the wildcard-bind default and over a
        // specific bind address alike, and lands in both derived URLs;
        // scheme and port derivation are untouched.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("rp.json"),
            r#"{"server":{"port":11115,"bind_address":"127.0.0.1","tls":{"cert":"c.pem","key":"k.pem"}}}"#,
        )
        .unwrap();
        let spec = derive_probe(dir.path(), "rp", Some("rig.example.com")).unwrap();
        assert_eq!(spec.health_url, "https://rp.rig.example.com:11115/health");
        assert_eq!(spec.alpaca_base, "https://rp.rig.example.com:11115/api/v1");
        std::fs::write(
            dir.path().join("dsd-fp2.json"),
            r#"{"server":{"port":11119}}"#,
        )
        .unwrap();
        assert_eq!(
            derive_probe(dir.path(), "dsd-fp2", Some("rig.example.com"))
                .unwrap()
                .health_url,
            "http://dsd-fp2.rig.example.com:11119/management/v1/configureddevices"
        );
    }

    #[test]
    fn derive_probe_missing_or_malformed_is_none() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(derive_probe(dir.path(), "absent", None).is_none());
        std::fs::write(dir.path().join("broken.json"), "{not json").unwrap();
        assert!(derive_probe(dir.path(), "broken", None).is_none());
        std::fs::write(dir.path().join("portless.json"), r#"{"server":{}}"#).unwrap();
        assert!(derive_probe(dir.path(), "portless", None).is_none());
        std::fs::write(dir.path().join("serverless.json"), r#"{"device":{}}"#).unwrap();
        assert!(derive_probe(dir.path(), "serverless", None).is_none());
    }

    #[test]
    fn derive_probe_ephemeral_port_is_none() {
        // Port 0 is OS-assigned at bind: the real port is unknowable from
        // the config, and probing :0 would report a healthy service as down.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("rp.json"), r#"{"server":{"port":0}}"#).unwrap();
        assert!(derive_probe(dir.path(), "rp", None).is_none());
    }

    #[test]
    fn derive_probe_tolerates_unknown_server_fields() {
        // A newer build's server block must still be readable (decision 7).
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("rp.json"),
            r#"{"server":{"port":11115,"future_field":{"x":1}}}"#,
        )
        .unwrap();
        assert!(derive_probe(dir.path(), "rp", None).is_some());
    }

    #[tokio::test]
    async fn stub_enumerates_units_txt() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("units.txt"),
            "rusty-photon-dsd-fp2 running\nrusty-photon-plate-solver inert\n",
        )
        .unwrap();
        let units = stub(&dir).enumerate().await.unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].unit, "rusty-photon-dsd-fp2");
        assert_eq!(units[0].state, RunState::Running);
        assert_eq!(units[1].state, RunState::Inert);
    }

    #[tokio::test]
    async fn stub_restart_logs_and_marks_running() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("units.txt"), "rusty-photon-x failed\n").unwrap();
        let manager = stub(&dir);
        manager
            .restart("rusty-photon-x", Duration::from_secs(1))
            .await
            .unwrap();
        let log = std::fs::read_to_string(dir.path().join("restarts.log")).unwrap();
        assert_eq!(log, "rusty-photon-x\n");
        assert_eq!(manager.recovery_check("rusty-photon-x").await, Some(true));
    }

    #[tokio::test]
    async fn stub_restart_fail_marker_fails_without_logging() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("units.txt"), "rusty-photon-x running\n").unwrap();
        std::fs::write(dir.path().join("restart-fail-rusty-photon-x"), "").unwrap();
        let err = stub(&dir)
            .restart("rusty-photon-x", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("scripted to fail"), "{err}");
        assert!(!dir.path().join("restarts.log").exists());
    }

    #[tokio::test]
    async fn stub_stuck_marker_keeps_the_prior_state() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("units.txt"), "rusty-photon-x failed\n").unwrap();
        std::fs::write(dir.path().join("stuck-rusty-photon-x"), "").unwrap();
        let manager = stub(&dir);
        manager
            .restart("rusty-photon-x", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            manager.recovery_check("rusty-photon-x").await,
            Some(false),
            "the stuck unit must not come back as running"
        );
    }

    #[tokio::test]
    async fn discover_excludes_self_and_derives_probes() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("units.txt"),
            "rusty-photon-sentinel running\nrusty-photon-plate-solver running\nnginx running\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("plate-solver.json"),
            r#"{"server":{"port":11131}}"#,
        )
        .unwrap();
        let manager: Arc<dyn ServiceManager> =
            Arc::new(StubServiceManager::new(dir.path().to_path_buf()));
        let services = discover(&manager, Some(dir.path()), None).await.unwrap();
        assert_eq!(services.len(), 1, "self and foreign units are excluded");
        let solver = &services["plate-solver"];
        assert_eq!(solver.unit, "rusty-photon-plate-solver");
        assert_eq!(
            solver.probe.as_ref().unwrap().health_url,
            "http://localhost:11131/health"
        );
    }

    #[test]
    fn policy_defaults_are_the_plan_constants() {
        let p = SupervisionPolicy::default();
        assert_eq!(p.discovery_interval, Duration::from_mins(1));
        assert_eq!(p.poll_interval, Duration::from_secs(30));
        assert_eq!(p.failure_threshold, 3);
        assert_eq!(p.restart_backoff, Duration::from_mins(1));
        assert_eq!(p.restart_backoff_max, Duration::from_mins(15));
        assert_eq!(p.restart_budget, Duration::from_mins(5));
    }
}
