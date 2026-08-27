//! pa-scops-oag's [`ConfigurableDriver`] implementation — the driver-specific
//! half of the `config.get` / `config.apply` / `config.schema` protocol.
//!
//! The generic machinery lives in [`rusty_photon_config::actions`]; this module
//! supplies only the Scops OAG's `Config` type, normalization, validation,
//! secret location, CLI overrides, and editability tiers. See
//! [`docs/services/pa-scops-oag.md`] "Config actions".
//!
//! [`docs/services/pa-scops-oag.md`]: ../../../docs/services/pa-scops-oag.md

use rusty_photon_config::actions::{ConfigurableDriver, FieldError};

use crate::config::{CliOverrides, Config};

/// Re-exported so tests can name the redaction sentinel.
pub use rusty_photon_config::actions::REDACTED;

/// Driver marker wiring the Scops OAG's `Config` into the generic config-action
/// protocol via [`rusty_photon_config::actions`].
pub struct ScopsFocuserDriver;

impl ConfigurableDriver for ScopsFocuserDriver {
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

    fn secret_pointers() -> &'static [&'static str] {
        &["/server/auth/password_hash"]
    }

    fn override_paths(overrides: &CliOverrides) -> Vec<String> {
        overrides.pinned_paths()
    }

    fn apply_overrides(config: &mut Config, overrides: &CliOverrides) {
        overrides.apply(config);
    }

    /// The device owns and generates its ASCOM `UniqueID`; editing it is an
    /// escape hatch for a misbehaving driver, not routine configuration.
    fn locked_paths() -> &'static [&'static str] {
        &["focuser.unique_id"]
    }

    /// `server.port` (the BFF can't follow a rebind) and `focuser.enabled`
    /// (disabling tears down the device the config actions live on) are
    /// self-lockout fields.
    fn read_only_paths() -> &'static [&'static str] {
        &["server.port", "focuser.enabled"]
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
                unique_id: "scops-test-id".to_string(),
                ..FocuserConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn validate_accepts_populated_config() {
        assert_eq!(
            ScopsFocuserDriver::validate(&valid_config()),
            Vec::<rusty_photon_config::actions::FieldError>::new()
        );
    }

    #[test]
    fn validate_rejects_empty_unique_id() {
        let config = Config::default(); // unique_id empty by default
        let errors = ScopsFocuserDriver::validate(&config);
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
        let paths: Vec<String> = ScopsFocuserDriver::validate(&config)
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
                port: "  /dev/ttyUSB0\n".to_string(),
                ..SerialConfig::default()
            },
            ..Config::default()
        };
        ScopsFocuserDriver::normalize(&mut config);
        assert_eq!(config.serial.port, "/dev/ttyUSB0");
    }

    #[test]
    fn editability_tiers_match_self_lockout_intent() {
        assert_eq!(ScopsFocuserDriver::locked_paths(), &["focuser.unique_id"]);
        assert_eq!(
            ScopsFocuserDriver::read_only_paths(),
            &["server.port", "focuser.enabled"]
        );
    }
}
