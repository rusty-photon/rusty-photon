//! `ConfigurableDriver` impl for qhy-camera — wires the cross-driver
//! `config.get` / `config.apply` / `config.schema` protocol
//! (`docs/services/config-actions.md`) to this service's [`Config`].
//!
//! Editability tiers:
//! - **Locked (identity):** none. ASCOM `UniqueID`s are derived from the
//!   camera/CFW SDK serial (see `docs/services/qhy-camera.md` "Device identity"),
//!   not minted into config, so there is no identity field to lock.
//! - **Hard read-only:** `server.port` (a BFF could not follow the rebind).
//! - **Editable:** `usb_devices` and the per-serial `devices` map (`name` /
//!   `description` / `filter_names`).

use rusty_photon_config::actions::{ConfigurableDriver, FieldError};
use rusty_photon_doctor_checks::claims;

use crate::config::{CliOverrides, Config};

/// Zero-sized marker implementing [`ConfigurableDriver`] for the qhy-camera
/// [`Config`]. The generic `rusty_photon_driver::dispatch` routes the three
/// config actions against this.
pub struct QhyCameraDriver;

impl ConfigurableDriver for QhyCameraDriver {
    type Config = Config;
    type Overrides = CliOverrides;

    fn normalize(_config: &mut Config) {}

    /// The `usb_devices` rules the load applies (U9), plus the one only an
    /// apply has: an empty list is refused, because it registers no device, so
    /// none would be left to serve the config actions. Then the `devices`
    /// overrides' filter names, each non-empty.
    fn validate(config: &Config) -> Vec<FieldError> {
        let mut list_errors = config.list_errors();
        if config.usb_devices.as_ref().is_some_and(Vec::is_empty) {
            list_errors.push(claims::empty_list_over_apply());
        }
        let mut errors: Vec<FieldError> = list_errors
            .into_iter()
            .map(|error| FieldError {
                path: error.path,
                msg: error.message,
            })
            .collect();
        for (serial, device) in &config.devices {
            if let Some(names) = &device.filter_names {
                for (index, name) in names.iter().enumerate() {
                    if name.trim().is_empty() {
                        errors.push(FieldError {
                            path: format!("devices.{serial}.filter_names.{index}"),
                            msg: "filter name must not be empty".to_string(),
                        });
                    }
                }
            }
        }
        errors
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
    fn empty_filter_name_is_rejected() {
        let mut config = Config::default();
        config.devices.insert(
            "CFW-1".to_string(),
            DeviceOverride {
                filter_names: Some(vec!["L".to_string(), "  ".to_string()]),
                ..Default::default()
            },
        );
        let errors = QhyCameraDriver::validate(&config);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "devices.CFW-1.filter_names.1");
    }

    #[test]
    fn valid_config_has_no_errors() {
        let mut config = Config::default();
        config.devices.insert(
            "QHY600M-0".to_string(),
            DeviceOverride {
                name: Some("Main".to_string()),
                description: Some("desc".to_string()),
                filter_names: None,
            },
        );
        assert_eq!(
            QhyCameraDriver::validate(&config),
            Vec::<rusty_photon_config::actions::FieldError>::new()
        );
    }

    fn entry(device_number: u32, usb_port: &str) -> crate::config::UsbDeviceEntry {
        crate::config::UsbDeviceEntry {
            device_number,
            usb_port: usb_port.to_string(),
            name: None,
            description: None,
            filter_wheel_number: None,
            filter_names: None,
        }
    }

    #[test]
    fn an_empty_list_is_refused_over_apply() {
        let config = Config {
            usb_devices: Some(Vec::new()),
            ..Config::default()
        };
        let errors = QhyCameraDriver::validate(&config);
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
    fn a_list_that_breaks_a_rule_is_answered_entry_by_entry() {
        let mut wheel = entry(1, "b");
        wheel.filter_wheel_number = Some(0);
        let mut twin = entry(0, "a");
        twin.filter_wheel_number = Some(0);
        let config = Config {
            usb_devices: Some(vec![twin, wheel]),
            ..Config::default()
        };
        let errors = QhyCameraDriver::validate(&config);
        assert!(
            errors
                .iter()
                .any(|e| e.path == "usb_devices.1.filter_wheel_number"
                    && e.msg == "filter_wheel_number 0 is also usb_devices[0]'s"),
            "{errors:?}"
        );
    }

    #[test]
    fn a_valid_list_has_no_errors() {
        let config = Config {
            usb_devices: Some(vec![entry(0, "a")]),
            ..Config::default()
        };
        assert_eq!(QhyCameraDriver::validate(&config), Vec::new());
    }

    #[test]
    fn no_locked_identity_fields() {
        assert_eq!(QhyCameraDriver::locked_paths(), Vec::<&str>::new());
    }

    #[test]
    fn port_is_read_only() {
        let read_only = QhyCameraDriver::read_only_paths();
        assert!(read_only.contains(&"server.port"));
    }

    #[test]
    fn port_override_is_pinned_and_applied() {
        let overrides = CliOverrides {
            server_port: Some(12321),
        };
        assert_eq!(
            QhyCameraDriver::override_paths(&overrides),
            vec!["server.port".to_string()]
        );
        let mut config = Config::default();
        QhyCameraDriver::apply_overrides(&mut config, &overrides);
        assert_eq!(config.server.port, 12321);
    }
}
