//! pa-falcon-rotator's [`ConfigurableDriver`] implementation.
//!
//! The generic `config.get` / `config.apply` / `config.schema` action dispatch
//! both devices delegate to lives in [`rusty_photon_driver`]; this module supplies
//! only what varies for the Falcon — its `Config`, validation, secrets, CLI
//! overrides, and editability tiers. See [`docs/services/falcon-rotator.md`]
//! "Config Actions".
//!
//! [`docs/services/falcon-rotator.md`]: ../../../docs/services/falcon-rotator.md

use rusty_photon_config::actions::{ConfigurableDriver, FieldError};

use crate::config::{CliOverrides, Config};

/// Driver marker wiring the Falcon's full `Config` into the generic protocol.
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

    fn secret_pointers() -> &'static [&'static str] {
        &["/server/auth/password_hash"]
    }

    fn override_paths(overrides: &CliOverrides) -> Vec<String> {
        overrides.pinned_paths()
    }

    fn apply_overrides(config: &mut Config, overrides: &CliOverrides) {
        overrides.apply(config);
    }

    fn locked_paths() -> &'static [&'static str] {
        &["rotator.unique_id", "switch.unique_id"]
    }

    fn read_only_paths() -> &'static [&'static str] {
        &["server.port", "rotator.enabled", "switch.enabled"]
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
            Vec::<rusty_photon_config::actions::FieldError>::new()
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
    fn editability_tiers_cover_both_devices() {
        assert_eq!(
            FalconRotatorDriver::locked_paths(),
            &["rotator.unique_id", "switch.unique_id"]
        );
        assert_eq!(
            FalconRotatorDriver::read_only_paths(),
            &["server.port", "rotator.enabled", "switch.enabled"]
        );
    }
}
