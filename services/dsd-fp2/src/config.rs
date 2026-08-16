//! Configuration types for the Deep Sky Dad FP2 driver.

pub use rusty_photon_server_config::AlpacaServerConfig;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// Top-level service configuration.
///
/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub serial: SerialConfig,
    pub server: AlpacaServerConfig,
    pub cover_calibrator: CoverCalibratorConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            serial: SerialConfig::default(),
            server: AlpacaServerConfig::new(11119),
            cover_calibrator: CoverCalibratorConfig::default(),
        }
    }
}

/// Serial port configuration.
///
/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SerialConfig {
    pub port: String,
    #[serde(default = "default_baud_rate")]
    pub baud_rate: u32,
    // `humantime_serde` stores the duration as a string (e.g. "500ms"); tell
    // schemars to describe it as a string so the generated schema matches the
    // wire form rather than the `{secs, nanos}` auto-derive.
    #[serde(default = "default_polling_interval", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub polling_interval: Duration,
    #[serde(default = "default_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub timeout: Duration,
}

/// `CoverCalibrator` device configuration.
///
/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoverCalibratorConfig {
    pub name: String,
    /// Stable ASCOM `UniqueID`. Defaults to empty: a spec-compliant `UUIDv4` is
    /// minted on first run by `rusty_photon_config::materialize_identity` (see
    /// `main.rs`) and persisted to the config file, never overwritten.
    #[serde(default)]
    pub unique_id: String,
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_brightness")]
    pub max_brightness: u32,
    /// Floor below which `calibrator_on` rejects a non-zero brightness. The
    /// FP2's EL panel response is non-linear below this value (dead zone at
    /// 1-2, sub-linear ramp through roughly 3-100, still outside the ASCOM
    /// spec's linearity expectation up to ~250) — see
    /// `docs/services/dsd-fp2.md` "Hardware Constraints". The default was
    /// measured on one physical panel; a different unit's EL turn-on
    /// threshold may differ, hence this is configurable rather than a
    /// hardcoded constant. `0` is always accepted regardless (the ASCOM
    /// "on at zero" state).
    #[serde(default = "default_min_brightness")]
    pub min_brightness: u32,
}

const fn default_baud_rate() -> u32 {
    115_200
}

const fn default_polling_interval() -> Duration {
    Duration::from_millis(500)
}

const fn default_timeout() -> Duration {
    Duration::from_secs(3)
}

const fn default_true() -> bool {
    true
}

const fn default_max_brightness() -> u32 {
    4096
}

const fn default_min_brightness() -> u32 {
    250
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

impl Default for CoverCalibratorConfig {
    fn default() -> Self {
        Self {
            name: "Deep Sky Dad FP2".to_string(),
            // Minted on first run (UUIDv4) and persisted; see `main.rs`.
            unique_id: String::new(),
            description: "Deep Sky Dad Flat Panel 2 (motorised flat field panel)".to_string(),
            enabled: true,
            max_brightness: default_max_brightness(),
            min_brightness: default_min_brightness(),
        }
    }
}

/// Load configuration from a JSON file.
pub fn load_config(
    path: &Path,
) -> std::result::Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let content = std::fs::read_to_string(path)?;
    let config: Config = serde_json::from_str(&content)?;
    Ok(config)
}

/// CLI overrides layered over the file config. Tracks which fields are pinned by
/// a command-line flag so the config actions can distinguish the file layer from
/// the override layer (see `docs/services/dsd-fp2.md` "Config Actions").
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
            config.serial.port.clone_from(port);
        }
        if let Some(port) = self.server_port {
            config.server.port = port;
        }
    }
}

