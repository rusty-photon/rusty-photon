//! dsd-fp2's [`ConfigurableDriver`] implementation — the driver-specific half of
//! the `config.get` / `config.apply` / `config.schema` protocol.
//!
//! The generic machinery (wire envelopes, redaction, layer-aware persist, diff,
//! schema generation) lives in [`rusty_photon_config::actions`]; this module
//! supplies only what varies for the FP2: its `Config` type, normalization,
//! validation, secret location, CLI overrides, and editability tiers. See
//! [`docs/services/dsd-fp2.md`] "Config Actions".
//!
//! [`docs/services/dsd-fp2.md`]: ../../../docs/services/dsd-fp2.md

use rusty_photon_config::actions::{ConfigurableDriver, FieldError};

use crate::config::{CliOverrides, Config};
use crate::protocol::MAX_BRIGHTNESS;

/// Re-exported so `device.rs` and tests can name the redaction sentinel without
/// reaching across crates.
pub use rusty_photon_config::actions::REDACTED;

/// Driver marker wiring the FP2's `Config` into the generic config-action
/// protocol via [`rusty_photon_config::actions`].
pub struct DsdFp2Driver;

impl ConfigurableDriver for DsdFp2Driver {
    type Config = Config;
    type Overrides = CliOverrides;

    /// Trim surrounding whitespace from `serial.port`, which is otherwise opened
    /// verbatim at runtime — so `" /dev/ttyACM0 "` would become an invalid path
    /// despite passing the non-empty check.
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
        if config.cover_calibrator.max_brightness > u32::from(MAX_BRIGHTNESS) {
            errors.push(FieldError {
                path: "cover_calibrator.max_brightness".to_string(),
                msg: format!("must be <= {MAX_BRIGHTNESS} (hardware ceiling)"),
            });
        }
        if config.cover_calibrator.min_brightness > config.cover_calibrator.max_brightness {
            errors.push(FieldError {
                path: "cover_calibrator.min_brightness".to_string(),
                msg: "must be <= cover_calibrator.max_brightness".to_string(),
            });
        }
        if config.cover_calibrator.unique_id.trim().is_empty() {
            errors.push(FieldError {
                path: "cover_calibrator.unique_id".to_string(),
                msg: "must not be empty (it is the device's stable ASCOM UniqueID)".to_string(),
            });
        }
        errors
    }

    /// The FP2's one secret: the server-auth password hash. `TlsConfig` stores
    /// file *paths*, not key material, so there is nothing to redact there.
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
        &["cover_calibrator.unique_id"]
    }

    /// `server.port` (the BFF can't follow a rebind to a new port) and
    /// `cover_calibrator.enabled` (disabling the device tears down the very
    /// endpoint the config actions live on) are self-lockout fields.
    fn read_only_paths() -> &'static [&'static str] {
        &["server.port", "cover_calibrator.enabled"]
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::{AlpacaServerConfig, Config, CoverCalibratorConfig, SerialConfig};
    use std::time::Duration;

    /// A config that is valid for `validate()`: like `Config::default()` but with
    /// a populated `cover_calibrator.unique_id` (the default is empty because the
    /// id is minted on first run by `materialize_identity`).
    fn valid_config() -> Config {
        Config {
            cover_calibrator: CoverCalibratorConfig {
                unique_id: "dsd-fp2-test-id".to_string(),
                ..CoverCalibratorConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn validate_accepts_populated_config() {
        assert_eq!(
            DsdFp2Driver::validate(&valid_config()),
            Vec::<rusty_photon_config::actions::FieldError>::new()
        );
    }

    #[test]
    fn validate_rejects_empty_unique_id() {
        for id in ["", "   ", "\t\n"] {
            let config = Config {
                cover_calibrator: CoverCalibratorConfig {
                    unique_id: id.to_string(),
                    ..CoverCalibratorConfig::default()
                },
                ..Config::default()
            };
            let errors = DsdFp2Driver::validate(&config);
            let err = errors
                .iter()
                .find(|e| e.path == "cover_calibrator.unique_id")
                .unwrap_or_else(|| panic!("expected a unique_id error for {id:?}, got {errors:?}"));
            assert_eq!(
                err.msg,
                "must not be empty (it is the device's stable ASCOM UniqueID)"
            );
        }
        assert!(DsdFp2Driver::validate(&valid_config())
            .iter()
            .all(|e| e.path != "cover_calibrator.unique_id"));
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
        DsdFp2Driver::normalize(&mut config);
        assert_eq!(config.serial.port, "/dev/ttyACM0");
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
            server: AlpacaServerConfig::new(11119),
            cover_calibrator: CoverCalibratorConfig {
                max_brightness: 9999,
                ..CoverCalibratorConfig::default()
            },
        };
        let errors = DsdFp2Driver::validate(&config);
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"serial.port"));
        assert!(paths.contains(&"serial.baud_rate"));
        assert!(paths.contains(&"serial.polling_interval"));
        assert!(paths.contains(&"serial.timeout"));
        assert!(paths.contains(&"cover_calibrator.max_brightness"));
    }

    #[test]
    fn validate_accepts_max_brightness_at_ceiling() {
        let config = Config {
            cover_calibrator: CoverCalibratorConfig {
                max_brightness: u32::from(MAX_BRIGHTNESS),
                unique_id: "dsd-fp2-test-id".to_string(),
                ..CoverCalibratorConfig::default()
            },
            ..Config::default()
        };
        assert_eq!(
            DsdFp2Driver::validate(&config),
            Vec::<rusty_photon_config::actions::FieldError>::new()
        );
    }

    #[test]
    fn validate_rejects_min_brightness_above_max_brightness() {
        let config = Config {
            cover_calibrator: CoverCalibratorConfig {
                min_brightness: 3000,
                max_brightness: 2048,
                unique_id: "dsd-fp2-test-id".to_string(),
                ..CoverCalibratorConfig::default()
            },
            ..Config::default()
        };
        let errors = DsdFp2Driver::validate(&config);
        let err = errors
            .iter()
            .find(|e| e.path == "cover_calibrator.min_brightness")
            .unwrap_or_else(|| panic!("expected a min_brightness error, got {errors:?}"));
        assert_eq!(err.msg, "must be <= cover_calibrator.max_brightness");
    }

    #[test]
    fn validate_accepts_min_brightness_equal_to_max_brightness() {
        let config = Config {
            cover_calibrator: CoverCalibratorConfig {
                min_brightness: 2048,
                max_brightness: 2048,
                unique_id: "dsd-fp2-test-id".to_string(),
                ..CoverCalibratorConfig::default()
            },
            ..Config::default()
        };
        assert!(DsdFp2Driver::validate(&config)
            .iter()
            .all(|e| e.path != "cover_calibrator.min_brightness"));
    }

    #[test]
    fn editability_tiers_match_self_lockout_intent() {
        assert_eq!(
            DsdFp2Driver::locked_paths(),
            &["cover_calibrator.unique_id"]
        );
        assert_eq!(
            DsdFp2Driver::read_only_paths(),
            &["server.port", "cover_calibrator.enabled"]
        );
    }
}
