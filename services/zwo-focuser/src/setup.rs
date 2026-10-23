//! zwo-focuser's [`ConfigurableDriver`] implementation — the driver-specific
//! half behind its setup pages.
//!
//! The pages themselves (the render, the save, the origin check, the
//! Security section, the reload) are `rusty-photon-setup`'s, and each EAF's
//! page — what the driver knows about it and its `devices` override's name
//! and description — is `rusty-photon-driver`'s `overrides` module, which
//! `lib.rs` feeds the EAFs the build registered. This module supplies what
//! is the service's own: its `Config` type, its secret, and the `--port`
//! pin. No field is locked: `UniqueID`s are derived from the EAF SDK serial,
//! not minted into config. See [`docs/services/zwo-focuser.md`] "Setup
//! pages".
//!
//! [`docs/services/zwo-focuser.md`]: ../../../docs/services/zwo-focuser.md

use rusty_photon_config::actions::{ConfigurableDriver, FieldError};

use crate::config::{CliOverrides, Config};

/// Driver marker wiring zwo-focuser's [`Config`] into the shared setup pages.
pub struct ZwoFocuserDriver;

impl ConfigurableDriver for ZwoFocuserDriver {
    type Config = Config;
    type Overrides = CliOverrides;

    fn normalize(_config: &mut Config) {}

    /// Nothing to check: the overrides are free-form name/description
    /// strings, and the server block's rules are the shared form's.
    fn validate(_config: &Config) -> Vec<FieldError> {
        Vec::new()
    }

    /// The one secret: the server-auth password hash, which the pages'
    /// Security section edits and never shows. `TlsConfig` stores file
    /// *paths*, not key material.
    fn secret_pointers() -> &'static [&'static str] {
        &["/server/auth/password_hash"]
    }

    fn override_paths(overrides: &CliOverrides) -> Vec<String> {
        overrides.pinned_paths()
    }

    fn apply_overrides(config: &mut Config, overrides: &CliOverrides) {
        overrides.apply(config);
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
            Vec::<FieldError>::new()
        );
    }

    #[test]
    fn no_locked_identity_fields() {
        assert_eq!(ZwoFocuserDriver::locked_paths(), Vec::<&str>::new());
    }

    #[test]
    fn only_the_password_hash_is_secret() {
        assert_eq!(
            ZwoFocuserDriver::secret_pointers(),
            &["/server/auth/password_hash"]
        );
    }

    #[test]
    fn port_override_is_pinned_and_applied() {
        let overrides = CliOverrides { port: Some(12321) };
        assert_eq!(
            ZwoFocuserDriver::pinned_paths(&Config::default(), &overrides),
            vec!["server.port".to_string()]
        );
        let mut config = Config::default();
        ZwoFocuserDriver::apply_overrides(&mut config, &overrides);
        assert_eq!(config.server.port, 12321);
    }
}
