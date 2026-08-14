//! Configuration types for the PPBA Driver

pub use rusty_photon_server_config::AlpacaServerConfig;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub serial: SerialConfig,
    pub server: AlpacaServerConfig,
    pub switch: SwitchConfig,
    pub observingconditions: ObservingConditionsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            serial: SerialConfig::default(),
            server: AlpacaServerConfig::new(11112),
            switch: SwitchConfig::default(),
            observingconditions: ObservingConditionsConfig::default(),
        }
    }
}

/// Serial port configuration
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SerialConfig {
    pub port: String,
    #[serde(default = "default_baud_rate")]
    pub baud_rate: u32,
    // `humantime_serde` stores the duration as a string; schemars describes it
    // as a string so the schema matches the wire form.
    #[serde(default = "default_polling_interval", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub polling_interval: Duration,
    #[serde(default = "default_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub timeout: Duration,
}

/// Switch device configuration
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SwitchConfig {
    pub name: String,
    /// ASCOM `UniqueID`. Minted as a `UUIDv4` on first run and persisted by
    /// `rusty_photon_config::materialize_identity`; never overwritten once set.
    /// Defaults to an empty string so a fresh config triggers materialization.
    #[serde(default)]
    pub unique_id: String,
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// `ObservingConditions` device configuration
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservingConditionsConfig {
    pub name: String,
    /// ASCOM `UniqueID`. Minted as a `UUIDv4` on first run and persisted by
    /// `rusty_photon_config::materialize_identity`; never overwritten once set.
    /// Defaults to an empty string so a fresh config triggers materialization.
    #[serde(default)]
    pub unique_id: String,
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_averaging_period", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub averaging_period: Duration,
}

const fn default_baud_rate() -> u32 {
    9600
}

const fn default_polling_interval() -> Duration {
    Duration::from_secs(5)
}

const fn default_timeout() -> Duration {
    Duration::from_secs(2)
}

const fn default_true() -> bool {
    true
}

const fn default_averaging_period() -> Duration {
    Duration::from_mins(5) // 5 minutes
}

/// Platform-dependent default serial port. Both values are placeholders the
/// operator replaces with the real device path: the driver restart-loops
/// until then, on Windows (`COM3`) exactly as on Unix (`/dev/ttyUSB0`).
#[cfg(windows)]
const DEFAULT_SERIAL_PORT: &str = "COM3";
#[cfg(not(windows))]
const DEFAULT_SERIAL_PORT: &str = "/dev/ttyUSB0";

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

impl Default for SwitchConfig {
    fn default() -> Self {
        Self {
            name: "Pegasus PPBA Switch".to_string(),
            // Empty by default: a real UUIDv4 is minted and persisted on first
            // run by `rusty_photon_config::materialize_identity`.
            unique_id: String::new(),
            description: "Pegasus Astro PPBA Gen2 Power Control".to_string(),
            enabled: true,
        }
    }
}

impl Default for ObservingConditionsConfig {
    fn default() -> Self {
        Self {
            name: "Pegasus PPBA Weather".to_string(),
            // Empty by default: a real UUIDv4 is minted and persisted on first
            // run by `rusty_photon_config::materialize_identity`.
            unique_id: String::new(),
            description: "Pegasus Astro PPBA Environmental Sensors".to_string(),
            enabled: true,
            averaging_period: default_averaging_period(),
        }
    }
}

/// Legacy `DeviceConfig` for backward compatibility during migration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    pub name: String,
    pub unique_id: String,
    pub description: String,
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
/// a command-line flag so the config actions can distinguish file vs. override
/// layers (see `docs/services/ppba-driver.md` "Config Actions").
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `--port` → `serial.port`.
    pub serial_port: Option<String>,
    /// `--server-port` → `server.port`.
    pub server_port: Option<u16>,
    /// `--enable-switch` → `switch.enabled`.
    pub enable_switch: Option<bool>,
    /// `--enable-observingconditions` → `observingconditions.enabled`.
    pub enable_observingconditions: Option<bool>,
}

