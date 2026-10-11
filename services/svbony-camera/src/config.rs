//! Configuration for the svbony-camera service.
//!
//! The shared `server` block (`AlpacaServerConfig`) binds the listener; the
//! `devices` override map (keyed by SDK serial — applied to each
//! `SvbonyCamera` at registration) mirrors `zwo-camera`'s shape. `SVBony` has
//! exactly one device type (Camera), so there is no filter-wheel-style
//! per-device-family config surface here (see ADR-014's precedent, though it
//! doesn't apply to this single-device-type SDK).

use std::collections::BTreeMap;
use std::path::Path;

use rusty_photon_doctor_checks::claims::{self, EntryView, ListError};
pub use rusty_photon_server_config::AlpacaServerConfig;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::SvbonyCameraError;

/// The default Alpaca listening port. Next free in the 1112x family; 11111-
/// 11124 are already allocated (see `docs/workspace.md`'s Services table).
pub const DEFAULT_PORT: u16 = 11125;

/// Effective service configuration.
///
/// `deny_unknown_fields` (as in `zwo-camera`/`zwo-focuser`) so typoed or
/// removed keys fail loudly at load instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Optional: pins each USB port to an Alpaca device number
    /// (docs/services/svbony-camera.md U1-U9). Absent registers every camera
    /// the SDK enumerates, in SDK order; `[]` registers none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usb_devices: Option<Vec<UsbDeviceEntry>>,
    /// Optional per-device overrides keyed by SDK serial. Read only with no
    /// `usb_devices` list; beside one it is refused.
    pub devices: BTreeMap<String, DeviceOverride>,
    /// HTTP server settings (the shared Alpaca `server` block).
    pub server: AlpacaServerConfig,
}

impl rusty_photon_config::ConfigFile for Config {}

impl Default for Config {
    fn default() -> Self {
        Self {
            usb_devices: None,
            devices: BTreeMap::new(),
            server: AlpacaServerConfig::new(DEFAULT_PORT),
        }
    }
}

impl Config {
    /// Every rule the `usb_devices` list breaks, and every `devices` override
    /// beside it; empty without a list. The load and `config.apply` both
    /// refuse a config with any.
    #[must_use]
    pub fn list_errors(&self) -> Vec<ListError> {
        let Some(list) = &self.usb_devices else {
            return Vec::new();
        };
        let views: Vec<EntryView<'_>> = list
            .iter()
            .map(|entry| EntryView {
                device_number: entry.device_number,
                usb_port: &entry.usb_port,
            })
            .collect();
        let mut errors = claims::validate_list(&views);
        errors.extend(
            self.devices
                .keys()
                .map(|key| claims::override_beside_list(key)),
        );
        errors
    }
}

/// One `usb_devices` entry: the camera on `usb_port` is served at
/// `device_number`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsbDeviceEntry {
    /// The Alpaca device number the camera on this port is served under.
    pub device_number: u32,
    /// The port, in the platform's native spelling, pasted from
    /// `svbony-camera doctor --devices`.
    pub usb_port: String,
    /// Display name override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Description override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl UsbDeviceEntry {
    /// The entry's display names as the override a camera is built with.
    #[must_use]
    pub fn display_override(&self) -> DeviceOverride {
        DeviceOverride {
            name: self.name.clone(),
            description: self.description.clone(),
        }
    }
}

/// Friendly overrides for a specific device, keyed by its SDK serial.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DeviceOverride {
    /// Display name override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Description override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// CLI overrides layered on top of the file configuration.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `--port`: overrides `server.port`.
    pub port: Option<u16>,
}

impl CliOverrides {
    /// Dotted config paths currently pinned by a CLI override.
    #[must_use]
    pub fn pinned_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        if self.port.is_some() {
            paths.push("server.port".to_owned());
        }
        paths
    }

    /// Apply the overrides onto `config` in place.
    pub const fn apply(&self, config: &mut Config) {
        if let Some(port) = self.port {
            config.server.port = port;
        }
    }
}

