//! Configuration types for the PHD2 guider service

pub use rusty_photon_server_config::ServerConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// PHD2 service configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// HTTP server settings (`serve` mode only; the CLI ignores it) —
    /// the shared shape from `crates/rusty-photon-server-config`.
    /// `port: 0` auto-assigns — used by tests.
    #[serde(default = "default_server")]
    pub server: ServerConfig,
    /// How long `POST /api/v1/guiding/stop` waits for PHD2 to reach
    /// the `Stopped` state (`serve` mode only).
    #[serde(default = "default_stop_timeout", with = "humantime_serde")]
    pub stop_timeout: Duration,
    #[serde(default)]
    pub phd2: Phd2Config,
    #[serde(default)]
    pub settling: SettleParams,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: default_server(),
            stop_timeout: default_stop_timeout(),
            phd2: Phd2Config::default(),
            settling: SettleParams::default(),
        }
    }
}

/// phd2-guider's default `server` block when the config file omits it:
/// port 11130 on all interfaces, plain HTTP.
const fn default_server() -> ServerConfig {
    ServerConfig::new(11130)
}

const fn default_stop_timeout() -> Duration {
    Duration::from_secs(10)
}

/// PHD2 connection settings
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phd2Config {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_path: Option<PathBuf>,
    #[serde(default = "default_connection_timeout", with = "humantime_serde")]
    pub connection_timeout: Duration,
    #[serde(default = "default_command_timeout", with = "humantime_serde")]
    pub command_timeout: Duration,
    #[serde(default)]
    pub auto_start: bool,
    #[serde(default)]
    pub auto_connect_equipment: bool,
    #[serde(default)]
    pub reconnect: ReconnectConfig,
    /// Environment variables to set when spawning the PHD2 process
    #[serde(default)]
    pub spawn_env: std::collections::HashMap<String, String>,
}

/// Configuration for automatic reconnection
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconnectConfig {
    /// Enable automatic reconnection when connection is lost
    #[serde(default = "default_reconnect_enabled")]
    pub enabled: bool,
    /// Interval between reconnection attempts
    #[serde(default = "default_reconnect_interval", with = "humantime_serde")]
    pub interval: Duration,
    /// Maximum number of reconnection attempts (None for unlimited)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            enabled: default_reconnect_enabled(),
            interval: default_reconnect_interval(),
            max_retries: None,
        }
    }
}

const fn default_reconnect_enabled() -> bool {
    true
}

const fn default_reconnect_interval() -> Duration {
    Duration::from_secs(5)
}

impl Default for Phd2Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            executable_path: None,
            connection_timeout: default_connection_timeout(),
            command_timeout: default_command_timeout(),
            auto_start: false,
            auto_connect_equipment: false,
            reconnect: ReconnectConfig::default(),
            spawn_env: std::collections::HashMap::new(),
        }
    }
}

fn default_host() -> String {
    "localhost".to_string()
}

const fn default_port() -> u16 {
    4400
}

const fn default_connection_timeout() -> Duration {
    Duration::from_secs(10)
}

const fn default_command_timeout() -> Duration {
    Duration::from_secs(30)
}

/// Settling parameters for guiding operations.
///
/// This struct is the operator-facing config representation: durations are
/// `std::time::Duration` and use humantime strings on the wire (`"10s"`).
/// When sending settle parameters into PHD2's JSON-RPC payload, the call
/// sites in `client.rs` convert `time` and `timeout` to integer seconds via
/// `settle_secs_ceil`, because the PHD2 protocol requires integer values and
/// ceil-rounding avoids truncating sub-second durations down to `0`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettleParams {
    #[serde(default = "default_settle_pixels")]
    pub pixels: f64,
    #[serde(default = "default_settle_time", with = "humantime_serde")]
    pub time: Duration,
    #[serde(default = "default_settle_timeout", with = "humantime_serde")]
    pub timeout: Duration,
}

impl Default for SettleParams {
    fn default() -> Self {
        Self {
            pixels: default_settle_pixels(),
            time: default_settle_time(),
            timeout: default_settle_timeout(),
        }
    }
}

const fn default_settle_pixels() -> f64 {
    0.5
}

const fn default_settle_time() -> Duration {
    Duration::from_secs(10)
}

const fn default_settle_timeout() -> Duration {
    Duration::from_mins(1)
}

