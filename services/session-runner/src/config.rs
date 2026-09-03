//! Service configuration, per `docs/services/session-runner.md`
//! § Configuration.
//!
//! Loaded via `rusty-photon-config` conventions; the file must exist
//! (there are no usable defaults for `workflows_dir` / `state_dir`).

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

pub use rusty_photon_server_config::ServerConfig;

use crate::error::{Result, SessionRunnerError};

/// The documented default listen port (the orchestrator-plugin range,
/// next to `calibrator-flats`' 11170).
pub const DEFAULT_PORT: u16 = 11171;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The HTTP server for `/runs`, `/validate`, `/health` (and the
    /// legacy `/invoke`).
    #[serde(default = "default_server")]
    pub server: ServerConfig,
    /// Directory of workflow documents; first-party documents ship in the
    /// package. Required.
    pub workflows_dir: PathBuf,
    /// Blackboard persistence directory. Required.
    pub state_dir: PathBuf,
    /// `rp`'s MCP endpoint: every `POST /runs` run and standalone
    /// `/validate` use it (a run cannot start without it); the legacy
    /// `/invoke` route uses the URL delivered in its payload.
    #[serde(default)]
    pub mcp_server_url: Option<String>,
    /// Explicit SSE endpoint override (Phase D); `null` derives
    /// `<mcp origin>/api/events/subscribe`.
    #[serde(default)]
    pub events_url: Option<String>,
    /// HTTP Basic credentials presented to `rp` — MCP calls, the event
    /// stream, and the completion POST alike. The D6 observatory
    /// credential; doctor `--fix` wires it (ADR-017).
    #[serde(default)]
    pub service_auth: Option<rp_mcp_client::ClientAuthConfig>,
    /// PEM CA path used to trust a TLS-enabled `rp` for the same
    /// connections. Per the ADR-017 policy, `service_auth` is only sent
    /// when this is set and the URL is https.
    #[serde(default)]
    pub ca_cert: Option<String>,
    /// How long a paused run keeps reconnecting after `rp` becomes
    /// unreachable before it fails (design § Safety Behavior). The
    /// startup self-resume waits unbounded.
    #[serde(default = "default_rp_outage_grace", with = "humantime_serde")]
    pub rp_outage_grace: Duration,
    /// `get_safety_status` cadence while a run waits for safe
    /// conditions; a `safety_changed` event ends the wait sooner.
    #[serde(default = "default_safety_poll_interval", with = "humantime_serde")]
    pub safety_poll_interval: Duration,
    /// Resume every run manifest in `state_dir` on startup (design
    /// § Runs → Self-resume on startup).
    #[serde(default = "default_true")]
    pub resume_on_start: bool,
}

const fn default_rp_outage_grace() -> Duration {
    Duration::from_mins(10)
}

const fn default_safety_poll_interval() -> Duration {
    Duration::from_secs(10)
}

const fn default_true() -> bool {
    true
}

impl Config {
    /// The client-side trust + credentials for every connection to `rp`,
    /// cloned out for the tasks that outlive the request handler.
    pub fn rp_connection(&self) -> RpConnection {
        RpConnection {
            service_auth: self.service_auth.clone(),
            ca_cert: self.ca_cert.as_ref().map(PathBuf::from),
        }
    }
}

/// The client-side credentials/trust for `rp` connections (MCP, the event
/// stream, the completion POST), derived from [`Config`].
#[derive(Clone, Debug, Default)]
pub struct RpConnection {
    pub service_auth: Option<rp_mcp_client::ClientAuthConfig>,
    pub ca_cert: Option<PathBuf>,
}

impl RpConnection {
    #[must_use]
    pub const fn auth(&self) -> Option<&rp_mcp_client::ClientAuthConfig> {
        self.service_auth.as_ref()
    }

    #[must_use]
    pub fn ca_path(&self) -> Option<&Path> {
        self.ca_cert.as_deref()
    }
}

/// session-runner's default `server` block when the config file omits it:
/// port 11171 on all interfaces, plain HTTP.
pub(crate) const fn default_server() -> ServerConfig {
    ServerConfig::new(DEFAULT_PORT)
}

/// CLI overrides layered over the file config after load: `--port` and
/// `--bind-address` pin `server.port` / `server.bind_address` over whatever
/// the file (or the `default_server()` fallback) supplied.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `--port` → `server.port`.
    pub port: Option<u16>,
    /// `--bind-address` → `server.bind_address`.
    pub bind_address: Option<IpAddr>,
}

impl CliOverrides {
    /// Apply the overrides onto `config` in place.
    pub const fn apply(&self, config: &mut Config) {
        if let Some(port) = self.port {
            config.server.port = port;
        }
        if let Some(bind_address) = self.bind_address {
            config.server.bind_address = bind_address;
        }
    }
}

