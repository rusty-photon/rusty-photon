//! The service config (docs/services/calibrator-flats.md § Configuration).
//!
//! The HTTP `server` block, the `rp` client fields, the exposure-search
//! tunables and the store path. There is no flat plan in here any more —
//! the train is a tool argument, the filters come from the wheel and the
//! target fraction is fixed — and the keys that used to carry one are
//! refused by name at load rather than silently ignored.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

pub use rusty_photon_server_config::ServerConfig;

use crate::error::{CalibratorFlatsError, Result};

/// The file name of the redb store under the state directory.
pub const STORE_FILE_NAME: &str = "calibrator-flats.redb";

/// The orchestrator-era flat-plan keys (calibrator-flats-provider plan,
/// D2). Present in a config file, each fails the load naming itself and
/// where the plan went.
pub const RETIRED_KEYS: &[&str] = &[
    "camera_id",
    "filter_wheel_id",
    "calibrator_id",
    "filters",
    "brightness",
    "target_adu_fraction",
];

/// The service configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The HTTP server for `/mcp` and `/health`. Files without a
    /// `server` block keep loading via the default.
    #[serde(default = "default_server")]
    pub server: ServerConfig,
    /// `rp`'s MCP endpoint, dialed per tool call. Required: it has no
    /// sensible default.
    pub mcp_server_url: String,
    /// HTTP Basic credentials presented to `rp` on MCP calls. The D6
    /// observatory credential; doctor `--fix` wires it (ADR-017).
    #[serde(default)]
    pub service_auth: Option<rp_mcp_client::ClientAuthConfig>,
    /// PEM CA path used to trust a TLS-enabled `rp`. Per the ADR-017
    /// policy, `service_auth` is only sent when this is set and the URL
    /// is https.
    #[serde(default)]
    pub ca_cert: Option<String>,
    /// Convergence: acceptable deviation of the measured median from
    /// the 50 % target (default 0.05 = 5 %).
    #[serde(default = "default_tolerance")]
    pub tolerance: f64,
    /// Search exposures per pass, per brightness level.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Where a filter's search starts (humantime, e.g. `"1s"`).
    #[serde(default = "default_initial_duration", with = "humantime_serde")]
    pub initial_duration: Duration,
    /// The search floor (plan D8): the effective floor is the larger of
    /// this and the camera's `exposure_min`. A step that wants less
    /// counts as over-bright and dims the panel instead.
    #[serde(default = "default_min_exposure", with = "humantime_serde")]
    pub min_exposure: Duration,
    /// `take_flats`' verification band around the 50 % target (plan D7,
    /// default 0.10 = 10 %).
    #[serde(default = "default_flat_warn_tolerance")]
    pub flat_warn_tolerance: f64,
    /// Override for the redb store file; `None` resolves to the platform
    /// state directory ([`Config::store_path`]).
    #[serde(default)]
    pub store_path: Option<PathBuf>,
}

impl Config {
    #[must_use]
    pub const fn rp_auth(&self) -> Option<&rp_mcp_client::ClientAuthConfig> {
        self.service_auth.as_ref()
    }

    pub fn rp_ca(&self) -> Option<&Path> {
        self.ca_cert.as_deref().map(Path::new)
    }

    /// Where the flat-timing store lives: `store_path` when set,
    /// otherwise `calibrator-flats.redb` in the platform state directory
    /// (docs/services/calibrator-flats.md § Store).
    ///
    /// # Errors
    ///
    /// Returns [`CalibratorFlatsError::Config`] if the platform state
    /// directory cannot be resolved (no `store_path`, and no platform
    /// config directory on macOS / Windows).
    pub fn store_path(&self) -> Result<PathBuf> {
        match &self.store_path {
            Some(path) => Ok(path.clone()),
            None => Ok(default_state_dir()?.join(STORE_FILE_NAME)),
        }
    }
}

/// The Linux state directory, provisioned and owned by the packaged
/// unit's systemd `StateDirectory=`.
#[cfg(not(any(windows, target_os = "macos")))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the macOS / Windows variant can fail to resolve the platform directory; one signature for both"
)]
fn default_state_dir() -> Result<PathBuf> {
    Ok(PathBuf::from("/var/lib/rusty-photon/calibrator-flats"))
}