/// Load configuration from a JSON file
///
/// # Errors
///
/// Fails when the file cannot be read or its contents are not valid JSON
/// for [`Config`].
pub fn load_config(
    path: &Path,
) -> std::result::Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let content = std::fs::read_to_string(path)?;
    let config: Config = serde_json::from_str(&content)?;
    Ok(config)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn default_scaffold_round_trips_through_load() {
        // main() writes `Config::default()` to the platform path on the
        // packaged serve path's first start (resolve_and_init); that
        // serialized form must load back cleanly through the strict parse.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phd2-guider.json");
        let scaffold = serde_json::to_string_pretty(&Config::default()).unwrap();
        std::fs::write(&path, scaffold).unwrap();
        let c = load_config(&path).unwrap();
        assert_eq!(c.server.port, 11130);
        assert_eq!(c.phd2.host, "localhost");
        assert_eq!(c.phd2.port, 4400);
    }

    #[test]
    fn test_settle_params_default() {
        let params = SettleParams::default();
        assert_eq!(params.pixels, 0.5);
        assert_eq!(params.time, Duration::from_secs(10));
        assert_eq!(params.timeout, Duration::from_mins(1));
    }

    #[test]
    fn test_phd2_config_default() {
        let config = Phd2Config::default();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 4400);
        assert_eq!(config.connection_timeout, Duration::from_secs(10));
        assert_eq!(config.command_timeout, Duration::from_secs(30));
        assert!(!config.auto_start);
        assert!(!config.auto_connect_equipment);
        assert!(config.reconnect.enabled);
        assert_eq!(config.reconnect.interval, Duration::from_secs(5));
        assert!(config.reconnect.max_retries.is_none());
    }

    #[test]
    fn test_reconnect_config_default() {
        let config = ReconnectConfig::default();
        assert!(config.enabled);
        assert_eq!(config.interval, Duration::from_secs(5));
        assert!(config.max_retries.is_none());
    }

    #[test]
    fn test_reconnect_config_serialization() {
        let config = ReconnectConfig {
            enabled: true,
            interval: Duration::from_secs(10),
            max_retries: Some(5),
        };
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["enabled"], true);
        assert_eq!(json["interval"], "10s");
        assert_eq!(json["max_retries"], 5);
    }

    #[test]
    fn test_settle_params_serialization() {
        let params = SettleParams {
            pixels: 1.5,
            time: Duration::from_secs(15),
            timeout: Duration::from_mins(2),
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json["pixels"], 1.5);
        assert_eq!(json["time"], "15s");
        assert_eq!(json["timeout"], "2m");
    }

    #[test]
    fn an_empty_config_loads_with_the_serve_mode_defaults() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config.server.port, 11130);
        assert_eq!(config.server.bind_address.to_string(), "0.0.0.0");
        assert_eq!(config.stop_timeout, Duration::from_secs(10));
        assert_eq!(config.phd2.host, "localhost");
        assert_eq!(config.settling.pixels, 0.5);
    }

    #[test]
    fn the_serve_mode_fields_parse_from_json() {
        let config: Config = serde_json::from_str(
            r#"{
                "server": { "bind_address": "127.0.0.1", "port": 0 },
                "stop_timeout": "1s",
                "phd2": { "host": "127.0.0.1", "port": 14400 }
            }"#,
        )
        .unwrap();
        assert_eq!(config.server.bind_address.to_string(), "127.0.0.1");
        assert_eq!(config.server.port, 0);
        assert_eq!(config.stop_timeout, Duration::from_secs(1));
        assert_eq!(config.phd2.port, 14400);
    }

    #[test]
    fn a_misspelled_config_key_fails_at_config_load() {
        let err = serde_json::from_str::<Config>(r#"{"stop_timout": "5s"}"#).unwrap_err();
        assert!(err.to_string().contains("stop_timout"));
        let err = serde_json::from_str::<Config>(r#"{"settling": {"pixles": 1.0}}"#).unwrap_err();
        assert!(err.to_string().contains("pixles"));
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod doctor_toml_parity {
    use rusty_photon_server_config::doctor_toml::{parse, ServerClass};

    use super::Config;

    /// `pkg/doctor.toml` is this service's catalog entry for
    /// `rusty-photon-doctor` and must match the config defaults
    /// (docs/services/doctor.md §The derived catalog).
    #[test]
    fn pkg_doctor_toml_matches_config_defaults() {
        let meta = parse(include_str!("../pkg/doctor.toml")).unwrap();
        assert_eq!(meta.port, Config::default().server.port);
        assert_eq!(meta.class, ServerClass::Core);
    }
}

#[cfg(test)]
mod persisted_config_shape {
    use rusty_photon_server_config::unset::explicit_nulls;

    use super::Config;

    /// An unset optional field is spelled by its key's absence, never by an
    /// explicit `null` — see [`rusty_photon_server_config::unset`] for why.
    /// A field that grows without `skip_serializing_if` trips here rather
    /// than filling operators' config files with nulls.
    #[test]
    fn the_default_config_persists_no_explicit_nulls() {
        let persisted = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(
            explicit_nulls(&persisted),
            Vec::<String>::new(),
            "unset optional fields must be omitted, not written as null: {persisted}"
        );
    }
}