/// Load the on-disk config (or defaults when the file is absent) and layer CLI
/// overrides on top.
///
/// # Errors
/// Returns [`SvbonyCameraError::Config`] when the file exists but cannot be
/// read or parsed, or when its `usb_devices` list breaks a rule — every rule
/// it breaks is named, entry by entry, before any USB or SDK work.
pub fn load_effective_config(
    path: &Path,
    overrides: &CliOverrides,
) -> Result<Config, SvbonyCameraError> {
    let mut config: Config = match std::fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents)
            .map_err(|e| SvbonyCameraError::Config(format!("parse {}: {e}", path.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => {
            return Err(SvbonyCameraError::Config(format!(
                "read {}: {e}",
                path.display()
            )))
        }
    };
    let errors = config.list_errors();
    if !errors.is_empty() {
        let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
        return Err(SvbonyCameraError::Config(format!(
            "{}: {}",
            path.display(),
            errors.join("; ")
        )));
    }
    overrides.apply(&mut config);
    Ok(config)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn default_scaffold_round_trips_through_load() {
        // main() writes `Config::default()` to the platform path on first
        // start (resolve_and_init); that serialized form must load back
        // cleanly through the strict parse.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svbony-camera.json");
        let scaffold = serde_json::to_string_pretty(&Config::default()).unwrap();
        std::fs::write(&path, scaffold).unwrap();
        let c = load_effective_config(&path, &CliOverrides::default()).unwrap();
        assert_eq!(c.server.port, 11125);
    }

    #[test]
    fn default_config_uses_the_reserved_port() {
        let config = Config::default();
        assert_eq!(config.server.port, 11125);
        assert_eq!(config.server.bind_address.to_string(), "0.0.0.0");
        assert!(config.devices.is_empty());
    }

    #[test]
    fn a_typoed_device_override_field_is_rejected_loudly() {
        let err =
            serde_json::from_str::<Config>(r#"{"devices": {"SVB-1": {"descripton": "oops"}}}"#)
                .unwrap_err()
                .to_string();
        assert!(err.contains("descripton"), "{err}");
    }

    #[test]
    fn an_unknown_top_level_key_is_rejected_loudly() {
        let err = serde_json::from_str::<Config>(r#"{"filterwheel": {"enabled": false}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("filterwheel"), "{err}");
    }

    #[test]
    fn cli_port_override_wins_and_is_pinned() {
        let mut config = Config::default();
        let overrides = CliOverrides { port: Some(12345) };
        overrides.apply(&mut config);
        assert_eq!(config.server.port, 12345);
        assert_eq!(overrides.pinned_paths(), vec!["server.port".to_owned()]);
    }

    #[test]
    fn no_override_pins_nothing() {
        assert_eq!(CliOverrides::default().pinned_paths(), Vec::<String>::new());
    }

    fn load(json: &str) -> Result<Config, SvbonyCameraError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svbony-camera.json");
        std::fs::write(&path, json).unwrap();
        load_effective_config(&path, &CliOverrides::default())
    }

    #[test]
    fn a_valid_list_loads_with_its_display_names() {
        let config = load(
            r#"{"usb_devices": [
                {"device_number": 1, "usb_port": "b"},
                {"device_number": 0, "usb_port": "a", "name": "Main"}
            ]}"#,
        )
        .unwrap();
        let list = config.usb_devices.unwrap();
        assert_eq!(list[1].display_override().name.as_deref(), Some("Main"));
    }

    #[test]
    fn no_list_is_the_default() {
        assert_eq!(load("{}").unwrap().usb_devices, None);
    }

    #[test]
    fn an_empty_list_loads() {
        assert_eq!(
            load(r#"{"usb_devices": []}"#).unwrap().usb_devices,
            Some(Vec::new())
        );
    }

    #[test]
    fn a_list_that_breaks_a_rule_refuses_the_load_naming_every_entry() {
        let err = load(
            r#"{"usb_devices": [
                {"device_number": 0, "usb_port": "a"},
                {"device_number": 2, "usb_port": " a"}
            ]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("usb_devices[1]: usb_port has leading or trailing whitespace"),
            "{err}"
        );
        assert!(
            err.contains("usb_devices: device numbers must run 0..N-1, and 1 is missing"),
            "{err}"
        );
    }

    #[test]
    fn a_devices_override_beside_a_list_refuses_the_load() {
        let err = load(
            r#"{"usb_devices": [{"device_number": 0, "usb_port": "a"}],
                "devices": {"SVB0123456789AB": {"name": "Main"}}}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("devices.SVB0123456789AB: move its fields into the usb_devices entry"),
            "{err}"
        );
    }

    #[test]
    fn a_devices_override_without_a_list_still_loads() {
        let config = load(r#"{"devices": {"SVB0123456789AB": {"name": "Main"}}}"#).unwrap();
        assert!(config.devices.contains_key("SVB0123456789AB"));
    }

    #[test]
    fn an_unknown_entry_key_is_rejected_loudly() {
        let err =
            load(r#"{"usb_devices": [{"device_number": 0, "usb_port": "a", "serial": "x"}]}"#)
                .unwrap_err()
                .to_string();
        assert!(err.contains("serial"), "{err}");
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

        // Vendor-only USB identity (any SVBony device); pins the file
        // against edits. No serial device — the SDK owns the USB link.
        assert!(meta.serial.is_none());
        let usb = meta.usb.unwrap();
        assert_eq!(usb.vendor, "f266");
        assert_eq!(usb.product, None);
        assert_eq!(usb.model, None);
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
