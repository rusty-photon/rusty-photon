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

use rusty_photon_doctor_checks::claims::{self, EntryView, ListError};
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
    /// Optional: pins each USB port to an Alpaca device number
    /// (docs/services/zwo-camera.md U1-U9). Absent registers every camera the
    /// SDK enumerates, in SDK order; `[]` registers none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usb_devices: Option<Vec<UsbDeviceEntry>>,
    /// Optional per-device overrides keyed by SDK serial. Read only with no
    /// `usb_devices` list; beside one it is refused.
    pub devices: BTreeMap<String, DeviceOverride>,
    /// Which `MaxADU` contract to present to clients (ST3).
    pub max_adu_reporting: MaxAduReporting,
    /// HTTP server settings (the shared Alpaca `server` block).
    pub server: AlpacaServerConfig,
}

impl rusty_photon_config::ConfigFile for Config {}

impl Default for Config {
    fn default() -> Self {
        Self {
            usb_devices: None,
            devices: BTreeMap::new(),
            max_adu_reporting: MaxAduReporting::default(),
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

    /// [`Self::list_errors`] as one message, every rule broken named.
    ///
    /// # Errors
    /// The joined message, when the list breaks any rule.
    pub fn check_list(&self) -> Result<(), String> {
        let errors = self.list_errors();
        if errors.is_empty() {
            return Ok(());
        }
        let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
        Err(errors.join("; "))
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
    /// `zwo-camera doctor --devices`.
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
/// parsed, or when its `usb_devices` list breaks a rule — every rule it breaks
/// is named, entry by entry, before any USB or SDK work.
pub fn load_effective_config(
    path: &Path,
    overrides: &CliOverrides,
) -> Result<Config, ZwoCameraError> {
    let config = read_effective_config(path, overrides)?;
    config
        .check_list()
        .map_err(|errors| ZwoCameraError::Config(format!("{}: {errors}", path.display())))?;
    Ok(config)
}

/// [`load_effective_config`] without the `usb_devices` rules.
///
/// What the file says, for a reader that must see a list the service would
/// refuse — `doctor --devices`, whose port listing is how such a list gets
/// fixed.
///
/// # Errors
/// Returns [`ZwoCameraError::Config`] when the file exists but cannot be read or
/// parsed.
pub fn read_effective_config(
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

    fn load(json: &str) -> Result<Config, ZwoCameraError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zwo-camera.json");
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
                "devices": {"1915d5081b090900": {"name": "Main"}}}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("devices.1915d5081b090900: move its fields into the usb_devices entry"),
            "{err}"
        );
    }

    #[test]
    fn a_devices_override_without_a_list_still_loads() {
        let config = load(r#"{"devices": {"1915d5081b090900": {"name": "Main"}}}"#).unwrap();
        assert!(config.devices.contains_key("1915d5081b090900"));
    }

    #[test]
    fn an_unknown_entry_key_is_rejected_loudly() {
        let err =
            load(r#"{"usb_devices": [{"device_number": 0, "usb_port": "a", "serial": "x"}]}"#)
                .unwrap_err()
                .to_string();
        assert!(err.contains("serial"), "{err}");
    }

    #[test]
    fn a_list_the_load_refuses_is_still_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zwo-camera.json");
        std::fs::write(
            &path,
            r#"{"usb_devices": [{"device_number": 1, "usb_port": "a"}]}"#,
        )
        .unwrap();
        let config = read_effective_config(&path, &CliOverrides::default()).unwrap();
        assert_eq!(config.usb_devices.unwrap()[0].device_number, 1);
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
