//! Configuration types for the qhy-camera service.
//!
//! With no `usb_devices` list the hardware is the source of truth: the service
//! enumerates every connected QHY camera (and any CFW the probe finds on it) at
//! startup and registers each as an ASCOM device, and config carries only
//! optional per-serial display overrides (`devices`). A `usb_devices` list
//! instead pins each USB port to an Alpaca device number and declares the
//! camera's filter wheel (docs/services/qhy-camera.md U1-U10). Each device's
//! ASCOM `UniqueID` is derived from its SDK serial either way (see
//! `docs/services/qhy-camera.md` "Device identity"), so there is no `unique_id`
//! field and no `materialize_identity`.

use std::collections::BTreeMap;
use std::path::Path;

use rusty_photon_doctor_checks::claims::{
    self, BlockEntry, EntryView, ListEntry, ListError, Numbering,
};
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
    /// Optional: pins each USB port to an Alpaca device number and declares
    /// the camera's filter wheel (docs/services/qhy-camera.md U1-U10). Absent
    /// registers every camera the USB scan places, in port order, with the
    /// wheels the CFW probe finds (U11); `[]` registers none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usb_devices: Option<Vec<UsbDeviceEntry>>,
    /// Optional per-device overrides keyed by **SDK serial**. A device with no
    /// entry uses SDK-derived defaults. Named `devices` (not `overrides`), the
    /// name every camera driver's map has. Read only with no `usb_devices`
    /// list; beside one it is refused.
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceOverride>,
    /// HTTP server settings (the shared Alpaca `server` block).
    #[serde(default = "default_server")]
    pub server: AlpacaServerConfig,
}

impl rusty_photon_config::ConfigFile for Config {}

impl Default for Config {
    fn default() -> Self {
        Self {
            usb_devices: None,
            devices: BTreeMap::new(),
            server: default_server(),
        }
    }
}

impl Config {
    /// Every rule the `usb_devices` list breaks — the shared ones, then the
    /// declared wheels' — and every `devices` override beside it; empty
    /// without a list. The load and every setup-page save refuse a config
    /// with any.
    #[must_use]
    pub fn list_errors(&self) -> Vec<ListError> {
        let list = self.usb_devices.as_deref();
        let mut errors = claims::list_errors(list, self.devices.keys());
        errors.extend(list.map_or_default(wheel_errors));
        errors
    }

    /// [`Self::list_errors`] as one message, every rule broken named.
    ///
    /// # Errors
    /// The joined message, when the list breaks any rule.
    pub fn check_list(&self) -> Result<(), String> {
        claims::check_list(&self.list_errors())
    }
}

/// Every rule the declared wheels of `list` break (U10): `filter_names` only
/// beside a `filter_wheel_number`, each name non-empty, each wheel number
/// unique, and the numbers exactly `0..M-1` — rp binds a wheel by its position
/// among the server's wheels, so a gap would become a phantom wheel's number.
fn wheel_errors(list: &[UsbDeviceEntry]) -> Vec<ListError> {
    let mut errors = Vec::new();
    for (index, entry) in list.iter().enumerate() {
        if let Some(names) = &entry.filter_names {
            if entry.filter_wheel_number.is_none() {
                errors.push(ListError::entry(
                    index,
                    "filter_names",
                    "filter_names needs a filter_wheel_number: an entry without one declares \
                     no wheel to name"
                        .to_string(),
                ));
            }
            for (slot, name) in names.iter().enumerate() {
                if name.trim().is_empty() {
                    errors.push(ListError::entry(
                        index,
                        "filter_names",
                        format!("filter_names[{slot}] is empty"),
                    ));
                }
            }
        }
    }
    let numbers: Vec<Option<u32>> = list.iter().map(|e| e.filter_wheel_number).collect();
    errors.extend(claims::numbering_errors(&numbers, WHEEL_NUMBERS));
    errors
}

/// A list's declared wheels, numbered as its cameras are.
const WHEEL_NUMBERS: Numbering<'static> = Numbering {
    field: "filter_wheel_number",
    numbers: "filter wheel numbers",
    bound: "M",
    renumber: "wheels",
};

