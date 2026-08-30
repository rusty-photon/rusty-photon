//! Configuration for the zwo-camera service.
//!
//! The shared `server` block (`AlpacaServerConfig`) binds the listener; the
//! `devices` override map (keyed by
//! SDK serial — applied to each `ZwoCamera` at registration) is live as of
//! Phase E, and the whole `Config` is exposed through the
//! `config.get`/`apply`/`schema` actions (see `config_actions.rs`). EFW filter
//! wheels are out of scope: they belong to a future separate service
//! (ADR-014), so there is no filter-wheel config surface here.

use std::collections::BTreeMap;
use std::path::Path;

pub use rusty_photon_server_config::AlpacaServerConfig;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ZwoCameraError;

/// The default Alpaca listening port. Next free in the 1112x family; 11121 is
/// `qhy-camera`.
pub const DEFAULT_PORT: u16 = 11122;

/// Effective service configuration.
///
/// `deny_unknown_fields` (as in zwo-focuser and the other newer services) so
/// typoed or removed keys fail loudly at load instead of being silently
/// ignored — in particular the pre-ADR-014 `filterwheel` section, which is no
/// longer valid here (the EFW belongs to a future separate service).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Optional per-device overrides keyed by SDK serial (Phase E+).
    pub devices: BTreeMap<String, DeviceOverride>,
    /// Which `MaxADU` contract to present to clients (ST3).
    pub max_adu_reporting: MaxAduReporting,
    /// HTTP server settings (the shared Alpaca `server` block).
    pub server: AlpacaServerConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            devices: BTreeMap::new(),
            max_adu_reporting: MaxAduReporting::default(),
            server: AlpacaServerConfig::new(DEFAULT_PORT),
        }
    }
}

/// Which `MaxADU` contract the service presents (ST3).
///
/// The two differ **only** where the ADC is packed into a larger container —
/// sub-16-bit depths in `Raw16`. `Raw8` reports 255 either way, and a 16-bit or
/// unknown depth reports 65535 either way, because there the container's own
/// maximum is the answer under both readings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MaxAduReporting {
    /// One quantization step below the delivered format's full scale, so that a
    /// sensor clipping short of its top ADC code still satisfies
    /// `pixel >= MaxADU`. The default: saturation detection is the capability
    /// the property exists for, and it should work unconfigured.
    #[default]
    SaturationThreshold,
    /// The delivered container's own maximum — 65535 in `Raw16`. What ZWO's own
    /// ASCOM driver reports, for clients written against that value.
    ///
    /// This **disables saturation detection** on a sub-16-bit sensor: no pixel
    /// can reach the value. It is a compatibility mode, not a second correct
    /// answer.
    ContainerFullScale,
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
/// Returns [`ZwoCameraError::Config`] when the file exists but cannot be read or
/// parsed.
pub fn load_effective_config(
    path: &Path,
    overrides: &CliOverrides,
) -> Result<Config, ZwoCameraError> {
    let mut config = match std::fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents)
            .map_err(|e| ZwoCameraError::Config(format!("parse {}: {e}", path.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => {
            return Err(ZwoCameraError::Config(format!(
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
        let path = dir.path().join("zwo-camera.json");
        let scaffold = serde_json::to_string_pretty(&Config::default()).unwrap();
        std::fs::write(&path, scaffold).unwrap();
        let c = load_effective_config(&path, &CliOverrides::default()).unwrap();
        assert_eq!(c.server.port, 11122);
    }

    #[test]
    fn default_config_uses_the_reserved_port() {
        let config = Config::default();
        assert_eq!(config.server.port, 11122);
        assert_eq!(config.server.bind_address.to_string(), "0.0.0.0");
        assert!(config.devices.is_empty());
    }

    #[test]
    fn a_legacy_filterwheel_section_is_rejected_loudly() {
        // Pre-ADR-014 configs carried a `filterwheel` section; the EFW moved
        // to its own future service, so the key must fail at load rather than
        // be silently ignored.
        let err = serde_json::from_str::<Config>(r#"{"filterwheel": {"enabled": false}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("filterwheel"), "{err}");
    }

    #[test]
    fn max_adu_reporting_defaults_to_the_accurate_threshold() {
        // Saturation detection is the capability MaxADU exists for, so it works
        // without configuration; the ZWO-compatible value is the opt-out.
        assert_eq!(
            Config::default().max_adu_reporting,
            MaxAduReporting::SaturationThreshold
        );
        // An older config predating the key must load and keep that default.
        let c: Config = serde_json::from_str(r#"{"devices": {}}"#).unwrap();
        assert_eq!(c.max_adu_reporting, MaxAduReporting::SaturationThreshold);
    }

    #[test]
    fn max_adu_reporting_round_trips_through_its_wire_names() {
        let c: Config =
            serde_json::from_str(r#"{"max_adu_reporting": "container_full_scale"}"#).unwrap();
        assert_eq!(c.max_adu_reporting, MaxAduReporting::ContainerFullScale);
        // The serialized scaffold must parse back through the strict loader.
        let text = serde_json::to_string(&c).unwrap();
        assert!(text.contains("container_full_scale"), "{text}");
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.max_adu_reporting, MaxAduReporting::ContainerFullScale);
    }

    #[test]
    fn an_unknown_max_adu_reporting_value_is_rejected_loudly() {
        // A typo must not silently fall back to the default and leave the
        // operator believing they changed the contract. "65535" is the
        // plausible wrong guess, since that is the value being asked for.
        let err = serde_json::from_str::<Config>(r#"{"max_adu_reporting": "65535"}"#)
            .unwrap_err()
            .to_string();
        // The message must name the accepted values, or the operator is left
        // guessing at the spelling of a key they cannot see in the schema.
        assert!(err.contains("saturation_threshold"), "{err}");
        assert!(err.contains("container_full_scale"), "{err}");
    }

    #[test]
    fn a_typoed_device_override_field_is_rejected_loudly() {
        let err =
            serde_json::from_str::<Config>(r#"{"devices": {"ASI-1": {"descripton": "oops"}}}"#)
                .unwrap_err()
                .to_string();
        assert!(err.contains("descripton"), "{err}");
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
