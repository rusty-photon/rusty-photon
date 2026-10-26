//! pa-falcon-rotator's [`ConfigurableDriver`] implementation — the
//! driver-specific half behind its setup pages.
//!
//! The pages themselves (the render, the save, the origin check, the
//! Security section, the reload) are `rusty-photon-setup`'s; this module
//! supplies only what varies for the Falcon: its `Config` type,
//! normalization, validation, its one secret, its command-line overrides, and
//! which config section each of its two devices' pages edits. See
//! [`docs/services/falcon-rotator.md`] "Setup pages".
//!
//! [`docs/services/falcon-rotator.md`]: ../../../docs/services/falcon-rotator.md

use rusty_photon_config::driver::{ConfigurableDriver, DeviceSection, FieldError};

use crate::config::{CliOverrides, Config};

/// The rotator's device type in its Alpaca path spelling.
pub const ROTATOR_DEVICE_TYPE: &str = "rotator";
/// The status switch's device type in its Alpaca path spelling.
pub const SWITCH_DEVICE_TYPE: &str = "switch";

/// Driver marker wiring the Falcon's full `Config` into the shared setup pages.
pub struct FalconRotatorDriver;

impl ConfigurableDriver for FalconRotatorDriver {
    type Config = Config;
    type Overrides = CliOverrides;

    fn normalize(config: &mut Config) {
        let trimmed = config.serial.port.trim();
        if trimmed.len() != config.serial.port.len() {
            config.serial.port = trimmed.to_string();
        }
    }

    fn validate(config: &Config) -> Vec<FieldError> {
        let mut errors = Vec::new();
        if config.serial.port.trim().is_empty() {
            errors.push(FieldError {
                path: "serial.port".to_string(),
                msg: "must not be empty".to_string(),
            });
        }
        if config.serial.baud_rate == 0 {
            errors.push(FieldError {
                path: "serial.baud_rate".to_string(),
                msg: "must be greater than 0".to_string(),
            });
        }
        if config.serial.timeout.is_zero() {
            errors.push(FieldError {
                path: "serial.timeout".to_string(),
                msg: "must be greater than 0".to_string(),
            });
        }
        for (path, id) in [
            ("rotator.unique_id", &config.rotator.unique_id),
            ("switch.unique_id", &config.switch.unique_id),
        ] {
            if id.trim().is_empty() {
                errors.push(FieldError {
                    path: path.to_string(),
                    msg: "must not be empty (it is the device's stable ASCOM UniqueID)".to_string(),
                });
            }
        }
        errors
    }

    /// The one secret: the server-auth password hash, which the pages'
    /// Security section edits and never shows.
    fn secret_pointers() -> &'static [&'static str] {
        &["/server/auth/password_hash"]
    }

    fn override_paths(overrides: &CliOverrides) -> Vec<String> {
        overrides.pinned_paths()
    }

    fn apply_overrides(config: &mut Config, overrides: &CliOverrides) {
        overrides.apply(config);
    }

    /// The rotator's page edits the `rotator` section and the status
    /// switch's page the `switch` section; the server page holds `serial`
    /// and `server`.
    fn device_sections(config: &Config) -> Vec<DeviceSection> {
        vec![
            DeviceSection {
                device_type: ROTATOR_DEVICE_TYPE.to_string(),
                device_number: 0,
                pointer: "/rotator".to_string(),
                name: config.rotator.name.clone(),
                enabled: config.rotator.enabled,
            },
            DeviceSection {
                device_type: SWITCH_DEVICE_TYPE.to_string(),
                device_number: 0,
                pointer: "/switch".to_string(),
                name: config.switch.name.clone(),
                enabled: config.switch.enabled,
            },
        ]
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::{Config, RotatorConfig, SerialConfig, SwitchConfig};
    use std::time::Duration;

    fn valid_config() -> Config {
        Config {
            rotator: RotatorConfig {
                unique_id: "rotator-id".to_string(),
                ..RotatorConfig::default()
            },
            switch: SwitchConfig {
                unique_id: "switch-id".to_string(),
                ..SwitchConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn validate_accepts_populated_config() {
        assert_eq!(
            FalconRotatorDriver::validate(&valid_config()),
            Vec::<FieldError>::new()
        );
    }

    #[test]
    fn validate_rejects_both_empty_unique_ids() {
        let errors = FalconRotatorDriver::validate(&Config::default());
        let paths: Vec<String> = errors.into_iter().map(|e| e.path).collect();
        assert!(paths.contains(&"rotator.unique_id".to_string()));
        assert!(paths.contains(&"switch.unique_id".to_string()));
    }

    #[test]
    fn validate_flags_bad_serial_fields() {
        let config = Config {
            serial: SerialConfig {
                port: "  ".to_string(),
                baud_rate: 0,
                timeout: Duration::ZERO,
            },
            ..valid_config()
        };
        let paths: Vec<String> = FalconRotatorDriver::validate(&config)
            .into_iter()
            .map(|e| e.path)
            .collect();
        for expected in ["serial.port", "serial.baud_rate", "serial.timeout"] {
            assert!(paths.contains(&expected.to_string()), "missing {expected}");
        }
    }

    #[test]
    fn each_device_page_edits_its_own_section() {
        let mut config = valid_config();
        config.rotator.name = "Imaging rotator".to_string();
        config.switch.enabled = false;
        let sections = FalconRotatorDriver::device_sections(&config);
        assert_eq!(
            sections,
            vec![
                DeviceSection {
                    device_type: "rotator".to_string(),
                    device_number: 0,
                    pointer: "/rotator".to_string(),
                    name: "Imaging rotator".to_string(),
                    enabled: true,
                },
                DeviceSection {
                    device_type: "switch".to_string(),
                    device_number: 0,
                    pointer: "/switch".to_string(),
                    name: config.switch.name,
                    enabled: false,
                },
            ]
        );
    }

    #[test]
    fn the_pages_secret_is_the_auth_hash_alone() {
        assert_eq!(
            FalconRotatorDriver::secret_pointers(),
            &["/server/auth/password_hash"]
        );
    }
}
