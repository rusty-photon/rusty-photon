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
    /// Optional per-device overrides keyed by SDK serial.
    pub devices: BTreeMap<String, DeviceOverride>,
    /// HTTP server settings (the shared Alpaca `server` block).
    pub server: AlpacaServerConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            devices: BTreeMap::new(),
            server: AlpacaServerConfig::new(DEFAULT_PORT),
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
/// read or parsed.
pub fn load_effective_config(
    path: &Path,
    overrides: &CliOverrides,
) -> Result<Config, SvbonyCameraError> {
    let mut config = match std::fs::read_to_string(path) {
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
