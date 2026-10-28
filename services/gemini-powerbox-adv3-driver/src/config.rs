//! Configuration types for the gemini-powerbox-adv3-driver.
//!
//! The shape follows `ppba-driver`'s with two differences the design doc
//! names: there is no `serial.baud_rate` — the hub speaks 19200 only, and
//! `deny_unknown_fields` makes the key a load error if someone adds it —
//! and the defaults name the Gemini box.

use rusty_photon_config::driver::FieldError;
use rusty_photon_rolling_stats::observing_conditions::MAX_AVERAGING_PERIOD;
pub use rusty_photon_server_config::AlpacaServerConfig;
use serde::{Deserialize, Serialize};

use crate::switches::GeminiSwitchLabels;
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

impl rusty_photon_config::ConfigFile for Config {
    /// The loader's own rules ([`loader_rules`]), which the setup page's
    /// `validate` builds on: a hand-edited file that breaks one is refused
    /// here, naming the field, rather than starting a service whose poll
    /// task dies on a zero interval or whose `AveragePeriod` reads back
    /// outside ASCOM's range. The port and the device ids are the start
    /// path's — `--port` may supply the port and the bootstrap mints the
    /// ids — so neither is checked at load, and `doctor` on a shipped
    /// example config with empty ids passes before the first start.
    fn check(&self) -> std::result::Result<(), String> {
        let errors = loader_rules(self);
        if errors.is_empty() {
            return Ok(());
        }
        Err(errors
            .iter()
            .map(|e| format!("{}: {}", e.path, e.msg))
            .collect::<Vec<_>>()
            .join("; "))
    }
}

/// The rules a config must meet to be loaded at all.
///
/// As [`FieldError`]s so the setup page can render them beside their
/// fields: a poll interval and
/// a timeout above zero (`tokio::time::interval` panics on a zero period),
/// and an averaging period no longer than ASCOM's `AveragePeriod` ceiling.
/// The setup page's `validate` adds the rules the start path fills in for
/// a loaded file: a non-empty port and the device ids.
#[must_use]
pub fn loader_rules(config: &Config) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if config.serial.polling_interval.is_zero() {
        errors.push(FieldError {
            path: "serial.polling_interval".to_string(),
            msg: "must be greater than 0".to_string(),
        });
    }
    if config.serial.timeout.is_zero() {
        errors.push(FieldError {
            path: "serial.timeout".to_string(),
            msg: "must be greater than 0".to_string(),
        });
    }
    // Deliberately no lower bound. Zero is ASCOM's "the device is not
    // averaging — give me the most recent value", which
    // `set_average_period(0.0)` accepts over the wire and
    // `GeminiManager::new` maps to the instantaneous window. Rejecting it
    // here would leave a client able to select a period at runtime that it
    // could never persist as the startup default.
    if config.observingconditions.averaging_period > MAX_AVERAGING_PERIOD {
        errors.push(FieldError {
            path: "observingconditions.averaging_period".to_string(),
            msg: "must not exceed 24h, the ASCOM AveragePeriod ceiling".to_string(),
        });
    }
    errors
}

/// Where each device's ASCOM `UniqueID` lives in the config file: the JSON
/// pointers the startup bootstrap mints into.
pub const IDENTITY_POINTERS: &[&str] = &["/switch/unique_id", "/observingconditions/unique_id"];

/// The server port the design doc assigns this service.
pub const DEFAULT_SERVER_PORT: u16 = 11132;

impl Default for Config {
    fn default() -> Self {
        Self {
            serial: SerialConfig::default(),
            server: AlpacaServerConfig::new(DEFAULT_SERVER_PORT),
            switch: SwitchConfig::default(),
            observingconditions: ObservingConditionsConfig::default(),
        }
    }
}

/// Serial port configuration. No baud rate: the hub has one.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SerialConfig {
    pub port: String,
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
    /// Operator labels for the connectors on the box, keyed by built-in
    /// name — see `docs/services/gemini-powerbox-adv3-driver.md` "Operator
    /// labels". The rules are enforced while deserializing, so a bad map
    /// fails the load naming the offending entry. Omitted from the persisted
    /// file when empty, so an operator who labels nothing keeps a config file
    /// that never mentions labels.
    #[serde(default, skip_serializing_if = "GeminiSwitchLabels::is_empty")]
    pub labels: GeminiSwitchLabels,
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
    Duration::from_mins(5)
}

