//! `ConfigurableDriver` impl for zwo-focuser — wires the cross-driver
//! `config.get` / `config.apply` / `config.schema` protocol
//! (`docs/services/config-actions.md`) to this service's [`Config`].
//!
//! Editability tiers:
//! - **Locked (identity):** none. ASCOM `UniqueID`s are derived from the EAF
//!   SDK serial (see `docs/services/zwo-focuser.md` "Device identity"), not
//!   minted into config, so there is no identity field to lock.
//! - **Hard read-only:** `server.port` (a BFF could not follow the rebind).
//! - **Editable:** the per-serial `devices` map (`name` / `description`).
//! - **Secret:** `server.auth.password_hash` (redacted on read, carried
//!   forward on apply).

use rusty_photon_config::actions::{ConfigurableDriver, FieldError};

use crate::config::{CliOverrides, Config};

/// Zero-sized marker implementing [`ConfigurableDriver`] for the zwo-focuser
/// [`Config`]. The generic `rusty_photon_driver::dispatch` routes the three
/// config actions against this.
pub struct ZwoFocuserDriver;

impl ConfigurableDriver for ZwoFocuserDriver {
    type Config = Config;
    type Overrides = CliOverrides;

    fn normalize(_config: &mut Config) {}

    fn validate(_config: &Config) -> Vec<FieldError> {
        Vec::new()
    }

    /// The Basic-Auth credential hash is redacted on read and carried
    /// forward on apply so a round-tripped form never blanks it.
    fn secret_pointers() -> &'static [&'static str] {
        &["/server/auth/password_hash"]
    }

    fn override_paths(overrides: &CliOverrides) -> Vec<String> {
        overrides.pinned_paths()
    }

    fn apply_overrides(config: &mut Config, overrides: &CliOverrides) {
        overrides.apply(config);
    }

    // `locked_paths()` intentionally not overridden (defaults to `&[]`): the
    // hardware-derived UniqueID means there is no locked identity field.

    fn read_only_paths() -> &'static [&'static str] {
        &["server.port"]
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn valid_config_has_no_errors() {
        assert_eq!(
            ZwoFocuserDriver::validate(&Config::default()),
            Vec::<rusty_photon_config::actions::FieldError>::new()
        );
    }

    #[test]
    fn no_locked_identity_fields() {
        assert_eq!(ZwoFocuserDriver::locked_paths(), Vec::<&str>::new());
    }

    #[test]
    fn port_is_read_only() {
        assert!(ZwoFocuserDriver::read_only_paths().contains(&"server.port"));
    }

    #[test]
    fn port_override_is_pinned_and_applied() {
        let overrides = CliOverrides { port: Some(12321) };
        assert_eq!(
            ZwoFocuserDriver::override_paths(&overrides),
            vec!["server.port".to_string()]
        );
        let mut config = Config::default();
        ZwoFocuserDriver::apply_overrides(&mut config, &overrides);
        assert_eq!(config.server.port, 12321);
    }
}