/// Load the effective config: the file at `path` if it exists, else
/// `Config::default()`, with CLI `overrides` applied on top. This is what the
/// running driver uses and what `config.get` reports.
pub fn load_effective_config(
    path: &Path,
    overrides: &CliOverrides,
) -> std::result::Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let mut config = match std::fs::read_to_string(path) {
        // Wrap both failure paths with the path (matching
        // `rusty_photon_config::read_file_value`) so a startup/reload failure
        // names the offending file instead of a bare "Permission denied" error.
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
    fn defaults_match_spec() {
        let c = Config::default();
        #[cfg(not(windows))]
        assert_eq!(c.serial.port, "/dev/ttyACM0");
        #[cfg(windows)]
        assert_eq!(c.serial.port, "COM3");
        assert_eq!(c.serial.baud_rate, 115_200);
        assert_eq!(c.serial.polling_interval, Duration::from_millis(500));
        assert_eq!(c.serial.timeout, Duration::from_secs(3));
        assert_eq!(c.server.port, 11119);
        assert_eq!(c.server.bind_address.to_string(), "0.0.0.0");
        assert!(c.cover_calibrator.enabled);
        assert_eq!(c.cover_calibrator.max_brightness, 4096);
        assert_eq!(c.cover_calibrator.min_brightness, 250);
        assert_eq!(c.cover_calibrator.name, "Deep Sky Dad FP2");
        // The id is no longer a hardcoded literal; it is minted as a UUIDv4 on
        // first run by `materialize_identity`, so the default is empty.
        assert_eq!(c.cover_calibrator.unique_id, "");
    }

    #[test]
    fn config_serialises_to_json() {
        let c = Config::default();
        let json = serde_json::to_string(&c).unwrap();
        #[cfg(not(windows))]
        assert!(json.contains("/dev/ttyACM0"));
        #[cfg(windows)]
        assert!(json.contains("COM3"));
        assert!(json.contains("115200"));
        assert!(json.contains("11119"));
        assert!(json.contains("Deep Sky Dad FP2"));
    }

    #[test]
    fn load_effective_config_corrupt_file_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dsd-fp2.json");
        std::fs::write(&path, "{ not valid json").unwrap();
        let err = load_effective_config(&path, &CliOverrides::default()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not valid JSON"), "{msg}");
        assert!(msg.contains(&path.display().to_string()), "{msg}");
    }

    #[test]
    fn config_deserialises_with_defaults() {
        let json = r#"{
            "serial": { "port": "/dev/ttyACM5" },
            "server": { "port": 9000 },
            "cover_calibrator": {
                "name": "FP2",
                "unique_id": "fp2-x",
                "description": "x"
            }
        }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.serial.port, "/dev/ttyACM5");
        assert_eq!(c.serial.baud_rate, 115_200);
        assert_eq!(c.serial.polling_interval, Duration::from_millis(500));
        assert_eq!(c.server.port, 9000);
        assert_eq!(c.cover_calibrator.max_brightness, 4096);
        assert_eq!(c.cover_calibrator.min_brightness, 250);
        assert!(c.cover_calibrator.enabled);
    }

    #[test]
    fn config_deserialises_full_override() {
        let json = r#"{
            "serial": {
                "port": "/dev/serial/by-id/usb-foo",
                "baud_rate": 9600,
                "polling_interval": "250ms",
                "timeout": "2s"
            },
            "server": {
                "port": 12345
            },
            "cover_calibrator": {
                "name": "Test FP",
                "unique_id": "tfp-1",
                "description": "test",
                "enabled": false,
                "max_brightness": 2048,
                "min_brightness": 200
            }
        }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.serial.port, "/dev/serial/by-id/usb-foo");
        assert_eq!(c.serial.baud_rate, 9600);
        assert_eq!(c.serial.polling_interval, Duration::from_millis(250));
        assert_eq!(c.serial.timeout, Duration::from_secs(2));
        assert_eq!(c.server.port, 12345);
        assert!(!c.cover_calibrator.enabled);
        assert_eq!(c.cover_calibrator.max_brightness, 2048);
        assert_eq!(c.cover_calibrator.min_brightness, 200);
    }

    #[test]
    fn load_config_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "serial": { "port": "/dev/ttyACM9" },
                "server": { "port": 8888 },
                "cover_calibrator": {
                    "name": "From File",
                    "unique_id": "ff-1",
                    "description": "loaded"
                }
            }"#,
        )
        .unwrap();
        let c = load_config(&path).unwrap();
        assert_eq!(c.serial.port, "/dev/ttyACM9");
        assert_eq!(c.server.port, 8888);
        assert_eq!(c.cover_calibrator.name, "From File");
    }

    #[test]
    fn load_config_missing_file_errors() {
        let path = std::path::PathBuf::from("/tmp/dsd_fp2_nonexistent_98765.json");
        load_config(&path).unwrap_err();
    }

    #[test]
    fn load_config_invalid_json_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        load_config(&path).unwrap_err();
    }

    #[test]
    fn a_typoed_config_field_is_rejected_loudly() {
        let err = serde_json::from_str::<Config>(r#"{"typoed_key": 1}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("typoed_key"), "{err}");
    }

    #[test]
    fn a_typoed_serial_field_is_rejected_loudly() {
        let err =
            serde_json::from_str::<SerialConfig>(r#"{"port": "/dev/ttyACM0", "baudrate": 9600}"#)
                .unwrap_err()
                .to_string();
        assert!(err.contains("baudrate"), "{err}");
    }

    #[test]
    fn a_typoed_cover_calibrator_field_is_rejected_loudly() {
        let err = serde_json::from_str::<CoverCalibratorConfig>(
            r#"{"name": "FP2", "description": "x", "max_brightnes": 4096}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("max_brightnes"), "{err}");
    }

    #[test]
    fn config_clone_and_debug_work() {
        let c = Config::default();
        let cloned = c.clone();
        assert_eq!(cloned.server.port, c.server.port);
        let dbg = format!("{c:?}");
        assert!(dbg.contains("Config"));
        assert!(dbg.contains("CoverCalibratorConfig"));
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
        assert_eq!(usb.vendor, "2e8a");
        assert_eq!(usb.product.as_deref(), Some("000a"));
        assert_eq!(usb.model.as_deref(), Some("FP2"));
    }
}
