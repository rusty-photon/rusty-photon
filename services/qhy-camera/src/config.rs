//! Configuration types for the qhy-camera service.
//!
//! The hardware is the source of truth: the service enumerates every connected
//! QHY camera (and any CFW discovered on it) at startup and registers each
//! as an ASCOM device. Config therefore carries no per-camera *binding* — only
//! optional per-serial display overrides (`devices`) and the listening port
//! (`server.port`). Each device's
//! ASCOM `UniqueID` is derived from its SDK serial (see `docs/services/qhy-camera.md`
//! "Device identity"), so there is no `unique_id` field and no `materialize_identity`.

use std::collections::BTreeMap;
use std::path::Path;

pub use rusty_photon_server_config::AlpacaServerConfig;
use serde::{Deserialize, Serialize};

/// Top-level service configuration.
///
/// `deny_unknown_fields` (as in zwo-camera and the other newer services) so
/// typoed or removed keys fail loudly at load instead of being silently
/// ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Optional per-device overrides keyed by **SDK serial**. A device with no
    /// entry uses SDK-derived defaults. Named `devices` (not `overrides`) to
    /// avoid colliding with the `config.get` response's own `overrides[]` field.
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceOverride>,
    /// HTTP server settings (the shared Alpaca `server` block).
    #[serde(default = "default_server")]
    pub server: AlpacaServerConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            devices: BTreeMap::new(),
            server: default_server(),
        }
    }
}

/// qhy-camera's default `server` block when the config file omits it: port
/// 11121 (next free in the 1112x family) on all interfaces, plain HTTP.
const fn default_server() -> AlpacaServerConfig {
    AlpacaServerConfig::new(11121)
}

/// Per-device override, keyed by SDK serial in [`Config::devices`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceOverride {
    /// Friendly camera name (overrides the SDK-derived default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Friendly camera description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Human filter names for a CFW (overrides the generated `Filter0..N`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_names: Option<Vec<String>>,
}

/// CLI overrides layered over the file config. Tracks which fields are pinned by
/// a command-line flag so the config actions can distinguish the file layer from
/// the override layer.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `--port` → `server.port`.
    pub server_port: Option<u16>,
}

impl CliOverrides {
    /// Dotted JSON paths currently pinned by an active override. Reported by
    /// `config.get` (`overrides[]`) and skipped by `config.apply`.
    #[must_use]
    pub fn pinned_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        if self.server_port.is_some() {
            paths.push("server.port".to_string());
        }
        paths
    }

    /// Apply the overrides onto `config` in place.
    pub const fn apply(&self, config: &mut Config) {
        if let Some(port) = self.server_port {
            config.server.port = port;
        }
    }
}

/// Load the effective config: the file at `path` if it exists, else
/// [`Config::default`], with CLI `overrides` applied on top. This is what the
/// running driver uses and what `config.get` reports.
///
/// # Errors
///
/// Returns a message naming the path if the file exists but cannot be read or
/// does not parse as a [`Config`]; an absent file is the default, not an error.
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
    fn default_scaffold_round_trips_through_load() {
        // main() writes `Config::default()` to the platform path on first
        // start (resolve_and_init); that serialized form must load back
        // cleanly through the strict parse.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qhy-camera.json");
        let scaffold = serde_json::to_string_pretty(&Config::default()).unwrap();
        std::fs::write(&path, scaffold).unwrap();
        let c = load_effective_config(&path, &CliOverrides::default()).unwrap();
        assert_eq!(c.server.port, 11121);
    }

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert!(c.devices.is_empty());
        assert_eq!(c.server.port, 11121);
        assert_eq!(c.server.bind_address.to_string(), "0.0.0.0");
    }

    #[test]
    fn empty_object_deserialises_to_defaults() {
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c.server.port, 11121);
        assert!(c.devices.is_empty());
    }

    #[test]
    fn full_config_round_trips() {
        let json = r#"{
            "devices": {
                "QHY600M-0123456789": { "name": "Main Imaging", "description": "QHY600M @ 1000mm" },
                "CFW3L-SR-9876543210": { "filter_names": ["L", "R", "G", "B"] }
            },
            "server": { "port": 12000 }
        }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.server.port, 12000);
        assert_eq!(
            c.devices["QHY600M-0123456789"].name.as_deref(),
            Some("Main Imaging")
        );
        assert_eq!(
            c.devices["CFW3L-SR-9876543210"]
                .filter_names
                .as_ref()
                .unwrap(),
            &vec!["L", "R", "G", "B"]
        );
    }

    #[test]
    fn a_typoed_top_level_field_is_rejected_loudly() {
        let err = serde_json::from_str::<Config>(r#"{"devcies": {}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("devcies"), "{err}");
    }

    #[test]
    fn a_typoed_device_override_field_is_rejected_loudly() {
        let err =
            serde_json::from_str::<Config>(r#"{"devices": {"QHY600M-01": {"descripton": "x"}}}"#)
                .unwrap_err()
                .to_string();
        assert!(err.contains("descripton"), "{err}");
    }

    #[test]
    fn cli_override_pins_and_applies_port() {
        let overrides = CliOverrides {
            server_port: Some(9999),
        };
        assert_eq!(overrides.pinned_paths(), vec!["server.port".to_string()]);
        let mut c = Config::default();
        overrides.apply(&mut c);
        assert_eq!(c.server.port, 9999);
    }

    #[test]
    fn load_effective_config_missing_file_uses_defaults() {
        // A fresh temp dir guarantees the path does not exist (a fixed /tmp path
        // could be left over from a prior run and make the test assert against
        // real contents).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");
        let c = load_effective_config(&path, &CliOverrides::default()).unwrap();
        assert_eq!(c.server.port, 11121);
    }

    #[test]
    fn load_effective_config_corrupt_file_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qhy-camera.json");
        std::fs::write(&path, "{ not valid json").unwrap();
        let err = load_effective_config(&path, &CliOverrides::default()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not valid JSON"), "{msg}");
        assert!(msg.contains(&path.display().to_string()), "{msg}");
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

        // Vendor-only USB identity (any QHY camera); pins the file
        // against edits. No serial device — the SDK owns the USB link.
        assert!(meta.serial.is_none());
        let usb = meta.usb.unwrap();
        assert_eq!(usb.vendor, "1618");
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
