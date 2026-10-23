//! planetarium-bridge's [`ConfigurableDriver`] implementation — the
//! driver-specific half behind its setup pages.
//!
//! The pages themselves (the render, the save, the origin check, the
//! Security section, the reload) are `rusty-photon-setup`'s; this module
//! supplies only what varies for the bridge: its `Config` type, the rules a
//! save adds to the ones its newtypes enforce when the submission is parsed,
//! its two secrets, and which config section the virtual Telescope's page
//! edits. The binary has no command-line overrides beyond `--config` and
//! `--log-level`, so `Overrides = ()` and no field is ever pinned. See
//! [`docs/services/planetarium-bridge.md`] "Setup pages".
//!
//! [`docs/services/planetarium-bridge.md`]: ../../../docs/services/planetarium-bridge.md

use rusty_photon_config::actions::{ConfigurableDriver, DeviceSection, FieldError};

use crate::config::Config;
use crate::device::DEVICE_NAME;

/// The virtual Telescope's device type in its Alpaca path spelling.
pub const DEVICE_TYPE: &str = "telescope";

/// Driver marker wiring the bridge's `Config` into the shared setup pages.
pub struct PlanetariumBridgeDriver;

impl ConfigurableDriver for PlanetariumBridgeDriver {
    type Config = Config;
    /// No command-line overrides — the binary only takes `--config` /
    /// `--log-level`.
    type Overrides = ();

    fn normalize(_config: &mut Config) {}

    /// The ranges, the URL's shape and the spool bound are the config
    /// newtypes', enforced when the submission is parsed; these are the
    /// rules a page adds.
    fn validate(config: &Config) -> Vec<FieldError> {
        let mut errors = Vec::new();
        // The startup bootstrap mints an id only when the process starts,
        // so a reload would serve a cleared one as it is.
        if config.device.unique_id.trim().is_empty() {
            errors.push(FieldError {
                path: "device.unique_id".to_string(),
                msg: "must not be empty (it is the device's stable ASCOM UniqueID)".to_string(),
            });
        }
        // The replay backoff doubles from one second up to this cap, so a
        // zero cap would retry a down rp in a tight loop.
        if config.spool.replay_backoff_max.is_zero() {
            errors.push(FieldError {
                path: "spool.replay_backoff_max".to_string(),
                msg: "must be greater than 0".to_string(),
            });
        }
        errors
    }

    /// The server-auth password hash, which the pages' Security section
    /// edits and never shows, and the plaintext password the bridge presents
    /// to rp. `rp.service_auth` is an optional block the form does not
    /// render, so the page names it as edited in the configuration file;
    /// listing its password keeps it off any page that ever does.
    fn secret_pointers() -> &'static [&'static str] {
        &["/server/auth/password_hash", "/rp/service_auth/password"]
    }

    fn override_paths((): &()) -> Vec<String> {
        Vec::new()
    }

    fn apply_overrides(_config: &mut Config, (): &()) {}

    /// The virtual Telescope's page edits the `device` section; the server
    /// page holds `rp`, `server`, `site` and `spool`.
    fn device_sections(_config: &Config) -> Vec<DeviceSection> {
        vec![DeviceSection {
            device_type: DEVICE_TYPE.to_string(),
            device_number: 0,
            pointer: "/device".to_string(),
            name: DEVICE_NAME.to_string(),
            enabled: true,
        }]
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::time::Duration;

    fn valid_config() -> Config {
        let mut config = Config::default();
        config.device.unique_id = "bridge-test-id".to_string();
        config
    }

    #[test]
    fn validate_accepts_the_default_config_with_an_id() {
        assert_eq!(
            PlanetariumBridgeDriver::validate(&valid_config()),
            Vec::<FieldError>::new()
        );
    }

    #[test]
    fn validate_rejects_an_empty_unique_id() {
        for id in ["", "   ", "\t\n"] {
            let mut config = valid_config();
            config.device.unique_id = id.to_string();
            let errors = PlanetariumBridgeDriver::validate(&config);
            assert!(
                errors.iter().any(|e| e.path == "device.unique_id"),
                "{id:?}: {errors:?}"
            );
        }
    }

    #[test]
    fn validate_rejects_a_zero_replay_backoff_cap() {
        let mut config = valid_config();
        config.spool.replay_backoff_max = Duration::ZERO;
        let errors = PlanetariumBridgeDriver::validate(&config);
        let [error] = errors.as_slice() else {
            panic!("expected the one field refused, got {errors:?}");
        };
        assert_eq!(error.path, "spool.replay_backoff_max");
    }

    #[test]
    fn the_telescopes_page_edits_the_device_section() {
        assert_eq!(
            PlanetariumBridgeDriver::device_sections(&valid_config()),
            vec![DeviceSection {
                device_type: "telescope".to_string(),
                device_number: 0,
                pointer: "/device".to_string(),
                name: DEVICE_NAME.to_string(),
                enabled: true,
            }]
        );
    }

    #[test]
    fn the_pages_secrets_are_the_auth_hash_and_the_rp_password_and_nothing_is_pinned() {
        assert_eq!(
            PlanetariumBridgeDriver::secret_pointers(),
            &["/server/auth/password_hash", "/rp/service_auth/password"]
        );
        assert_eq!(
            PlanetariumBridgeDriver::override_paths(&()),
            Vec::<String>::new()
        );
        assert_eq!(
            PlanetariumBridgeDriver::pinned_paths(&valid_config(), &()),
            Vec::<String>::new()
        );
    }
}
