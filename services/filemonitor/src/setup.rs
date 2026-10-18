//! filemonitor's [`ConfigurableDriver`] implementation — the driver-specific
//! half behind its setup pages.
//!
//! The pages themselves (the render, the save, the origin check, the
//! Security section, the reload) are `rusty-photon-setup`'s; this module
//! supplies only what varies for filemonitor: its `Config` type, validation,
//! its one secret, and which config section the safety monitor's page edits.
//! The binary has no command-line overrides beyond `--config` and
//! `--log-level`, so `Overrides = ()` and no field is ever pinned. See
//! [`docs/services/filemonitor.md`] "Setup pages".
//!
//! [`docs/services/filemonitor.md`]: ../../../docs/services/filemonitor.md

use rusty_photon_config::actions::{ConfigurableDriver, DeviceSection, FieldError};

use crate::{Config, RuleType};

/// The safety monitor's device type in its Alpaca path spelling.
pub const DEVICE_TYPE: &str = "safetymonitor";

/// Driver marker wiring filemonitor's `Config` into the shared setup pages.
pub struct FileMonitorDriver;

impl ConfigurableDriver for FileMonitorDriver {
    type Config = Config;
    /// No command-line overrides — the binary only takes `--config` /
    /// `--log-level`.
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
        // a safety rule. The load applies these rules too (`ConfigFile::check`),
        // so such a file never starts the service; the rules are edited in
        // the file, not on a page, so the page names the rule by its index
        // above the form when a hand edit puts one into a running service's
        // file.
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

    /// filemonitor's one secret: the server-auth password hash, which the
    /// pages' Security section edits and never shows. `TlsConfig` stores
    /// file *paths*, not key material, so there is nothing to hide there.
    fn secret_pointers() -> &'static [&'static str] {
        &["/server/auth/password_hash"]
    }

    fn override_paths((): &()) -> Vec<String> {
        Vec::new()
    }

    fn apply_overrides(_config: &mut Config, (): &()) {}

    /// The safety monitor's page edits the `device` section; the server
    /// page holds `file`, `parsing` and `server`.
    fn device_sections(config: &Config) -> Vec<DeviceSection> {
        vec![DeviceSection {
            device_type: DEVICE_TYPE.to_string(),
            device_number: 0,
            pointer: "/device".to_string(),
            name: config.device.name.clone(),
            enabled: true,
        }]
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
            Vec::<FieldError>::new()
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
            Vec::<FieldError>::new()
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
    fn the_safety_monitors_page_edits_the_device_section() {
        let mut config = valid_config();
        config.device.name = "Roof monitor".to_string();
        assert_eq!(
            FileMonitorDriver::device_sections(&config),
            vec![DeviceSection {
                device_type: "safetymonitor".to_string(),
                device_number: 0,
                pointer: "/device".to_string(),
                name: "Roof monitor".to_string(),
                enabled: true,
            }]
        );
    }

    #[test]
    fn the_pages_secret_is_the_auth_hash_alone_and_nothing_is_pinned() {
        assert_eq!(
            FileMonitorDriver::secret_pointers(),
            &["/server/auth/password_hash"]
        );
        assert_eq!(FileMonitorDriver::override_paths(&()), Vec::<String>::new());
        assert_eq!(
            FileMonitorDriver::pinned_paths(&valid_config(), &()),
            Vec::<String>::new()
        );
    }
}