/// macOS and Windows keep state beside the config, as `rp` does:
/// `~/Library/Application Support/rusty-photon/calibrator-flats/` and
/// `%PROGRAMDATA%\rusty-photon\calibrator-flats\`.
#[cfg(any(windows, target_os = "macos"))]
fn default_state_dir() -> Result<PathBuf> {
    rusty_photon_config::default_config_dir()
        .map(|dir| dir.join("calibrator-flats"))
        .map_err(|e| {
            CalibratorFlatsError::Config(format!(
                "cannot resolve the platform state directory for the store: {e}"
            ))
        })
}

/// calibrator-flats' default `server` block when the file omits it:
/// port 11170 on all interfaces, plain HTTP.
pub(crate) const fn default_server() -> ServerConfig {
    ServerConfig::new(11170)
}

/// CLI overrides layered over the file config after load: `--port` and
/// `--bind-address` pin `server.port` / `server.bind_address` over
/// whatever the file (or the `default_server()` fallback) supplied.
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

const fn default_tolerance() -> f64 {
    0.05
}

const fn default_max_iterations() -> u32 {
    10
}

const fn default_initial_duration() -> Duration {
    Duration::from_secs(1)
}

const fn default_min_exposure() -> Duration {
    Duration::from_millis(250)
}

const fn default_flat_warn_tolerance() -> f64 {
    0.10
}

/// Parse a config document. A retired flat-plan key is refused by name
/// before the typed parse, so the operator reads where the plan went
/// rather than a generic unknown-field error.
///
/// # Errors
///
/// Returns [`CalibratorFlatsError::Config`] if `contents` is not JSON,
/// names a retired key, or does not parse as a [`Config`]. `origin`
/// names the source in the message.
pub fn parse_config(contents: &str, origin: &str) -> Result<Config> {
    let value: serde_json::Value = serde_json::from_str(contents).map_err(|e| {
        CalibratorFlatsError::Config(format!("failed to parse config file '{origin}': {e}"))
    })?;
    if let Some(object) = value.as_object() {
        if let Some(key) = RETIRED_KEYS.iter().find(|key| object.contains_key(**key)) {
            return Err(CalibratorFlatsError::Config(format!(
                "config file '{origin}': the key `{key}` was retired — calibrator-flats is a tool \
                 provider and carries no flat plan; the train is the `train_id` argument of \
                 `train_flats` / `take_flats`, the filters come from the train's wheel and the \
                 target fraction is fixed at 50 % (docs/services/calibrator-flats.md § \
                 Configuration). Remove it"
            )));
        }
    }
    serde_json::from_value(value).map_err(|e| {
        CalibratorFlatsError::Config(format!("failed to parse config file '{origin}': {e}"))
    })
}

