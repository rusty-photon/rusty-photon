//! `ConfigurableDriver` impl for svbony-camera — wires the cross-driver
//! `config.get` / `config.apply` / `config.schema` protocol
//! (`docs/services/config-actions.md`) to this service's [`Config`].
//!
//! Editability tiers (mirrors `zwo-camera`):
//! - **Locked (identity):** none. ASCOM `UniqueID`s are derived from the
//!   camera SDK serial (see `docs/services/svbony-camera.md` "Device
//!   identity"), not minted into config, so there is no identity field to
//!   lock.
//! - **Hard read-only:** `server.port` (a BFF could not follow the rebind).
//! - **Editable:** the `usb_devices` list and the per-serial `devices` map
//!   (`name` / `description`).

use rusty_photon_config::actions::{ConfigurableDriver, FieldError};
use rusty_photon_doctor_checks::claims;

use crate::config::{CliOverrides, Config};

/// Zero-sized marker implementing [`ConfigurableDriver`] for the
/// svbony-camera [`Config`]. The generic `rusty_photon_driver::dispatch`
/// routes the three config actions against this.
pub struct SvbonyCameraDriver;

impl ConfigurableDriver for SvbonyCameraDriver {
    type Config = Config;
    type Overrides = CliOverrides;

    fn normalize(_config: &mut Config) {}

    /// The `usb_devices` rules the load applies (U9), plus the one only an
    /// apply has: an empty list is refused, because it would remove the device
    /// the apply arrived through. The per-serial overrides are free-form
    /// name/description strings.
    fn validate(config: &Config) -> Vec<FieldError> {
        let mut errors = config.list_errors();
        if config.usb_devices.as_ref().is_some_and(Vec::is_empty) {
            errors.push(claims::empty_list_over_apply());
        }
        errors
            .into_iter()
            .map(|error| FieldError {
                path: error.path,
                msg: error.message,
            })
            .collect()
    }

    /// The one secret: the server-auth password hash. `TlsConfig` stores file
    /// *paths*, not key material, so there is nothing to redact there.
    fn secret_pointers() -> &'static [&'static str] {
        &["/server/auth/password_hash"]
    }

    fn override_paths(overrides: &CliOverrides) -> Vec<String> {
        overrides.pinned_paths()
    }

    fn apply_overrides(config: &mut Config, overrides: &CliOverrides) {
        overrides.apply(config);
    }

    // `locked_paths()` intentionally not overridden (defaults to `&[]`): the
    // hardware-derived UniqueID means there is no locked identity field.

    fn read_only_paths() -> &'static [&'static str] {
        &["server.port"]
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::DeviceOverride;

    #[test]
    fn valid_config_has_no_errors() {
        let mut config = Config::default();
        config.devices.insert(
            "SVB0123456789AB".to_string(),
            DeviceOverride {
                name: Some("Main".to_string()),
                description: Some("desc".to_string()),
            },
        );
        assert_eq!(
            SvbonyCameraDriver::validate(&config),
            Vec::<rusty_photon_config::actions::FieldError>::new()
        );
    }

    fn entry(device_number: u32, usb_port: &str) -> crate::config::UsbDeviceEntry {
        crate::config::UsbDeviceEntry {
            device_number,
            usb_port: usb_port.to_string(),
            name: None,
            description: None,
        }
    }

    #[test]
    fn a_valid_list_has_no_errors() {
        let config = Config {
            usb_devices: Some(vec![entry(0, "a")]),
            ..Config::default()
        };
        assert_eq!(
            SvbonyCameraDriver::validate(&config),
            Vec::<FieldError>::new()
        );
    }

    #[test]
    fn an_empty_list_is_refused_at_the_list() {
        let config = Config {
            usb_devices: Some(Vec::new()),
            ..Config::default()
        };
        let errors = SvbonyCameraDriver::validate(&config);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "usb_devices");
        assert!(
            errors[0]
                .msg
                .starts_with("an empty list registers no camera"),
            "{}",
            errors[0].msg
        );
    }

    #[test]
    fn a_broken_entry_is_named_by_its_field_path() {
        let config = Config {
            usb_devices: Some(vec![entry(0, "a"), entry(0, "b")]),
            ..Config::default()
        };
        let errors = SvbonyCameraDriver::validate(&config);
        let repeat = errors
            .iter()
            .find(|e| e.path == "usb_devices.1.device_number")
            .unwrap();
        assert_eq!(repeat.msg, "device_number 0 is also usb_devices[0]'s");
    }

    #[test]
    fn a_devices_override_beside_a_list_is_named_by_its_key() {
        let mut config = Config {
            usb_devices: Some(vec![entry(0, "a")]),
            ..Config::default()
        };
        config
            .devices
            .insert("SVB-1".to_string(), DeviceOverride::default());
        let errors = SvbonyCameraDriver::validate(&config);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "devices.SVB-1");
    }

    #[test]
    fn no_locked_identity_fields() {
        assert_eq!(SvbonyCameraDriver::locked_paths(), Vec::<&str>::new());
    }

    #[test]
    fn port_is_read_only() {
        assert_eq!(SvbonyCameraDriver::read_only_paths(), &["server.port"]);
    }

    #[test]
    fn port_override_is_pinned_and_applied() {
        let overrides = CliOverrides { port: Some(12321) };
        assert_eq!(
            SvbonyCameraDriver::override_paths(&overrides),
            vec!["server.port".to_string()]
        );
        let mut config = Config::default();
        SvbonyCameraDriver::apply_overrides(&mut config, &overrides);
        assert_eq!(config.server.port, 12321);
    }
}
