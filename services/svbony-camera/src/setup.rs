//! svbony-camera's [`ConfigurableDriver`] implementation — the
//! driver-specific half behind its setup pages.
//!
//! The pages themselves (the render, the save, the origin check, the
//! Security section, the confirmation, the reload) are `rusty-photon-setup`'s,
//! and the camera drivers' own part of them — the Cameras section that edits
//! `usb_devices`, and each camera's page — is `rusty-photon-driver`'s
//! `claims` module, which `lib.rs` feeds what the build registered. This
//! module supplies what is the service's own: its `Config` type, validation,
//! its secret, and the `--port` pin. See [`docs/services/svbony-camera.md`]
//! "Setup pages".
//!
//! [`docs/services/svbony-camera.md`]: ../../../docs/services/svbony-camera.md

use rusty_photon_config::driver::{ConfigurableDriver, FieldError};

use crate::config::{CliOverrides, Config};

/// Driver marker wiring svbony-camera's [`Config`] into the shared setup
/// pages.
pub struct SvbonyCameraDriver;

impl ConfigurableDriver for SvbonyCameraDriver {
    type Config = Config;
    type Overrides = CliOverrides;

    fn normalize(_config: &mut Config) {}

    /// The `usb_devices` rules the load applies (U9), each named by its
    /// entry's field. An empty list is legal: the server page outlives it.
    fn validate(config: &Config) -> Vec<FieldError> {
        config
            .list_errors()
            .into_iter()
            .map(|error| FieldError {
                path: error.path,
                msg: error.message,
            })
            .collect()
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
            Vec::<FieldError>::new()
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
    fn an_empty_list_is_saved_like_any_other() {
        let config = Config {
            usb_devices: Some(Vec::new()),
            ..Config::default()
        };
        assert_eq!(
            SvbonyCameraDriver::validate(&config),
            Vec::<FieldError>::new()
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
    fn port_override_is_pinned_and_applied() {
        let overrides = CliOverrides { port: Some(12321) };
        assert_eq!(
            SvbonyCameraDriver::pinned_paths(&Config::default(), &overrides),
            vec!["server.port".to_string()]
        );
        let mut config = Config::default();
        SvbonyCameraDriver::apply_overrides(&mut config, &overrides);
        assert_eq!(config.server.port, 12321);
    }
}