/// Platform-dependent default serial port. On Linux it is the hub's `by-id`
/// link, which every Gemini CH340 gets and which follows the device when
/// the hub's USB side re-enumerates and the kernel hands it another
/// `ttyUSBn` (a raw node would not be found again). A rig with a second
/// CH340 device sets a `/dev/serial/by-path/…` link instead, since the
/// by-id name is shared. On Windows (`COM3`) and macOS the value is a
/// placeholder the operator replaces; the driver restart-loops until then.
#[cfg(windows)]
const DEFAULT_SERIAL_PORT: &str = "COM3";
#[cfg(not(windows))]
const DEFAULT_SERIAL_PORT: &str = "/dev/serial/by-id/usb-1a86_USB_Serial-if00-port0";

impl Default for SerialConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_SERIAL_PORT.to_string(),
            polling_interval: default_polling_interval(),
            timeout: default_timeout(),
        }
    }
}

impl Default for SwitchConfig {
    fn default() -> Self {
        Self {
            name: "Gemini Advanced 3 Switch".to_string(),
            // Empty by default: a real UUIDv4 is minted and persisted on first
            // run by `rusty_photon_config::materialize_identity`.
            unique_id: String::new(),
            description: "Gemini Astro Power & Data Hubs Advanced 3 Power Control".to_string(),
            enabled: true,
            labels: GeminiSwitchLabels::default(),
        }
    }
}

impl Default for ObservingConditionsConfig {
    fn default() -> Self {
        Self {
            name: "Gemini Advanced 3 Weather".to_string(),
            // Empty by default: a real UUIDv4 is minted and persisted on first
            // run by `rusty_photon_config::materialize_identity`.
            unique_id: String::new(),
            description: "Gemini Astro Power & Data Hubs Advanced 3 Environmental Sensors"
                .to_string(),
            enabled: true,
            averaging_period: default_averaging_period(),
        }
    }
}

