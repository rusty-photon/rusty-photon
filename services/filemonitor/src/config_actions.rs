//! filemonitor's [`ConfigurableDriver`] implementation — the driver-specific
//! half of the `config.get` / `config.apply` / `config.schema` protocol.
//!
//! The generic machinery lives in [`rusty_photon_config::actions`]; this module
//! supplies only what varies for filemonitor: its `Config` type, normalization,
//! validation, secret location, and editability tiers. Single ASCOM device (the
//! `SafetyMonitor`); the binary has no CLI overrides beyond `--config`, so
//! `Overrides = ()`. See [`docs/services/filemonitor.md`] "Config actions".
//!
//! [`docs/services/filemonitor.md`]: ../../../docs/services/filemonitor.md

use rusty_photon_config::actions::{ConfigurableDriver, FieldError};

use crate::{Config, RuleType};

/// Re-exported so tests can name the redaction sentinel.
pub use rusty_photon_config::actions::REDACTED;

/// Driver marker wiring filemonitor's `Config` into the generic config-action
/// protocol via [`rusty_photon_config::actions`].
pub struct FileMonitorDriver;

impl ConfigurableDriver for FileMonitorDriver {
    type Config = Config;
    /// No CLI overrides — the binary only takes `--config` / `--log-level`.
    type Overrides = ();

    fn normalize(_config: &mut Config) {}

    fn validate(config: &Config) -> Vec<FieldError> {
        let mut errors = Vec::new();
        if config.device.unique_id.trim().is_empty() {
            errors.push(FieldError {
                path: "device.unique_id".to_string(),
                msg: "must not be empty (it is the device's stable ASCOM UniqueID)".to_string(),
            });
        }
        if config.file.path.as_os_str().is_empty() {
            errors.push(FieldError {
                path: "file.path".to_string(),
                msg: "must not be empty".to_string(),
            });
        }
        if config.file.polling_interval.is_zero() {
            errors.push(FieldError {
                path: "file.polling_interval".to_string(),
                msg: "must be greater than 0".to_string(),
            });
        }
        // A rule whose pattern doesn't compile as a regex is otherwise a
        // silent no-match at evaluation time (see `evaluate_safety`) — reject
        // it at the config boundary instead so a typo doesn't quietly disable
        // a safety rule.
        for (i, rule) in config.parsing.rules.iter().enumerate() {
            if matches!(rule.rule_type, RuleType::Regex) {
                if let Err(e) = regex::Regex::new(&rule.pattern) {
                    errors.push(FieldError {
                        path: format!("parsing.rules.{i}.pattern"),
                        msg: format!("invalid regex: {e}"),
                    });
                }
            }
        }
        errors
    }

    /// filemonitor's one secret: the server-auth password hash. `TlsConfig`
    /// stores file *paths*, not key material, so there is nothing to redact
    /// there.
    fn secret_pointers() -> &'static [&'static str] {
        &["/server/auth/password_hash"]
    }

    fn override_paths(_overrides: &()) -> Vec<String> {
        Vec::new()
    }

    fn apply_overrides(_config: &mut Config, _overrides: &()) {}

    /// The device owns its ASCOM `UniqueID`; editing it is an escape hatch for
    /// a misbehaving driver, not routine configuration.
    fn locked_paths() -> &'static [&'static str] {
        &["device.unique_id"]
    }

    /// `server.port` is a self-lockout field: the BFF can't follow a rebind to
    /// a new port.
    fn read_only_paths() -> &'static [&'static str] {
        &["server.port"]
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::{AlpacaServerConfig, DeviceConfig, FileConfig, ParsingConfig, ParsingRule};
    use std::path::PathBuf;
    use std::time::Duration;

    fn valid_config() -> Config {
        Config {
            device: DeviceConfig {
                name: "Test Monitor".to_string(),
                unique_id: "filemonitor-test-id".to_string(),
                description: "Test".to_string(),
            },
            file: FileConfig {
                path: PathBuf::from("/tmp/RoofStatusFile.txt"),
                polling_interval: Duration::from_mins(1),
            },
            parsing: ParsingConfig {
                rules: vec![ParsingRule {
                    rule_type: RuleType::Contains,
                    pattern: "OPEN".to_string(),
                    safe: false,
                }],
                case_sensitive: false,
            },
            server: AlpacaServerConfig::new(11111),
        }
    }

    #[test]
    fn validate_accepts_populated_config() {
        assert_eq!(
            FileMonitorDriver::validate(&valid_config()),
            Vec::<rusty_photon_config::actions::FieldError>::new()
        );
    }

    #[test]
    fn validate_rejects_empty_unique_id() {
        let mut config = valid_config();
        config.device.unique_id = "  ".to_string();
        let errors = FileMonitorDriver::validate(&config);
        assert!(errors.iter().any(|e| e.path == "device.unique_id"));
    }

    #[test]
    fn validate_rejects_empty_file_path() {
        let mut config = valid_config();
        config.file.path = PathBuf::new();
        let errors = FileMonitorDriver::validate(&config);
        assert!(errors.iter().any(|e| e.path == "file.path"));
    }

    #[test]
    fn validate_rejects_zero_polling_interval() {
        let mut config = valid_config();
        config.file.polling_interval = Duration::ZERO;
        let errors = FileMonitorDriver::validate(&config);
        assert!(errors.iter().any(|e| e.path == "file.polling_interval"));
    }

    #[test]
    fn validate_rejects_invalid_regex_pattern() {
        let mut config = valid_config();
        config.parsing.rules.push(ParsingRule {
            rule_type: RuleType::Regex,
            pattern: "(unclosed".to_string(),
            safe: true,
        });
        let errors = FileMonitorDriver::validate(&config);
        // Index 1: valid_config() already has one rule at index 0.
        assert!(errors.iter().any(|e| e.path == "parsing.rules.1.pattern"));
    }

    #[test]
    fn validate_accepts_valid_regex_pattern() {
        let mut config = valid_config();
        config.parsing.rules.push(ParsingRule {
            rule_type: RuleType::Regex,
            pattern: r"Status:\s*(SAFE|OK)".to_string(),
            safe: true,
        });
        assert_eq!(
            FileMonitorDriver::validate(&config),
            Vec::<rusty_photon_config::actions::FieldError>::new()
        );
    }

    #[test]
    fn validate_flags_each_bad_field() {
        let mut config = valid_config();
        config.device.unique_id = String::new();
        config.file.path = PathBuf::new();
        config.file.polling_interval = Duration::ZERO;
        let paths: Vec<String> = FileMonitorDriver::validate(&config)
            .into_iter()
            .map(|e| e.path)
            .collect();
        for expected in ["device.unique_id", "file.path", "file.polling_interval"] {
            assert!(paths.contains(&expected.to_string()), "missing {expected}");
        }
    }

    #[test]
    fn editability_tiers_and_secrets() {
        assert_eq!(FileMonitorDriver::locked_paths(), &["device.unique_id"]);
        assert_eq!(FileMonitorDriver::read_only_paths(), &["server.port"]);
        assert_eq!(
            FileMonitorDriver::secret_pointers(),
            &["/server/auth/password_hash"]
        );
    }
}
