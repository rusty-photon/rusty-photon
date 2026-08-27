//! Configuration for the zwo-focuser service.
//!
//! `server.socket_addr()` binds the listener; the `devices` override map
//! (keyed by SDK serial — applied to each `ZwoFocuser` at registration) and
//! the whole `Config` are exposed through the `config.get`/`apply`/`schema`
//! actions (see `config_actions.rs`).

use std::collections::BTreeMap;
use std::path::Path;

pub use rusty_photon_server_config::AlpacaServerConfig;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ZwoFocuserError;

/// The default Alpaca listening port. Next free in the 1112x family; 11123 is
/// `pa-scops-oag`.
pub const DEFAULT_PORT: u16 = 11124;

/// Effective service configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Optional per-device overrides keyed by SDK serial.
    pub devices: BTreeMap<String, DeviceOverride>,
    /// HTTP server settings (the shared `rusty-photon-server-config` shape).
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
    pub name: Option<String>,
    /// Description override.
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
/// Returns [`ZwoFocuserError::Config`] when the file exists but cannot be read
/// or parsed.
pub fn load_effective_config(
    path: &Path,
    overrides: &CliOverrides,
) -> Result<Config, ZwoFocuserError> {
    let mut config = match std::fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents)
            .map_err(|e| ZwoFocuserError::Config(format!("parse {}: {e}", path.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => {
            return Err(ZwoFocuserError::Config(format!(
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
        let path = dir.path().join("zwo-focuser.json");
        let scaffold = serde_json::to_string_pretty(&Config::default()).unwrap();
        std::fs::write(&path, scaffold).unwrap();
        let c = load_effective_config(&path, &CliOverrides::default()).unwrap();
        assert_eq!(c.server.port, 11124);
    }

    #[test]
    fn default_config_uses_the_reserved_port() {
        let config = Config::default();
        assert_eq!(config.server.port, 11124);
        assert_eq!(config.server.bind_address.to_string(), "0.0.0.0");
        assert!(config.devices.is_empty());
    }

    #[test]
    fn a_config_without_a_server_block_keeps_the_default_port() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config.server.port, 11124);
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

        // Vendor-only USB identity (any ZWO device); pins the file
        // against edits. No serial device — the SDK owns the USB link.
        assert!(meta.serial.is_none());
        let usb = meta.usb.unwrap();
        assert_eq!(usb.vendor, "03c3");
        assert_eq!(usb.product, None);
        assert_eq!(usb.model, None);
    }
}
