//! Configuration types for the QHY Q-Focuser driver

pub use rusty_photon_server_config::AlpacaServerConfig;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// Main configuration structure
///
/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub serial: SerialConfig,
    pub server: AlpacaServerConfig,
    pub focuser: FocuserConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            serial: SerialConfig::default(),
            server: AlpacaServerConfig::new(11113),
            focuser: FocuserConfig::default(),
        }
    }
}

/// Serial port configuration
///
/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SerialConfig {
    pub port: String,
    #[serde(default = "default_baud_rate")]
    pub baud_rate: u32,
    // `humantime_serde` stores the duration as a string (e.g. "1s"); schemars
    // describes it as a string so the schema matches the wire form.
    #[serde(default = "default_polling_interval", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub polling_interval: Duration,
    #[serde(default = "default_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub timeout: Duration,
}

/// Focuser device configuration
///
/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FocuserConfig {
    pub name: String,
    /// ASCOM `UniqueID`. Minted as a `UUIDv4` on first run by
    /// `rusty_photon_config::materialize_identity` (JSON pointer
    /// `/focuser/unique_id`), persisted, and never overwritten. Defaults to an
    /// empty string so an absent or empty value triggers minting rather than
    /// reusing a hardcoded literal.
    #[serde(default)]
    pub unique_id: String,
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_step")]
    pub max_step: u32,
    #[serde(default)]
    pub speed: u8,
    #[serde(default)]
    pub reverse: bool,
}

const fn default_baud_rate() -> u32 {
    9600
}

const fn default_polling_interval() -> Duration {
    Duration::from_secs(1)
}

const fn default_timeout() -> Duration {
    Duration::from_secs(2)
}

const fn default_true() -> bool {
    true
}

const fn default_max_step() -> u32 {
    64_000
}

/// Platform-dependent default serial port. Both values are placeholders the
/// operator replaces with the real device path: the driver restart-loops
/// until then, on Windows (`COM3`) exactly as on Unix (`/dev/ttyACM0`).
#[cfg(windows)]
const DEFAULT_SERIAL_PORT: &str = "COM3";
#[cfg(not(windows))]
const DEFAULT_SERIAL_PORT: &str = "/dev/ttyACM0";

impl Default for SerialConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_SERIAL_PORT.to_string(),
            baud_rate: default_baud_rate(),
            polling_interval: default_polling_interval(),
            timeout: default_timeout(),
        }
    }
}

impl Default for FocuserConfig {
    fn default() -> Self {
        Self {
            name: "QHY Q-Focuser".to_string(),
            // Empty by default; minted as a UUIDv4 on first run and persisted by
            // `rusty_photon_config::materialize_identity`.
            unique_id: String::new(),
            description: "QHY Q-Focuser (EAF) Stepper Motor Controller".to_string(),
            enabled: true,
            max_step: default_max_step(),
            speed: 0,
            reverse: false,
        }
    }
}

/// Load configuration from a JSON file
pub fn load_config(
    path: &Path,
) -> std::result::Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let content = std::fs::read_to_string(path)?;
    let config: Config = serde_json::from_str(&content)?;
    Ok(config)
}

/// CLI overrides layered over the file config. Tracks which fields are pinned by
/// a command-line flag so the config actions can distinguish the file layer from
/// the override layer (see `docs/services/qhy-focuser.md` "Config Actions").
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `--port` → `serial.port`.
    pub serial_port: Option<String>,
    /// `--server-port` → `server.port`.
    pub server_port: Option<u16>,
}

impl CliOverrides {
    /// Dotted JSON paths currently pinned by an active override. Reported by
    /// `config.get` (`overrides[]`) and skipped by `config.apply`.
    #[must_use]
    pub fn pinned_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        if self.serial_port.is_some() {
            paths.push("serial.port".to_string());
        }
        if self.server_port.is_some() {
            paths.push("server.port".to_string());
        }
        paths
    }

    /// Apply the overrides onto `config` in place.
    pub fn apply(&self, config: &mut Config) {
        if let Some(port) = &self.serial_port {
            config.serial.port = port.clone();
        }
        if let Some(port) = self.server_port {
            config.server.port = port;
        }
    }
}

