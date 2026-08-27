//! star-adventurer-gti's [`ConfigurableDriver`] implementation.
//!
//! The generic `config.get` / `config.apply` / `config.schema` action dispatch
//! the mount device delegates to (alongside its existing `ApParkAction` vendor
//! actions) lives in [`rusty_photon_driver`]; this module supplies only what
//! varies for the `GTi` — its `Config`, validation, secrets, and editability tiers.
//!
//! The mount's parse-don't-validate config types (`FlipRangeHours`, `MinAltitudeDegrees`,
//! the `Usb|Udp` transport enum, the custom-serde `ApPark`, …) self-validate at
//! **deserialize** time, so a bad submission fails with `ApplyError::Parse`
//! before `validate` runs. `Overrides = ()`: the CLI transport/server-port
//! overrides all target fields the UI renders **read-only**, so there is nothing
//! to override-pin. See [`docs/services/star-adventurer-gti.md`] "Config Actions".
//!
//! [`docs/services/star-adventurer-gti.md`]: ../../../docs/services/star-adventurer-gti.md

use rusty_photon_config::actions::{ConfigurableDriver, FieldError};

use crate::config::Config;

/// Driver marker wiring the mount's `Config` into the generic protocol.
pub struct StarAdvDriver;

impl ConfigurableDriver for StarAdvDriver {
    type Config = Config;
    /// CLI overrides (`--transport`/`--port`/`--baud`/`--server-port`) all target
    /// read-only fields, so there is nothing to override-pin.
    type Overrides = ();

    fn normalize(_config: &mut Config) {}

    fn validate(config: &Config) -> Vec<FieldError> {
        // The typed config self-validates at deserialize (parse-don't-validate
        // newtypes), so only the minted identity needs a domain check here.
        let mut errors = Vec::new();
        if config.mount.unique_id.trim().is_empty() {
            errors.push(FieldError {
                path: "mount.unique_id".to_string(),
                msg: "must not be empty (it is the device's stable ASCOM UniqueID)".to_string(),
            });
        }
        errors
    }

    fn secret_pointers() -> &'static [&'static str] {
        &["/server/auth/password_hash"]
    }

    fn override_paths(_overrides: &()) -> Vec<String> {
        Vec::new()
    }

    fn apply_overrides(_config: &mut Config, _overrides: &()) {}

    fn locked_paths() -> &'static [&'static str] {
        &["mount.unique_id"]
    }

    /// The `transport` block (a `Usb|Udp` tagged enum) is rendered read-only —
    /// changing the transport from the web form is an escape-hatch best done in
    /// the config file. `server.port` and `mount.enabled` are self-lockout fields.
    fn read_only_paths() -> &'static [&'static str] {
        &[
            "transport.kind",
            "transport.port",
            "transport.address",
            "transport.baud_rate",
            "transport.command_timeout",
            "transport.polling_interval",
            "server.port",
            "mount.enabled",
        ]
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_empty_unique_id() {
        let config = Config::default(); // unique_id empty by default
        assert!(StarAdvDriver::validate(&config)
            .iter()
            .any(|e| e.path == "mount.unique_id"));
    }

    #[test]
    fn validate_accepts_populated_unique_id() {
        let mut config = Config::default();
        config.mount.unique_id = "star-adv-id".to_string();
        assert_eq!(
            StarAdvDriver::validate(&config),
            Vec::<rusty_photon_config::actions::FieldError>::new()
        );
    }

    #[test]
    fn editability_tiers() {
        assert_eq!(StarAdvDriver::locked_paths(), &["mount.unique_id"]);
        assert!(StarAdvDriver::read_only_paths().contains(&"transport.kind"));
        assert!(StarAdvDriver::read_only_paths().contains(&"server.port"));
        assert!(StarAdvDriver::read_only_paths().contains(&"mount.enabled"));
    }
}