impl CliOverrides {
    /// Dotted JSON paths currently pinned by an active override.
    #[must_use]
    pub fn pinned_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        if self.serial_port.is_some() {
            paths.push("serial.port".to_string());
        }
        if self.server_port.is_some() {
            paths.push("server.port".to_string());
        }
        if self.enable_switch.is_some() {
            paths.push("switch.enabled".to_string());
        }
        if self.enable_observingconditions.is_some() {
            paths.push("observingconditions.enabled".to_string());
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
        if let Some(enable) = self.enable_switch {
            config.switch.enabled = enable;
        }
        if let Some(enable) = self.enable_observingconditions {
            config.observingconditions.enabled = enable;
        }
    }
}

/// Load the effective config: the file at `path` if it exists, else
/// `Config::default()`, with CLI `overrides` applied on top. A present-but-
/// corrupt file is surfaced (naming the path) rather than silently reset.
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

        assert_eq!(config.switch.name, "Pegasus PPBA Switch");
        assert!(config.switch.enabled);

        assert_eq!(config.observingconditions.name, "Pegasus PPBA Weather");
        assert!(config.observingconditions.enabled);

        #[cfg(not(windows))]
        assert_eq!(config.serial.port, "/dev/ttyUSB0");
        #[cfg(windows)]
        assert_eq!(config.serial.port, "COM3");
        assert_eq!(config.serial.baud_rate, 9600);
        assert_eq!(config.serial.polling_interval, Duration::from_secs(5));
        assert_eq!(config.serial.timeout, Duration::from_secs(2));

        assert_eq!(config.server.port, 11112);
        assert_eq!(config.server.bind_address.to_string(), "0.0.0.0");
    }

    #[test]
    fn switch_config_default() {
        let config = SwitchConfig::default();

        assert_eq!(config.name, "Pegasus PPBA Switch");
        // Default is empty; the real UUIDv4 is minted at runtime by
        // `rusty_photon_config::materialize_identity`.
        assert_eq!(config.unique_id, "");
        assert!(!config.description.is_empty());
        assert!(config.enabled);
    }

    #[test]
    fn observingconditions_config_default() {
        let config = ObservingConditionsConfig::default();

        assert_eq!(config.name, "Pegasus PPBA Weather");
        // Default is empty; the real UUIDv4 is minted at runtime by
        // `rusty_photon_config::materialize_identity`.
        assert_eq!(config.unique_id, "");
        assert!(!config.description.is_empty());
        assert!(config.enabled);
        assert_eq!(config.averaging_period, Duration::from_mins(5)); // 5 minutes
    }

    #[test]
    fn serial_config_default() {
        let config = SerialConfig::default();

        #[cfg(not(windows))]
        assert_eq!(config.port, "/dev/ttyUSB0");
        #[cfg(windows)]
        assert_eq!(config.port, "COM3");
        assert_eq!(config.baud_rate, 9600);
        assert_eq!(config.polling_interval, Duration::from_secs(5));
        assert_eq!(config.timeout, Duration::from_secs(2));
    }

    #[test]
    fn config_serializes_to_json() {
        let config = Config::default();
        let json = serde_json::to_string(&config).unwrap();

        assert!(json.contains("Pegasus PPBA"));
        #[cfg(not(windows))]
        assert!(json.contains("/dev/ttyUSB0"));
        #[cfg(windows)]
        assert!(json.contains("COM3"));
        assert!(json.contains("9600"));
        assert!(json.contains("11112"));
    }

    #[test]
    fn config_deserializes_from_json() {
        let json = r#"{
            "serial": {
                "port": "/dev/ttyACM0",
                "baud_rate": 115200,
                "polling_interval": "10s",
                "timeout": "5s"
            },
            "server": {
                "port": 8080
            },
            "switch": {
                "name": "Test Switch",
                "unique_id": "test-switch-001",
                "description": "Test switch description",
                "enabled": true
            },
            "observingconditions": {
                "name": "Test Weather",
                "unique_id": "test-weather-001",
                "description": "Test weather description",
                "enabled": false,
                "averaging_period": "120s"
            }
        }"#;

        let config: Config = serde_json::from_str(json).unwrap();

        assert_eq!(config.switch.name, "Test Switch");
        assert_eq!(config.switch.unique_id, "test-switch-001");
        assert!(config.switch.enabled);

        assert_eq!(config.observingconditions.name, "Test Weather");
        assert!(!config.observingconditions.enabled);
        assert_eq!(
            config.observingconditions.averaging_period,
            Duration::from_mins(2)
        );

        assert_eq!(config.serial.port, "/dev/ttyACM0");
        assert_eq!(config.serial.baud_rate, 115_200);
        assert_eq!(config.serial.polling_interval, Duration::from_secs(10));
        assert_eq!(config.serial.timeout, Duration::from_secs(5));
        assert_eq!(config.server.port, 8080);
    }

    #[test]
    fn config_deserializes_with_defaults() {
        // Minimal JSON with only required fields
        let json = r#"{
            "serial": {
                "port": "/dev/ttyUSB1"
            },
            "server": {
                "port": 9000
            },
            "switch": {
                "name": "Minimal Switch",
                "unique_id": "min-switch-001",
                "description": "Minimal config"
            },
            "observingconditions": {
                "name": "Minimal Weather",
                "unique_id": "min-weather-001",
                "description": "Minimal weather config"
            }
        }"#;

        let config: Config = serde_json::from_str(json).unwrap();

        assert_eq!(config.switch.name, "Minimal Switch");
        assert_eq!(config.serial.port, "/dev/ttyUSB1");
        // These should have defaults
        assert_eq!(config.serial.baud_rate, 9600);
        assert_eq!(config.serial.polling_interval, Duration::from_secs(5));
        assert_eq!(config.serial.timeout, Duration::from_secs(2));
        assert!(config.switch.enabled);
        assert!(config.observingconditions.enabled);
        assert_eq!(
            config.observingconditions.averaging_period,
            Duration::from_mins(5)
        );
    }

    #[test]
    fn config_clone_works() {
        let config = Config::default();
        let cloned = config.clone();

        assert_eq!(config.switch.name, cloned.switch.name);
        assert_eq!(config.serial.port, cloned.serial.port);
        assert_eq!(config.server.port, cloned.server.port);
    }

    #[test]
    fn config_rejects_unknown_top_level_field() {
        let json = r#"{
            "serial": {"port": "/dev/ttyUSB0"},
            "server": {"port": 11112},
            "switch": {"name": "n", "description": "d"},
            "observingconditions": {"name": "n", "description": "d"},
            "plugins": []
        }"#;
        let err = serde_json::from_str::<Config>(json).unwrap_err();
        assert!(err.to_string().contains("plugins"), "{err}");
    }

    #[test]
    fn serial_config_rejects_unknown_field() {
        let json = r#"{"port": "/dev/ttyUSB0", "flow_control": "none"}"#;
        let err = serde_json::from_str::<SerialConfig>(json).unwrap_err();
        assert!(err.to_string().contains("flow_control"), "{err}");
    }

    #[test]
    fn switch_config_rejects_unknown_field() {
        let json = r#"{"name": "n", "description": "d", "channels": 4}"#;
        let err = serde_json::from_str::<SwitchConfig>(json).unwrap_err();
        assert!(err.to_string().contains("channels"), "{err}");
    }

    #[test]
    fn observingconditions_config_rejects_unknown_field() {
        let json = r#"{"name": "n", "description": "d", "sample_rate": "10s"}"#;
        let err = serde_json::from_str::<ObservingConditionsConfig>(json).unwrap_err();
        assert!(err.to_string().contains("sample_rate"), "{err}");
    }

    #[test]
    fn config_debug_works() {
        let config = Config::default();
        let debug_str = format!("{config:?}");

        assert!(debug_str.contains("Config"));
        assert!(debug_str.contains("SwitchConfig"));
        assert!(debug_str.contains("ObservingConditionsConfig"));
        assert!(debug_str.contains("SerialConfig"));
        assert!(debug_str.contains("AlpacaServerConfig"));
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

        // Hardware-measured USB identity; pins the file against edits.
        let usb = meta.usb.unwrap();
        assert_eq!(usb.vendor, "0403");
        assert_eq!(usb.product.as_deref(), Some("6015"));
        assert_eq!(usb.model.as_deref(), Some("PPBA"));
    }
}