/// Load and parse the configuration file. Unknown keys are rejected —
/// a misspelled field must not silently fall back to a default.
///
/// # Errors
///
/// Returns [`SessionRunnerError::Config`] naming the path if the file
/// cannot be read or does not parse (an unknown key included).
pub fn load_config(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| SessionRunnerError::Config(format!("cannot read {}: {e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| SessionRunnerError::Config(format!("cannot parse {}: {e}", path.display())))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn write_config(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("session-runner.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn test_minimal_config_gets_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            r#"{ "workflows_dir": "/var/lib/rp/workflows", "state_dir": "/var/lib/rp/state" }"#,
        );
        let config = load_config(&path).unwrap();
        assert_eq!(config.server.port, 11171);
        assert_eq!(config.server.bind_address.to_string(), "0.0.0.0");
        assert!(config.server.tls.is_none());
        assert!(config.server.auth.is_none());
        assert_eq!(config.workflows_dir, PathBuf::from("/var/lib/rp/workflows"));
        assert_eq!(config.state_dir, PathBuf::from("/var/lib/rp/state"));
        assert_eq!(config.mcp_server_url, None);
        assert_eq!(config.events_url, None);
        assert_eq!(config.rp_outage_grace, Duration::from_mins(10));
        assert_eq!(config.safety_poll_interval, Duration::from_secs(10));
        assert!(config.resume_on_start);
    }

    #[test]
    fn test_run_supervision_knobs_parse_as_humantime() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            r#"{ "workflows_dir": "w", "state_dir": "s", "rp_outage_grace": "90s",
                 "safety_poll_interval": "250ms", "resume_on_start": false }"#,
        );
        let config = load_config(&path).unwrap();
        assert_eq!(config.rp_outage_grace, Duration::from_secs(90));
        assert_eq!(config.safety_poll_interval, Duration::from_millis(250));
        assert!(!config.resume_on_start);
    }

    #[test]
    fn test_full_config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            r#"{ "server": { "port": 12000 }, "workflows_dir": "w", "state_dir": "s",
                 "mcp_server_url": "http://localhost:11115/mcp",
                 "events_url": "http://localhost:11115/api/events/subscribe" }"#,
        );
        let config = load_config(&path).unwrap();
        assert_eq!(config.server.port, 12000);
        assert_eq!(
            config.mcp_server_url.as_deref(),
            Some("http://localhost:11115/mcp")
        );
    }

    #[test]
    fn test_cli_overrides_pin_port_and_bind_address() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, r#"{ "workflows_dir": "w", "state_dir": "s" }"#);
        let mut config = load_config(&path).unwrap();
        let overrides = CliOverrides {
            port: Some(12345),
            bind_address: Some("127.0.0.1".parse().unwrap()),
        };
        overrides.apply(&mut config);
        assert_eq!(config.server.socket_addr().to_string(), "127.0.0.1:12345");
    }

    #[test]
    fn test_empty_cli_overrides_leave_the_file_config_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, r#"{ "workflows_dir": "w", "state_dir": "s" }"#);
        let mut config = load_config(&path).unwrap();
        CliOverrides::default().apply(&mut config);
        assert_eq!(config.server.socket_addr().to_string(), "0.0.0.0:11171");
    }

    #[test]
    fn test_missing_required_directory_fields_fail_loud() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, r#"{ "workflows_dir": "w" }"#);
        let err = load_config(&path).unwrap_err();
        assert!(err.to_string().contains("state_dir"), "{err}");
    }

    #[test]
    fn test_unknown_keys_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            r#"{ "workflows_dir": "w", "state_dir": "s", "workflow_dir": "typo" }"#,
        );
        let err = load_config(&path).unwrap_err();
        assert!(err.to_string().contains("workflow_dir"), "{err}");
    }

    #[test]
    fn test_missing_file_is_a_config_error() {
        let err = load_config(Path::new("/nonexistent/session-runner.json")).unwrap_err();
        assert!(matches!(err, SessionRunnerError::Config(_)), "{err}");
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod doctor_toml_parity {
    use rusty_photon_server_config::doctor_toml::{parse, ServerClass};

    use super::{default_server, DEFAULT_PORT};

    /// `pkg/doctor.toml` is this service's catalog entry for
    /// `rusty-photon-doctor` and must match the config defaults
    /// (docs/services/doctor.md §The derived catalog).
    #[test]
    fn pkg_doctor_toml_matches_config_defaults() {
        let meta = parse(include_str!("../pkg/doctor.toml")).unwrap();
        assert_eq!(meta.port, DEFAULT_PORT);
        assert_eq!(meta.port, default_server().port);
        assert_eq!(meta.class, ServerClass::Core);
        assert!(
            meta.config_gated,
            "session-runner has no sensible default config"
        );
    }
}
