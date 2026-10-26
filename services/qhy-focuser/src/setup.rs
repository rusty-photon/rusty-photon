//! qhy-focuser's [`ConfigurableDriver`] implementation — the driver-specific
//! half behind its setup pages.
//!
//! The pages themselves (the render, the save, the origin check, the
//! Security section, the reload) are `rusty-photon-setup`'s; this module
//! supplies only what varies for the Q-Focuser: its `Config` type,
//! normalization, validation, its one secret, its command-line overrides, and
//! which config section the focuser's page edits. See
//! [`docs/services/qhy-focuser.md`] "Setup pages".
//!
//! [`docs/services/qhy-focuser.md`]: ../../../docs/services/qhy-focuser.md

use rusty_photon_config::driver::{ConfigurableDriver, DeviceSection, FieldError};

use crate::config::{CliOverrides, Config};

/// The focuser's device type in its Alpaca path spelling.
pub const DEVICE_TYPE: &str = "focuser";

/// Driver marker wiring the Q-Focuser's `Config` into the shared setup pages.
pub struct QhyFocuserDriver;

impl ConfigurableDriver for QhyFocuserDriver {
    type Config = Config;
    type Overrides = CliOverrides;

    /// Trim surrounding whitespace from `serial.port`, which is otherwise opened
    /// verbatim at runtime.
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
        if config.serial.polling_interval.is_zero() {
            errors.push(FieldError {
                path: "serial.polling_interval".to_string(),
                msg: "must be greater than 0".to_string(),
            });
        }
        if config.serial.timeout.is_zero() {
            errors.push(FieldError {
                path: "serial.timeout".to_string(),
                msg: "must be greater than 0".to_string(),
            });
        }
        if config.focuser.max_step == 0 {
            errors.push(FieldError {
                path: "focuser.max_step".to_string(),
                msg: "must be greater than 0".to_string(),
            });
        }
        if config.focuser.unique_id.trim().is_empty() {
            errors.push(FieldError {
                path: "focuser.unique_id".to_string(),
                msg: "must not be empty (it is the device's stable ASCOM UniqueID)".to_string(),
            });
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

    /// The focuser's page edits the `focuser` section; the server page holds
    /// `serial` and `server`.
    fn device_sections(config: &Config) -> Vec<DeviceSection> {
        vec![DeviceSection {
            device_type: DEVICE_TYPE.to_string(),
            device_number: 0,
            pointer: "/focuser".to_string(),
            name: config.focuser.name.clone(),
            enabled: config.focuser.enabled,
        }]
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::{Config, FocuserConfig, SerialConfig};
    use std::time::Duration;

    fn valid_config() -> Config {
        Config {
            focuser: FocuserConfig {
                unique_id: "qhy-focuser-test-id".to_string(),
                ..FocuserConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn validate_accepts_populated_config() {
        assert_eq!(
            QhyFocuserDriver::validate(&valid_config()),
            Vec::<FieldError>::new()
        );
    }

    #[test]
    fn validate_rejects_empty_unique_id() {
        let config = Config::default(); // unique_id empty by default
        let errors = QhyFocuserDriver::validate(&config);
        assert!(errors.iter().any(|e| e.path == "focuser.unique_id"));
    }

    #[test]
    fn validate_flags_each_bad_field() {
        let config = Config {
            serial: SerialConfig {
                port: "   ".to_string(),
                baud_rate: 0,
                polling_interval: Duration::ZERO,
                timeout: Duration::ZERO,
            },
            focuser: FocuserConfig {
                max_step: 0,
                unique_id: "id".to_string(),
                ..FocuserConfig::default()
            },
            ..Config::default()
        };
        let paths: Vec<String> = QhyFocuserDriver::validate(&config)
            .into_iter()
            .map(|e| e.path)
            .collect();
        for expected in [
            "serial.port",
            "serial.baud_rate",
            "serial.polling_interval",
            "serial.timeout",
            "focuser.max_step",
        ] {
            assert!(paths.contains(&expected.to_string()), "missing {expected}");
        }
    }

    #[test]
    fn normalize_trims_serial_port() {
        let mut config = Config {
            serial: SerialConfig {
                port: "  /dev/ttyACM0\n".to_string(),
                ..SerialConfig::default()
            },
            ..Config::default()
        };
        QhyFocuserDriver::normalize(&mut config);
        assert_eq!(config.serial.port, "/dev/ttyACM0");
    }

    #[test]
    fn the_focusers_page_edits_its_own_section() {
        let mut config = valid_config();
        config.focuser.name = "Main focuser".to_string();
        assert_eq!(
            QhyFocuserDriver::device_sections(&config),
            vec![DeviceSection {
                device_type: "focuser".to_string(),
                device_number: 0,
                pointer: "/focuser".to_string(),
                name: "Main focuser".to_string(),
                enabled: true,
            }]
        );
        config.focuser.enabled = false;
        assert!(!QhyFocuserDriver::device_sections(&config)[0].enabled);
    }

    #[test]
    fn the_pages_secret_is_the_auth_hash_alone() {
        assert_eq!(
            QhyFocuserDriver::secret_pointers(),
            &["/server/auth/password_hash"]
        );
    }
}