/// CLI overrides layered over the file config.
///
/// Tracks which fields are pinned by a command-line flag so the setup pages
/// can show a pinned field and never persist it (see
/// `docs/services/gemini-powerbox-adv3-driver.md` "Setup pages").
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
            config.serial.port.clone_from(port);
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
/// `Config::default()`, with CLI `overrides` applied on top.
///
/// A present-but-corrupt file is surfaced (naming the path) rather than
/// silently reset.
///
/// # Errors
///
/// Returns a message naming the path if the file exists but cannot be read or
/// does not parse as a [`Config`]; an absent file is the default, not an
/// error.
pub fn load_effective_config(
    path: &Path,
    overrides: &CliOverrides,
) -> std::result::Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let mut config = rusty_photon_config::load_file::<Config>(path)?.unwrap_or_default();
    overrides.apply(&mut config);
    Ok(config)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// A hand-written file that leaves out the device sections gains each
    /// one from the defaults, with a minted id, and then loads.
    #[test]
    #[cfg_attr(miri, ignore)] // the save fsyncs the config's directory, which Miri does not support
    fn bootstrap_fills_left_out_device_sections_from_the_defaults() {
        let default = serde_json::to_value(Config::default()).unwrap();
        let sections: Vec<&str> = IDENTITY_POINTERS
            .iter()
            .map(|pointer| pointer.split('/').nth(1).unwrap())
            .collect();
        let mut written = default.clone();
        for section in &sections {
            written.as_object_mut().unwrap().remove(*section).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, written.to_string()).unwrap();

        rusty_photon_config::materialize_identity::<Config>(&path, &default, IDENTITY_POINTERS)
            .unwrap();

        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for section in sections {
            let mut got = on_disk[section].clone();
            let id = got.as_object_mut().unwrap().remove("unique_id").unwrap();
            assert!(!id.as_str().unwrap().is_empty(), "{section}: {id}");
            let mut want = default[section].clone();
            want.as_object_mut().unwrap().remove("unique_id");
            assert_eq!(got, want, "{section}");
        }
        rusty_photon_config::load_file::<Config>(&path)
            .unwrap()
            .unwrap();
    }

    #[test]
    fn default_config_has_the_design_docs_values() {
        let config = Config::default();

        assert_eq!(config.switch.name, "Gemini Advanced 3 Switch");
        assert!(config.switch.enabled);
        assert_eq!(config.observingconditions.name, "Gemini Advanced 3 Weather");
        assert!(config.observingconditions.enabled);
        assert_eq!(
            config.observingconditions.averaging_period,
            Duration::from_mins(5)
        );

        #[cfg(not(windows))]
        assert_eq!(
            config.serial.port,
            "/dev/serial/by-id/usb-1a86_USB_Serial-if00-port0"
        );
        #[cfg(windows)]
        assert_eq!(config.serial.port, "COM3");
        assert_eq!(config.serial.polling_interval, Duration::from_secs(5));
        assert_eq!(config.serial.timeout, Duration::from_secs(2));

        assert_eq!(config.server.port, 11132);
        assert_eq!(config.server.bind_address.to_string(), "0.0.0.0");
    }

    #[test]
    fn both_unique_ids_default_empty_so_they_get_minted() {
        assert_eq!(SwitchConfig::default().unique_id, "");
        assert_eq!(ObservingConditionsConfig::default().unique_id, "");
    }

    #[test]
    fn config_serializes_to_json_without_a_baud_rate() {
        let config = Config::default();
        let json = serde_json::to_value(&config).unwrap();
        assert!(json["switch"]["name"]
            .as_str()
            .unwrap()
            .contains("Gemini Advanced 3"));
        assert_eq!(json["server"]["port"], 11132);
        assert!(
            json["serial"].get("baud_rate").is_none(),
            "the hub speaks 19200 only; a baud key is a trap: {json}"
        );
    }

    #[test]
    fn config_deserializes_from_json() {
        let json = r#"{
            "serial": {
                "port": "/dev/serial/by-path/pci-0000:00:14.0-usb-0:3:1.0-port0",
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
        assert!(config.serial.port.starts_with("/dev/serial/by-path/"));
        assert_eq!(config.serial.polling_interval, Duration::from_secs(10));
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

        assert_eq!(config.serial.port, "/dev/ttyUSB1");
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
    fn a_baud_rate_key_fails_the_load() {
        // The design doc's trap: a field whose single valid value is fixed
        // by the firmware is refused rather than accepted and ignored.
        let json = r#"{"port": "/dev/ttyUSB0", "baud_rate": 19200}"#;
        let err = serde_json::from_str::<SerialConfig>(json).unwrap_err();
        assert!(err.to_string().contains("baud_rate"), "{err}");
    }

    #[test]
    fn config_rejects_unknown_top_level_field() {
        let json = r#"{
            "serial": {"port": "/dev/ttyUSB0"},
            "server": {"port": 11132},
            "switch": {"name": "n", "description": "d"},
            "observingconditions": {"name": "n", "description": "d"},
            "plugins": []
        }"#;
        let err = serde_json::from_str::<Config>(json).unwrap_err();
        assert!(err.to_string().contains("plugins"), "{err}");
    }

    #[test]
    fn switch_config_defaults_to_no_labels() {
        assert!(SwitchConfig::default().labels.is_empty());
    }

    #[test]
    fn labels_load_from_the_switch_block() {
        let json = r#"{
            "name": "n",
            "description": "d",
            "labels": {"DC2": "Mount", "DEW6": "Guide scope heater", "USB A": "Imaging camera"}
        }"#;
        let config: SwitchConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.labels.get("DC2"), Some("Mount"));
        assert_eq!(config.labels.get("DEW6"), Some("Guide scope heater"));
        assert_eq!(config.labels.get("USB A"), Some("Imaging camera"));
    }

    #[test]
    fn a_label_on_a_switch_that_cannot_be_labelled_fails_the_load() {
        let json = r#"{"name": "n", "description": "d", "labels": {"Humidity": "Sky"}}"#;
        let err = serde_json::from_str::<SwitchConfig>(json).unwrap_err();
        assert!(err.to_string().contains("Humidity"), "{err}");
    }

    #[test]
    fn an_empty_label_map_is_not_persisted_and_a_populated_one_is() {
        let json = serde_json::to_value(SwitchConfig::default()).unwrap();
        assert!(
            json.get("labels").is_none(),
            "an operator who labels nothing should not gain a labels key: {json}"
        );
        let config = SwitchConfig {
            labels: serde_json::from_str(r#"{"DC2": "Mount"}"#).unwrap(),
            ..SwitchConfig::default()
        };
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["labels"]["DC2"], "Mount");
    }

    #[test]
    fn every_block_rejects_an_unknown_field() {
        let err = serde_json::from_str::<SwitchConfig>(
            r#"{"name": "n", "description": "d", "channels": 4}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("channels"), "{err}");
        let err = serde_json::from_str::<ObservingConditionsConfig>(
            r#"{"name": "n", "description": "d", "sample_rate": "10s"}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("sample_rate"), "{err}");
    }

    #[test]
    fn cli_overrides_pin_and_apply_their_four_fields() {
        let overrides = CliOverrides {
            serial_port: Some("/dev/ttyUSB7".to_string()),
            server_port: Some(4321),
            enable_switch: Some(false),
            enable_observingconditions: Some(false),
        };
        assert_eq!(
            overrides.pinned_paths(),
            [
                "serial.port",
                "server.port",
                "switch.enabled",
                "observingconditions.enabled"
            ]
        );
        let mut config = Config::default();
        overrides.apply(&mut config);
        assert_eq!(config.serial.port, "/dev/ttyUSB7");
        assert_eq!(config.server.port, 4321);
        assert!(!config.switch.enabled);
        assert!(!config.observingconditions.enabled);
        assert_eq!(CliOverrides::default().pinned_paths(), Vec::<String>::new());
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

        // The CH340 bridge's identity, transcribed from the dev box's bus.
        // No model: the bridge's descriptor names the chip, not the hub.
        let usb = meta.usb.unwrap();
        assert_eq!(usb.vendor, "1a86");
        assert_eq!(usb.product.as_deref(), Some("7523"));
        assert_eq!(usb.model, None);
    }
}

#[cfg(test)]
mod persisted_config_shape {
    use rusty_photon_server_config::unset::explicit_nulls;

    use super::Config;

    /// An unset optional field is spelled by its key's absence, never by an
    /// explicit `null` — see [`rusty_photon_server_config::unset`] for why.
    #[test]
    fn the_default_config_persists_no_explicit_nulls() {
        let persisted = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(
            explicit_nulls(&persisted),
            Vec::<String>::new(),
            "unset optional fields must be omitted, not written as null: {persisted}"
        );
    }

    #[test]
    fn the_loader_refuses_a_zero_interval_or_timeout_and_a_period_over_the_ceiling() {
        use rusty_photon_config::ConfigFile as _;
        let mut zero_interval = loadable_config();
        zero_interval.serial.polling_interval = std::time::Duration::ZERO;
        let mut zero_timeout = loadable_config();
        zero_timeout.serial.timeout = std::time::Duration::ZERO;
        let mut long_period = loadable_config();
        long_period.observingconditions.averaging_period = std::time::Duration::from_hours(25);
        for (field, config) in [
            ("serial.polling_interval", zero_interval),
            ("serial.timeout", zero_timeout),
            ("observingconditions.averaging_period", long_period),
        ] {
            let reason = config.check().unwrap_err();
            assert!(reason.contains(field), "{field}: {reason}");
        }
    }

    #[test]
    fn the_loader_leaves_the_port_and_the_ids_to_the_start_path() {
        use rusty_photon_config::ConfigFile as _;
        // A shipped example config: ids the bootstrap will mint and a port
        // `--port` may supply. `doctor` loads such a file before the first
        // start and must not call it broken.
        let mut config = Config::default();
        config.serial.port = String::new();
        assert_eq!(config.switch.unique_id, "");
        assert_eq!(config.observingconditions.unique_id, "");
        config.check().unwrap();
    }

    #[test]
    fn a_config_the_setup_page_accepts_passes_the_loader_check() {
        use rusty_photon_config::ConfigFile as _;
        loadable_config().check().unwrap();
    }

    /// The defaults with the ids the bootstrap would have minted.
    fn loadable_config() -> Config {
        let mut config = Config::default();
        config.switch.unique_id = "switch-id".to_string();
        config.observingconditions.unique_id = "weather-id".to_string();
        config
    }
}
