//! star-adventurer-gti's [`ConfigurableDriver`] implementation — the
//! driver-specific half behind its setup pages.
//!
//! The pages themselves (the render, the save, the origin check, the
//! Security section, the reload) are `rusty-photon-setup`'s; this module
//! supplies only what varies for the `GTi`: its `Config` type, its
//! normalization and validation, its one secret, which fields its
//! command-line flags pin, and which config section the mount's page edits.
//! See [`docs/services/star-adventurer-gti.md`] "Setup pages".
//!
//! The mount's parse-don't-validate config types (`MinAltitudeDegrees`, the
//! custom-serde `ApPark`, …) self-validate at **deserialize** time, so a
//! submission that breaks one of their rules is refused by the typed parse,
//! before `validate` runs, and the page names the rule above the form. What
//! `validate` adds is the transport's own rules, the minted identity, and
//! the one rule that spans two blocks.
//!
//! [`docs/services/star-adventurer-gti.md`]: ../../../docs/services/star-adventurer-gti.md

use rusty_photon_config::driver::{ConfigurableDriver, DeviceSection, FieldError};

use crate::config::{CliOverrides, Config, TransportConfig};

/// The mount's device type in its Alpaca path spelling.
pub const DEVICE_TYPE: &str = "telescope";

/// Driver marker wiring the mount's `Config` into the shared setup pages.
pub struct StarAdvDriver;

impl ConfigurableDriver for StarAdvDriver {
    type Config = Config;
    type Overrides = CliOverrides;

    /// Trim the USB path, which is otherwise opened verbatim.
    fn normalize(config: &mut Config) {
        if let TransportConfig::Usb(usb) = &mut config.transport {
            let trimmed = usb.port.trim();
            if trimmed.len() != usb.port.len() {
                usb.port = trimmed.to_string();
            }
        }
    }

    /// The transport's fields are named by their config path
    /// (`transport.port`); the page re-points each at the chosen variant's
    /// control (`transport:usb.port`).
    fn validate(config: &Config) -> Vec<FieldError> {
        let mut errors = Vec::new();
        let (command_timeout, polling_interval) = match &config.transport {
            TransportConfig::Usb(usb) => {
                if usb.port.trim().is_empty() {
                    errors.push(field("transport.port", "must not be empty"));
                }
                if usb.baud_rate == 0 {
                    errors.push(field("transport.baud_rate", "must be greater than 0"));
                }
                (usb.command_timeout, usb.polling_interval)
            }
            TransportConfig::Udp(udp) => (udp.command_timeout, udp.polling_interval),
        };
        if command_timeout.is_zero() {
            errors.push(field("transport.command_timeout", "must be greater than 0"));
        }
        if polling_interval.is_zero() {
            errors.push(field(
                "transport.polling_interval",
                "must be greater than 0",
            ));
        }
        if config.mount.unique_id.trim().is_empty() {
            errors.push(field(
                "mount.unique_id",
                "must not be empty (it is the device's stable ASCOM UniqueID)",
            ));
        }
        // An auto-flip offset outside the band where a flip is possible is
        // a setting that silently does nothing. `load_config` refuses it at
        // startup; refusing it here too keeps a save from installing what
        // startup would have rejected.
        if let Some(msg) = config.mount.auto_flip_offset_error() {
            errors.push(FieldError {
                path: "mount.auto_flip.meridian_offset_hours".to_string(),
                msg,
            });
        }
        errors
    }

