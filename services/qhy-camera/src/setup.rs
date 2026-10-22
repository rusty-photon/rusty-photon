//! qhy-camera's [`ConfigurableDriver`] implementation — the driver-specific
//! half behind its setup pages.
//!
//! The pages themselves (the render, the save, the origin check, the
//! Security section, the confirmation, the reload) are `rusty-photon-setup`'s,
//! and the camera drivers' own part of them — the Cameras section that edits
//! `usb_devices` with each camera's declared wheel, each camera's page and
//! each wheel's — is `rusty-photon-driver`'s `claims` module, which `lib.rs`
//! feeds what the build registered. This module supplies what is the
//! service's own: its `Config` type, validation, its secret, and the `--port`
//! pin. No field is locked: `UniqueID`s are derived from the camera/CFW SDK
//! id, not minted into config. See [`docs/services/qhy-camera.md`] "Setup
//! pages".
//!
//! [`docs/services/qhy-camera.md`]: ../../../docs/services/qhy-camera.md

use rusty_photon_config::actions::{ConfigurableDriver, FieldError};

use crate::config::{CliOverrides, Config};

/// Driver marker wiring qhy-camera's [`Config`] into the shared setup pages.
pub struct QhyCameraDriver;

impl ConfigurableDriver for QhyCameraDriver {
    type Config = Config;
    type Overrides = CliOverrides;

    fn normalize(_config: &mut Config) {}

    /// The `usb_devices` rules the load applies (U9), each named by its
    /// entry's field — an empty list is legal: the server page outlives it —
    /// then the `devices` overrides' filter names, each non-empty.
    fn validate(config: &Config) -> Vec<FieldError> {
        let mut errors: Vec<FieldError> = config
            .list_errors()
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

    /// The one secret: the server-auth password hash, which the pages'
    /// Security section edits and never shows. `TlsConfig` stores file
    /// *paths*, not key material.
    fn secret_pointers() -> &'static [&'static str] {
        &["/server/auth/password_hash"]
    }

    fn override_paths(overrides: &CliOverrides) -> Vec<String> {
        overrides.pinned_paths()
    }

    fn apply_overrides(config: &mut Config, overrides: &CliOverrides) {
        overrides.apply(config);
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
        assert_eq!(QhyCameraDriver::validate(&config), Vec::<FieldError>::new());
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
    fn an_empty_list_is_saved_like_any_other() {
        let config = Config {
            usb_devices: Some(Vec::new()),
            ..Config::default()
        };
        assert_eq!(QhyCameraDriver::validate(&config), Vec::<FieldError>::new());
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
    fn an_empty_filter_name_in_an_entry_is_named_by_its_field() {
        let mut named = entry(0, "a");
        named.filter_wheel_number = Some(0);
        named.filter_names = Some(vec!["L".to_string(), String::new()]);
        let config = Config {
            usb_devices: Some(vec![named]),
            ..Config::default()
        };
        let errors = QhyCameraDriver::validate(&config);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "usb_devices.0.filter_names");
        assert_eq!(errors[0].msg, "filter_names[1] is empty");
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
    fn port_override_is_pinned_and_applied() {
        let overrides = CliOverrides {
            server_port: Some(12321),
        };
        assert_eq!(
            QhyCameraDriver::pinned_paths(&Config::default(), &overrides),
            vec!["server.port".to_string()]
        );
        let mut config = Config::default();
        QhyCameraDriver::apply_overrides(&mut config, &overrides);
        assert_eq!(config.server.port, 12321);
    }
}