/// Load the effective config: the file at `path` if it exists, else
/// `Config::default()`, with CLI `overrides` applied on top. This is what the
/// running driver uses and what `config.get` reports. A present-but-corrupt file
/// is surfaced (naming the path) rather than silently reset.
pub fn load_effective_config(
    path: &Path,
    overrides: &CliOverrides,
) -> std::result::Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let mut config = match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content)
            .map_err(|e| format!("config file {} is not valid JSON: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(format!("could not read config file {}: {e}", path.display()).into()),
    };
    overrides.apply(&mut config);
    Ok(config)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_values() {
        let config = Config::default();

        assert_eq!(config.focuser.name, "QHY Q-Focuser");
        assert!(config.focuser.enabled);
        assert_eq!(config.focuser.max_step, 64_000);
        assert_eq!(config.focuser.speed, 0);
        assert!(!config.focuser.reverse);

        #[cfg(not(windows))]
        assert_eq!(config.serial.port, "/dev/ttyACM0");
        #[cfg(windows)]
        assert_eq!(config.serial.port, "COM3");
        assert_eq!(config.serial.baud_rate, 9600);
        assert_eq!(config.serial.polling_interval, Duration::from_secs(1));
        assert_eq!(config.serial.timeout, Duration::from_secs(2));

        assert_eq!(config.server.port, 11113);
        assert_eq!(config.server.bind_address.to_string(), "0.0.0.0");
    }

    #[test]
    fn focuser_config_default() {
        let config = FocuserConfig::default();

        assert_eq!(config.name, "QHY Q-Focuser");
        // `unique_id` defaults to empty so it is minted on first run rather than
        // reusing a hardcoded literal (see `materialize_identity` in main.rs).
        assert_eq!(config.unique_id, "");
        assert!(!config.description.is_empty());
        assert!(config.enabled);
        assert_eq!(config.max_step, 64_000);
        assert_eq!(config.speed, 0);
        assert!(!config.reverse);
    }

    #[test]
    fn serial_config_default() {
        let config = SerialConfig::default();

        #[cfg(not(windows))]
        assert_eq!(config.port, "/dev/ttyACM0");
        #[cfg(windows)]
        assert_eq!(config.port, "COM3");
        assert_eq!(config.baud_rate, 9600);
        assert_eq!(config.polling_interval, Duration::from_secs(1));
        assert_eq!(config.timeout, Duration::from_secs(2));
    }

    #[test]
    fn config_serializes_to_json() {
        let config = Config::default();
        let json = serde_json::to_string(&config).unwrap();

        assert!(json.contains("QHY Q-Focuser"));
        #[cfg(not(windows))]
        assert!(json.contains("/dev/ttyACM0"));
        #[cfg(windows)]
        assert!(json.contains("COM3"));
        assert!(json.contains("9600"));
        assert!(json.contains("11113"));
    }

    #[test]
    fn config_deserializes_from_json() {
        let json = r#"{
            "serial": {
                "port": "/dev/ttyACM0",
                "baud_rate": 115200,
                "polling_interval": "2s",
                "timeout": "5s"
            },
            "server": {
                "port": 8080
            },
            "focuser": {
                "name": "Test Focuser",
                "unique_id": "test-focuser-001",
                "description": "Test focuser description",
                "enabled": true,
                "max_step": 100000,
                "speed": 3,
                "reverse": true
            }
        }"#;

        let config: Config = serde_json::from_str(json).unwrap();

        assert_eq!(config.focuser.name, "Test Focuser");
        assert_eq!(config.focuser.unique_id, "test-focuser-001");
        assert!(config.focuser.enabled);
        assert_eq!(config.focuser.max_step, 100_000);
        assert_eq!(config.focuser.speed, 3);
        assert!(config.focuser.reverse);

        assert_eq!(config.serial.port, "/dev/ttyACM0");
        assert_eq!(config.serial.baud_rate, 115_200);
        assert_eq!(config.serial.polling_interval, Duration::from_secs(2));
        assert_eq!(config.serial.timeout, Duration::from_secs(5));
        assert_eq!(config.server.port, 8080);
    }

    #[test]
    fn config_deserializes_with_defaults() {
        let json = r#"{
            "serial": {
                "port": "/dev/ttyUSB1"
            },
            "server": {
                "port": 9000
            },
            "focuser": {
                "name": "Minimal Focuser",
                "unique_id": "min-focuser-001",
                "description": "Minimal config"
            }
        }"#;

        let config: Config = serde_json::from_str(json).unwrap();

        assert_eq!(config.focuser.name, "Minimal Focuser");
        assert_eq!(config.serial.port, "/dev/ttyUSB1");
        assert_eq!(config.serial.baud_rate, 9600);
        assert_eq!(config.serial.polling_interval, Duration::from_secs(1));
        assert_eq!(config.serial.timeout, Duration::from_secs(2));
        assert!(config.focuser.enabled);
        assert_eq!(config.focuser.max_step, 64_000);
        assert_eq!(config.focuser.speed, 0);
        assert!(!config.focuser.reverse);
    }

    #[test]
    fn config_deserializes_with_omitted_unique_id() {
        // A config that omits `unique_id` must still parse, defaulting the field
        // to empty so first-run minting fills it.
        let json = r#"{
            "serial": { "port": "/dev/ttyUSB1" },
            "server": { "port": 9000 },
            "focuser": {
                "name": "No-ID Focuser",
                "description": "unique_id omitted"
            }
        }"#;

        let config: Config = serde_json::from_str(json).unwrap();

        assert_eq!(config.focuser.name, "No-ID Focuser");
        assert_eq!(config.focuser.unique_id, "");
    }

    #[test]
    fn a_typoed_top_level_field_is_rejected_loudly() {
        let err = serde_json::from_str::<Config>(r#"{"serial_typo": 1}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("serial_typo"), "{err}");
    }

    #[test]
    fn a_typoed_serial_field_is_rejected_loudly() {
        let err = serde_json::from_str::<SerialConfig>(r#"{"port": "/dev/ttyACM0", "baud": 9600}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("baud"), "{err}");
    }

    #[test]
    fn a_typoed_focuser_field_is_rejected_loudly() {
        let err = serde_json::from_str::<FocuserConfig>(
            r#"{"name": "F", "description": "d", "max_stpe": 1000}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("max_stpe"), "{err}");
    }

    #[test]
    fn config_clone_works() {
        let config = Config::default();
        let cloned = config.clone();

        assert_eq!(config.focuser.name, cloned.focuser.name);
        assert_eq!(config.serial.port, cloned.serial.port);
        assert_eq!(config.server.port, cloned.server.port);
    }

    #[test]
    fn config_debug_works() {
        let config = Config::default();
        let debug_str = format!("{config:?}");

        assert!(debug_str.contains("Config"));
        assert!(debug_str.contains("FocuserConfig"));
        assert!(debug_str.contains("SerialConfig"));
        assert!(debug_str.contains("AlpacaServerConfig"));
    }

    #[test]
    fn load_config_from_file() {
        let dir = std::env::temp_dir().join("qhy_focuser_test_load_config");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let json = r#"{
            "serial": { "port": "/dev/ttyUSB0", "baud_rate": 115200 },
            "server": { "port": 9999 },
            "focuser": {
                "name": "Test Focuser",
                "unique_id": "test-001",
                "description": "A test focuser",
                "speed": 7
            }
        }"#;
        std::fs::write(&path, json).unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.serial.port, "/dev/ttyUSB0");
        assert_eq!(config.serial.baud_rate, 115_200);
        assert_eq!(config.server.port, 9999);
        assert_eq!(config.focuser.name, "Test Focuser");
        assert_eq!(config.focuser.speed, 7);

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_config_nonexistent_file() {
        let path = std::path::PathBuf::from("/tmp/qhy_focuser_nonexistent_config_12345.json");
        let result = load_config(&path);
        assert!(result.is_err());
    }

    #[test]
    fn load_config_invalid_json() {
        let dir = std::env::temp_dir().join("qhy_focuser_test_invalid_json");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad_config.json");

        std::fs::write(&path, "this is not valid json").unwrap();

        let result = load_config(&path);
        assert!(result.is_err());

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
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
        assert_eq!(meta.class, ServerClass::Alpaca);

        // The declared serial pointer must resolve, in the serialized
        // default config, to the declared platform default.
        let serial = meta.serial.unwrap();
        let value = serde_json::to_value(Config::default()).unwrap();
        let port = value.pointer(&serial.pointer).unwrap().as_str().unwrap();
        #[cfg(unix)]
        assert_eq!(port, serial.default_unix);
        #[cfg(windows)]
        assert_eq!(port, serial.default_windows);
        assert_eq!(serial.gate, None);

        // No USB identity declared yet (not measured on hardware).
        assert!(meta.usb.is_none());
    }
}