/// One `usb_devices` entry: the camera on `usb_port` is served at
/// `device_number`, and the CFW on it, when it declares one, at
/// `filter_wheel_number`.
///
/// The shared entry's four fields (`rusty-photon-doctor-checks`'
/// `UsbDeviceEntry`), spelled out here because this driver's adds the wheel,
/// and a flattened struct cannot refuse unknown keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsbDeviceEntry {
    /// The Alpaca device number the camera on this port is served under.
    pub device_number: u32,
    /// The port, in the platform's native spelling, pasted from
    /// `qhy-camera doctor --devices`.
    pub usb_port: String,
    /// Display name override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Description override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The camera has a CFW, served as a `FilterWheel` at this Alpaca number.
    /// Declared, not discovered: an entry without one registers no wheel,
    /// whatever is plugged in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_wheel_number: Option<u32>,
    /// Names for the declared wheel's slots, replacing `Filter0..N`. Only
    /// beside a `filter_wheel_number`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_names: Option<Vec<String>>,
}

impl ListEntry for UsbDeviceEntry {
    fn view(&self) -> EntryView<'_> {
        EntryView {
            device_number: self.device_number,
            usb_port: &self.usb_port,
        }
    }

    fn block_entry(&self) -> BlockEntry {
        BlockEntry {
            device_number: self.device_number,
            usb_port: self.usb_port.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            filter_wheel_number: self.filter_wheel_number,
            filter_names: self.filter_names.clone(),
        }
    }
}

impl From<&UsbDeviceEntry> for DeviceOverride {
    /// An entry's display names as the override its camera is built with. Its
    /// `filter_names` belong to its declared wheel, which is built from the
    /// entry directly.
    fn from(entry: &UsbDeviceEntry) -> Self {
        Self {
            name: entry.name.clone(),
            description: entry.description.clone(),
            filter_names: None,
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
/// a command-line flag so the setup pages can tell the file layer from the
/// override layer.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `--port` → `server.port`.
    pub server_port: Option<u16>,
}

impl CliOverrides {
    /// Dotted JSON paths currently pinned by an active override, which the
    /// setup pages show disabled and a save leaves as the file has them.
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
/// running driver uses.
///
/// # Errors
///
/// Returns a message naming the path if the file exists but cannot be read or
/// does not parse as a [`Config`], or when its `usb_devices` list breaks a
/// rule — every rule it breaks is named, entry by entry, before any USB or SDK
/// work. An absent file is the default, not an error.
pub fn load_effective_config(
    path: &Path,
    overrides: &CliOverrides,
) -> std::result::Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let config = read_effective_config(path, overrides)?;
    config
        .check_list()
        .map_err(|errors| format!("{}: {errors}", path.display()))?;
    Ok(config)
}

/// [`load_effective_config`] without the `usb_devices` rules.
///
/// What the file says, for a reader that must see a list the service would
/// refuse — `doctor --devices`, whose port listing is how such a list gets
/// fixed.
///
/// # Errors
///
/// Returns a message naming the path if the file exists but cannot be read or
/// does not parse as a [`Config`].
pub fn read_effective_config(
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

    fn load(json: &str) -> std::result::Result<Config, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qhy-camera.json");
        std::fs::write(&path, json).unwrap();
        load_effective_config(&path, &CliOverrides::default()).map_err(|e| e.to_string())
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
    fn a_valid_list_loads_with_its_names_and_declared_wheels() {
        let config = load(
            r#"{"usb_devices": [
                {"device_number": 1, "usb_port": "b", "filter_wheel_number": 0,
                 "filter_names": ["L", "R"]},
                {"device_number": 0, "usb_port": "a", "name": "Guide"}
            ]}"#,
        )
        .unwrap();
        let list = config.usb_devices.unwrap();
        assert_eq!(list[0].filter_wheel_number, Some(0));
        assert_eq!(list[0].filter_names, Some(vec!["L".into(), "R".into()]));
        assert_eq!(
            DeviceOverride::from(&list[1]).name.as_deref(),
            Some("Guide")
        );
        assert_eq!(list[1].filter_wheel_number, None);
    }

    #[test]
    fn an_entry_carries_its_wheel_names_to_the_wheel_not_to_the_camera() {
        let entry = UsbDeviceEntry {
            device_number: 0,
            usb_port: "a".to_string(),
            name: Some("Main".to_string()),
            description: None,
            filter_wheel_number: Some(0),
            filter_names: Some(vec!["L".to_string()]),
        };
        assert_eq!(DeviceOverride::from(&entry).filter_names, None);
        let block = entry.block_entry();
        assert_eq!(block.filter_wheel_number, Some(0));
        assert_eq!(block.filter_names, Some(vec!["L".to_string()]));
        assert_eq!(block.name.as_deref(), Some("Main"));
    }

