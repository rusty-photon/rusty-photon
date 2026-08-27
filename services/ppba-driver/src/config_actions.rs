//! ppba-driver's [`ConfigurableDriver`] implementation.
//!
//! The driver registers **two** ASCOM devices (Switch + `ObservingConditions`)
//! backed by one config file. The generic `config.get` / `config.apply` /
//! `config.schema` action dispatch both devices delegate to lives in
//! [`rusty_photon_driver`]; this module supplies only what varies for the PPBA —
//! its `Config`, validation, secrets, CLI overrides, and editability tiers. See
//! [`docs/services/ppba-driver.md`] "Config Actions".
//!
//! [`docs/services/ppba-driver.md`]: ../../../docs/services/ppba-driver.md

use rusty_photon_config::actions::{ConfigurableDriver, FieldError};

use crate::config::{CliOverrides, Config};

/// Driver marker wiring the PPBA's full `Config` into the generic protocol.
pub struct PpbaDriver;

impl ConfigurableDriver for PpbaDriver {
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
        if config.observingconditions.averaging_period.is_zero() {
            errors.push(FieldError {
                path: "observingconditions.averaging_period".to_string(),
                msg: "must be greater than 0".to_string(),
            });
        }
        for (path, id) in [
            ("switch.unique_id", &config.switch.unique_id),
            (
                "observingconditions.unique_id",
                &config.observingconditions.unique_id,
            ),
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
        &["switch.unique_id", "observingconditions.unique_id"]
    }

    fn read_only_paths() -> &'static [&'static str] {
        &[
            "server.port",
            "switch.enabled",
            "observingconditions.enabled",
        ]
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::{Config, ObservingConditionsConfig, SwitchConfig};

    fn valid_config() -> Config {
        Config {
            switch: SwitchConfig {
                unique_id: "switch-id".to_string(),
                ..SwitchConfig::default()
            },
            observingconditions: ObservingConditionsConfig {
                unique_id: "oc-id".to_string(),
                ..ObservingConditionsConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn validate_accepts_populated_config() {
        assert_eq!(
            PpbaDriver::validate(&valid_config()),
            Vec::<rusty_photon_config::actions::FieldError>::new()
        );
    }

    #[test]
    fn validate_rejects_both_empty_unique_ids() {
        let paths: Vec<String> = PpbaDriver::validate(&Config::default())
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert!(paths.contains(&"switch.unique_id".to_string()));
        assert!(paths.contains(&"observingconditions.unique_id".to_string()));
    }

    #[test]
    fn override_paths_cover_enable_flags() {
        let overrides = CliOverrides {
            enable_switch: Some(false),
            enable_observingconditions: Some(true),
            ..CliOverrides::default()
        };
        let paths = PpbaDriver::override_paths(&overrides);
        assert!(paths.contains(&"switch.enabled".to_string()));
        assert!(paths.contains(&"observingconditions.enabled".to_string()));
    }

    #[test]
    fn editability_tiers_cover_both_devices() {
        assert_eq!(
            PpbaDriver::locked_paths(),
            &["switch.unique_id", "observingconditions.unique_id"]
        );
        assert_eq!(
            PpbaDriver::read_only_paths(),
            &[
                "server.port",
                "switch.enabled",
                "observingconditions.enabled"
            ]
        );
    }
}
