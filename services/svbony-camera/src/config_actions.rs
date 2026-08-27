//! `ConfigurableDriver` impl for svbony-camera — wires the cross-driver
//! `config.get` / `config.apply` / `config.schema` protocol
//! (`docs/services/config-actions.md`) to this service's [`Config`].
//!
//! Editability tiers (mirrors `zwo-camera`):
//! - **Locked (identity):** none. ASCOM `UniqueID`s are derived from the
//!   camera SDK serial (see `docs/services/svbony-camera.md` "Device
//!   identity"), not minted into config, so there is no identity field to
//!   lock.
//! - **Hard read-only:** `server.port` (a BFF could not follow the rebind).
//! - **Editable:** the per-serial `devices` map (`name` / `description`).

use rusty_photon_config::actions::{ConfigurableDriver, FieldError};

use crate::config::{CliOverrides, Config};

/// Zero-sized marker implementing [`ConfigurableDriver`] for the
/// svbony-camera [`Config`]. The generic `rusty_photon_driver::dispatch`
/// routes the three config actions against this.
pub struct SvbonyCameraDriver;

impl ConfigurableDriver for SvbonyCameraDriver {
    type Config = Config;
    type Overrides = CliOverrides;

    fn normalize(_config: &mut Config) {}

    /// Nothing to validate in v0: the per-serial overrides are free-form
    /// name/description strings.
    fn validate(_config: &Config) -> Vec<FieldError> {
        Vec::new()
    }

    /// The one secret: the server-auth password hash. `TlsConfig` stores file
    /// *paths*, not key material, so there is nothing to redact there.
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
    use crate::config::DeviceOverride;

    #[test]
    fn valid_config_has_no_errors() {
        let mut config = Config::default();
        config.devices.insert(
            "SVB0123456789AB".to_string(),
            DeviceOverride {
                name: Some("Main".to_string()),
                description: Some("desc".to_string()),
            },
        );
        assert_eq!(
            SvbonyCameraDriver::validate(&config),
            Vec::<rusty_photon_config::actions::FieldError>::new()
        );
    }

    #[test]
    fn no_locked_identity_fields() {
        assert_eq!(SvbonyCameraDriver::locked_paths(), Vec::<&str>::new());
    }

    #[test]
    fn port_is_read_only() {
        assert_eq!(SvbonyCameraDriver::read_only_paths(), &["server.port"]);
    }

    #[test]
    fn port_override_is_pinned_and_applied() {
        let overrides = CliOverrides { port: Some(12321) };
        assert_eq!(
            SvbonyCameraDriver::override_paths(&overrides),
            vec!["server.port".to_string()]
        );
        let mut config = Config::default();
        SvbonyCameraDriver::apply_overrides(&mut config, &overrides);
        assert_eq!(config.server.port, 12321);
    }
}