    /// The one secret: the server-auth password hash, which the pages'
    /// Security section edits and never shows.
    fn secret_pointers() -> &'static [&'static str] {
        &["/server/auth/password_hash"]
    }

    /// The fields the mount writes at runtime: the park position `SetPark`
    /// captures. The mount's page names them as edited in the file rather
    /// than carrying inputs, which a form filled before such a write would
    /// send back over the device's own write.
    fn file_only_pointers() -> &'static [&'static str] {
        &["/mount/park_ra_ticks", "/mount/park_dec_ticks"]
    }

    /// Without a config the transport's kind is unknown, so this names
    /// what the flags pin on the default (USB) transport; the pages read
    /// [`pinned_paths`](Self::pinned_paths), which knows the kind.
    fn override_paths(overrides: &CliOverrides) -> Vec<String> {
        overrides.pinned_paths(&Config::default())
    }

    fn pinned_paths(config: &Config, overrides: &CliOverrides) -> Vec<String> {
        overrides.pinned_paths(config)
    }

    /// The flags fit the running config, where they were applied at start;
    /// what they can fail on is a saved config of the other transport kind,
    /// which the reload reports on its own.
    fn apply_overrides(config: &mut Config, overrides: &CliOverrides) {
        if let Err(e) = overrides.apply(config) {
            tracing::warn!(
                error = %e,
                "the command-line overrides do not fit the saved configuration"
            );
        }
    }

    /// The mount's page edits the `mount` section; the server page holds
    /// `transport` and `server`.
    fn device_sections(config: &Config) -> Vec<DeviceSection> {
        vec![DeviceSection {
            device_type: DEVICE_TYPE.to_string(),
            device_number: 0,
            pointer: "/mount".to_string(),
            name: config.mount.name.clone(),
            enabled: config.mount.enabled,
        }]
    }
}