/// Load a [`Config`] from the JSON file at `path`.
///
/// # Errors
///
/// Returns [`CalibratorFlatsError::Config`] if the file cannot be read,
/// names a retired key, or does not parse as a [`Config`].
pub fn load_config(path: &Path) -> Result<Config> {
    let contents = std::fs::read_to_string(path).map_err(|e| {
        CalibratorFlatsError::Config(format!(
            "failed to read config file '{}': {}",
            path.display(),
            e
        ))
    })?;
    parse_config(&contents, &path.display().to_string())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"{ "mcp_server_url": "http://localhost:11115/mcp" }"#;

    #[test]
    fn a_minimal_config_takes_the_documented_defaults() {
        let config = parse_config(MINIMAL, "test").unwrap();
        assert_eq!(config.mcp_server_url, "http://localhost:11115/mcp");
        assert_eq!(config.tolerance, 0.05);
        assert_eq!(config.max_iterations, 10);
        assert_eq!(config.initial_duration, Duration::from_secs(1));
        assert_eq!(config.min_exposure, Duration::from_millis(250));
        assert_eq!(config.flat_warn_tolerance, 0.10);
        assert!(config.store_path.is_none());
        assert!(config.service_auth.is_none());
        assert!(config.ca_cert.is_none());
        // A file without a `server` block keeps loading via the default.
        assert_eq!(config.server.port, 11170);
        assert_eq!(config.server.bind_address.to_string(), "0.0.0.0");
        assert!(config.server.tls.is_none());
        assert!(config.server.auth.is_none());
    }

    #[test]
    fn mcp_server_url_is_required() {
        let err = parse_config("{}", "test").unwrap_err();
        assert!(err.to_string().contains("mcp_server_url"), "{err}");
    }

    #[test]
    fn overrides_parse() {
        let json = r#"{
            "server": { "port": 12000, "bind_address": "127.0.0.1" },
            "mcp_server_url": "https://rig:11115/mcp",
            "service_auth": { "username": "observatory", "password": "s3cret" },
            "ca_cert": "/etc/rusty-photon/pki/ca.pem",
            "tolerance": 0.1,
            "max_iterations": 5,
            "initial_duration": "500ms",
            "min_exposure": "1s",
            "flat_warn_tolerance": 0.2,
            "store_path": "/data/flats.redb"
        }"#;
        let config = parse_config(json, "test").unwrap();
        assert_eq!(config.server.socket_addr().to_string(), "127.0.0.1:12000");
        assert_eq!(config.rp_auth().unwrap().username, "observatory");
        assert_eq!(
            config.rp_ca(),
            Some(Path::new("/etc/rusty-photon/pki/ca.pem"))
        );
        assert_eq!(config.tolerance, 0.1);
        assert_eq!(config.max_iterations, 5);
        assert_eq!(config.initial_duration, Duration::from_millis(500));
        assert_eq!(config.min_exposure, Duration::from_secs(1));
        assert_eq!(config.flat_warn_tolerance, 0.2);
        assert_eq!(
            config.store_path().unwrap(),
            PathBuf::from("/data/flats.redb")
        );
    }

    #[test]
    fn the_default_store_path_ends_in_the_store_file_under_the_service_dir() {
        let config = parse_config(MINIMAL, "test").unwrap();
        let path = config.store_path().unwrap();
        assert_eq!(path.file_name().unwrap(), STORE_FILE_NAME);
        assert_eq!(
            path.parent().unwrap().file_name().unwrap(),
            "calibrator-flats",
            "{}",
            path.display()
        );
    }

    #[test]
    fn every_retired_key_is_refused_by_name() {
        for key in RETIRED_KEYS {
            let json = format!(r#"{{ "mcp_server_url": "http://x/mcp", "{key}": 1 }}"#);
            let err = parse_config(&json, "plan.json").unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains(&format!("`{key}` was retired")),
                "{message}"
            );
            assert!(message.contains("train_flats"), "{message}");
            assert!(message.contains("plan.json"), "{message}");
        }
    }

    #[test]
    fn an_unknown_key_is_rejected_naming_it() {
        let json = r#"{ "mcp_server_url": "http://x/mcp", "dither_pixels": 5.0 }"#;
        let err = parse_config(json, "test").unwrap_err();
        assert!(err.to_string().contains("dither_pixels"), "{err}");
    }

    #[test]
    fn cli_overrides_pin_port_and_bind_address() {
        let mut config = parse_config(MINIMAL, "test").unwrap();
        let overrides = CliOverrides {
            port: Some(12345),
            bind_address: Some("127.0.0.1".parse().unwrap()),
        };
        overrides.apply(&mut config);
        assert_eq!(config.server.socket_addr().to_string(), "127.0.0.1:12345");
    }

    #[test]
    fn empty_cli_overrides_leave_the_config_untouched() {
        let mut config = parse_config(MINIMAL, "test").unwrap();
        CliOverrides::default().apply(&mut config);
        assert_eq!(config.server.socket_addr().to_string(), "0.0.0.0:11170");
    }

    #[test]
    fn load_config_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calibrator-flats.json");
        std::fs::write(&path, MINIMAL).unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(config.mcp_server_url, "http://localhost:11115/mcp");
    }

    #[test]
    fn load_config_missing_file() {
        let err = load_config(Path::new("/nonexistent/calibrator-flats/config.json")).unwrap_err();
        assert!(err.to_string().contains("failed to read config file"));
    }

    #[test]
    fn load_config_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "not valid json").unwrap();
        let err = load_config(&path).unwrap_err();
        assert!(err.to_string().contains("failed to parse config file"));
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod doctor_toml_parity {
    use rusty_photon_server_config::doctor_toml::{parse, ServerClass};

    use super::default_server;

    /// `pkg/doctor.toml` is this service's catalog entry for
    /// `rusty-photon-doctor` and must match the config defaults
    /// (docs/services/doctor.md §The derived catalog).
    #[test]
    fn pkg_doctor_toml_matches_config_defaults() {
        let meta = parse(include_str!("../pkg/doctor.toml")).unwrap();
        assert_eq!(meta.port, default_server().port);
        assert_eq!(meta.class, ServerClass::Core);
        assert!(
            meta.config_gated,
            "calibrator-flats has no sensible default config"
        );
    }
}