    #[test]
    fn a_list_that_breaks_a_shared_rule_refuses_the_load_naming_the_file() {
        let err = load(
            r#"{"usb_devices": [
                {"device_number": 0, "usb_port": "a"},
                {"device_number": 2, "usb_port": " a"}
            ]}"#,
        )
        .unwrap_err();
        assert!(err.contains("qhy-camera.json: "), "{err}");
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
    fn wheel_numbers_must_run_from_zero_without_a_gap() {
        let err = load(
            r#"{"usb_devices": [
                {"device_number": 0, "usb_port": "a", "filter_wheel_number": 1},
                {"device_number": 1, "usb_port": "b", "filter_wheel_number": 3}
            ]}"#,
        )
        .unwrap_err();
        assert!(
            err.contains("filter wheel numbers must run 0..M-1, and 0 is missing"),
            "{err}"
        );
    }

    #[test]
    fn a_wheel_number_listed_twice_names_the_later_entry() {
        let config: Config = serde_json::from_str(
            r#"{"usb_devices": [
                {"device_number": 0, "usb_port": "a", "filter_wheel_number": 0},
                {"device_number": 1, "usb_port": "b", "filter_wheel_number": 0}
            ]}"#,
        )
        .unwrap();
        let errors = config.list_errors();
        let duplicate = errors
            .iter()
            .find(|e| e.path == "usb_devices.1.filter_wheel_number")
            .unwrap_or_else(|| panic!("{errors:?}"));
        assert_eq!(
            duplicate.message,
            "filter_wheel_number 0 is also usb_devices[0]'s"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("and 1 is missing")),
            "{errors:?}"
        );
    }

    #[test]
    fn filter_names_need_a_declared_wheel() {
        let config: Config = serde_json::from_str(
            r#"{"usb_devices": [{"device_number": 0, "usb_port": "a", "filter_names": ["L"]}]}"#,
        )
        .unwrap();
        let errors = config.list_errors();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "usb_devices.0.filter_names");
        assert!(
            errors[0]
                .to_string()
                .starts_with("usb_devices[0]: filter_names needs a filter_wheel_number"),
            "{}",
            errors[0]
        );
    }

    #[test]
    fn a_blank_filter_name_is_named_by_its_slot() {
        let config: Config = serde_json::from_str(
            r#"{"usb_devices": [{"device_number": 0, "usb_port": "a",
                "filter_wheel_number": 0, "filter_names": ["L", " ", ""]}]}"#,
        )
        .unwrap();
        let errors: Vec<String> = config
            .list_errors()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            errors,
            [
                "usb_devices[0]: filter_names[1] is empty",
                "usb_devices[0]: filter_names[2] is empty"
            ]
        );
    }

    #[test]
    fn a_devices_override_beside_a_list_refuses_the_load() {
        let err = load(
            r#"{"usb_devices": [{"device_number": 0, "usb_port": "a"}],
                "devices": {"QHY600M-01": {"name": "Main"}}}"#,
        )
        .unwrap_err();
        assert!(
            err.contains("devices.QHY600M-01: move its fields into the usb_devices entry"),
            "{err}"
        );
    }

    #[test]
    fn a_devices_override_without_a_list_still_loads() {
        let config = load(r#"{"devices": {"QHY600M-01": {"name": "Main"}}}"#).unwrap();
        assert!(config.devices.contains_key("QHY600M-01"));
    }

    #[test]
    fn an_unknown_entry_key_is_rejected_loudly() {
        let err =
            load(r#"{"usb_devices": [{"device_number": 0, "usb_port": "a", "serial": "x"}]}"#)
                .unwrap_err();
        assert!(err.contains("serial"), "{err}");
    }

    #[test]
    fn a_list_the_load_refuses_is_still_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qhy-camera.json");
        std::fs::write(
            &path,
            r#"{"usb_devices": [{"device_number": 1, "usb_port": "a"}]}"#,
        )
        .unwrap();
        let config = read_effective_config(&path, &CliOverrides::default()).unwrap();
        assert_eq!(config.usb_devices.unwrap()[0].device_number, 1);
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