fn field(path: &str, msg: &str) -> FieldError {
    FieldError {
        path: path.to_string(),
        msg: msg.to_string(),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::config::{UdpConfig, UsbConfig};

    fn valid_config() -> Config {
        let mut config = Config::default();
        config.mount.unique_id = "star-adv-id".to_string();
        config
    }

    fn paths(errors: &[FieldError]) -> Vec<&str> {
        errors.iter().map(|e| e.path.as_str()).collect()
    }

    #[test]
    fn validate_accepts_a_populated_config() {
        assert_eq!(
            StarAdvDriver::validate(&valid_config()),
            Vec::<FieldError>::new()
        );
    }

    #[test]
    fn validate_rejects_an_empty_unique_id() {
        let config = Config::default(); // unique_id empty by default
        assert!(paths(&StarAdvDriver::validate(&config)).contains(&"mount.unique_id"));
    }

    #[test]
    fn validate_names_the_usb_transports_fields_by_their_config_path() {
        let mut config = valid_config();
        config.transport = TransportConfig::Usb(UsbConfig {
            port: "   ".to_string(),
            baud_rate: 0,
            command_timeout: Duration::ZERO,
            polling_interval: Duration::ZERO,
        });
        let errors = StarAdvDriver::validate(&config);
        assert_eq!(
            paths(&errors),
            vec![
                "transport.port",
                "transport.baud_rate",
                "transport.command_timeout",
                "transport.polling_interval",
            ]
        );
    }

    #[test]
    fn validate_checks_the_udp_transports_durations_and_nothing_usb_has() {
        let mut config = valid_config();
        config.transport = TransportConfig::Udp(UdpConfig {
            command_timeout: Duration::ZERO,
            polling_interval: Duration::ZERO,
            ..UdpConfig::default()
        });
        assert_eq!(
            paths(&StarAdvDriver::validate(&config)),
            vec!["transport.command_timeout", "transport.polling_interval"]
        );
        config.transport = TransportConfig::Udp(UdpConfig::default());
        assert_eq!(StarAdvDriver::validate(&config), Vec::<FieldError>::new());
    }

    #[test]
    fn validate_rejects_an_auto_flip_offset_no_flip_can_happen_at() {
        // The cross-block rule `load_config` enforces at startup has to
        // hold on a save too — otherwise a regression that drops this
        // hook still passes the config tests while letting a save install
        // what startup refuses. Offset 0.95 is past the guard entry
        // (0.95 - 0.05) for the shipped zone, so the auto-flip could never
        // fire.
        let mut config = valid_config();
        config.mount.auto_flip.enabled = true;
        config.mount.auto_flip.meridian_offset_hours = 0.95;
        let errors = StarAdvDriver::validate(&config);
        let err = errors
            .iter()
            .find(|e| e.path == "mount.auto_flip.meridian_offset_hours")
            .unwrap_or_else(|| panic!("expected the offset field error, got {errors:?}"));
        assert!(
            err.msg.contains("tracking guard"),
            "error should say why the offset is unreachable, got: {msg}",
            msg = err.msg
        );
    }

    #[test]
    fn validate_accepts_an_auto_flip_offset_inside_the_reachable_band() {
        let mut config = valid_config();
        config.mount.auto_flip.enabled = true;
        config.mount.auto_flip.meridian_offset_hours = 0.3;
        assert_eq!(StarAdvDriver::validate(&config), Vec::<FieldError>::new());
    }

    #[test]
    fn normalize_trims_the_usb_path() {
        let mut config = valid_config();
        config.transport = TransportConfig::Usb(UsbConfig {
            port: "  /dev/ttyACM0\n".to_string(),
            ..UsbConfig::default()
        });
        StarAdvDriver::normalize(&mut config);
        let TransportConfig::Usb(usb) = &config.transport else {
            panic!("the transport must stay USB");
        };
        assert_eq!(usb.port, "/dev/ttyACM0");
    }

    #[test]
    fn the_mounts_page_edits_its_own_section() {
        let mut config = valid_config();
        config.mount.name = "Pier mount".to_string();
        assert_eq!(
            StarAdvDriver::device_sections(&config),
            vec![DeviceSection {
                device_type: "telescope".to_string(),
                device_number: 0,
                pointer: "/mount".to_string(),
                name: "Pier mount".to_string(),
                enabled: true,
            }]
        );
        config.mount.enabled = false;
        assert!(!StarAdvDriver::device_sections(&config)[0].enabled);
    }

    #[test]
    fn the_pages_pins_follow_the_transport_kind_in_effect() {
        let overrides = CliOverrides {
            port: Some("/dev/ttyACM9".to_string()),
            baud: Some(9600),
            ..CliOverrides::default()
        };
        let usb = valid_config();
        assert_eq!(
            StarAdvDriver::pinned_paths(&usb, &overrides),
            vec!["transport.port", "transport.baud_rate"]
        );
        let mut udp = valid_config();
        udp.transport = TransportConfig::Udp(UdpConfig::default());
        assert_eq!(
            StarAdvDriver::pinned_paths(&udp, &overrides),
            vec!["transport.address"],
            "--baud lands nowhere on UDP"
        );
        assert_eq!(
            StarAdvDriver::override_paths(&overrides),
            vec!["transport.port", "transport.baud_rate"],
            "without a config the default transport (USB) decides"
        );
    }

    #[test]
    fn apply_overrides_pins_the_running_values_and_tolerates_a_flag_that_no_longer_fits() {
        let overrides = CliOverrides {
            port: Some("/dev/ttyACM9".to_string()),
            server_port: Some(4247),
            ..CliOverrides::default()
        };
        let mut usb = valid_config();
        StarAdvDriver::apply_overrides(&mut usb, &overrides);
        let TransportConfig::Usb(transport) = &usb.transport else {
            panic!("the transport must stay USB");
        };
        assert_eq!(transport.port, "/dev/ttyACM9");
        assert_eq!(usb.server.port, 4247);
        // A saved UDP transport under a `--port` that is a serial path: the
        // page's view of the reloaded config keeps everything else the
        // flags set, and nothing panics.
        let mut udp = valid_config();
        udp.transport = TransportConfig::Udp(UdpConfig::default());
        StarAdvDriver::apply_overrides(&mut udp, &overrides);
        assert_eq!(udp.server.port, 4247);
        assert!(matches!(udp.transport, TransportConfig::Udp(_)));
    }

    #[test]
    fn the_runtime_written_fields_are_file_only_on_the_page() {
        // What `SetPark` writes; a page must not carry inputs for them.
        assert_eq!(
            StarAdvDriver::file_only_pointers(),
            &["/mount/park_ra_ticks", "/mount/park_dec_ticks"]
        );
    }

    #[test]
    fn the_pages_secret_is_the_auth_hash_alone() {
        assert_eq!(
            StarAdvDriver::secret_pointers(),
            &["/server/auth/password_hash"]
        );
    }
}
