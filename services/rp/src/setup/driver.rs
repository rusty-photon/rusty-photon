//! rp's [`ConfigurableDriver`] implementation — the service-specific half
//! behind its setup page ([`crate::setup`]).
//!
//! rp has no in-process reload (`main.rs` runs `ServiceRunner::run`, not
//! `run_with_reload`), so its
//! [`save_disposition`](ConfigurableDriver::save_disposition) is
//! [`SaveDisposition::Restart`]: every persisted change waits for the next
//! rp start, which the saved page says.

use rusty_photon_config::driver::{ConfigurableDriver, FieldError, SaveDisposition};

use crate::config::{validate_config, Config};

/// Marker wiring rp's [`Config`] into its setup page.
pub struct RpConfigDriver;

impl ConfigurableDriver for RpConfigDriver {
    type Config = Config;
    /// rp's `serve` carries no config-overriding CLI flags — `--config`
    /// names the file and `--log-level` is not config — so nothing is
    /// override-pinned.
    type Overrides = ();

    fn normalize(_config: &mut Config) {}

    /// Reuses the exact validation `load_config` runs at startup, so a
    /// config the setup page saves is a config rp will boot from.
    fn validate(config: &Config) -> Vec<FieldError> {
        validate_config(config)
    }

    /// Every credential in rp's config:
    ///
    /// - `server.auth.password_hash` — the Argon2id hash guarding rp's own
    ///   HTTP surface;
    /// - the plaintext per-device `auth.password` rp sends as HTTP Basic
    ///   Auth to auth-enabled Alpaca drivers, the plate-solver service, and
    ///   the guider service. The equipment arrays use the `*` wildcard
    ///   (every element); `mount`, `mount.guiding`, and the top-level
    ///   `plate_solver` are singular, so their pointers are exact.
    ///
    /// `server.tls.cert` / `server.tls.key` are file *paths*, not key
    /// material, so they are deliberately not redacted.
    fn secret_pointers() -> &'static [&'static str] {
        &[
            "/server/auth/password_hash",
            "/equipment/cameras/*/auth/password",
            "/equipment/focusers/*/auth/password",
            "/equipment/filter_wheels/*/auth/password",
            "/equipment/cover_calibrators/*/auth/password",
            "/equipment/safety_monitors/*/auth/password",
            "/equipment/switches/*/auth/password",
            "/equipment/rotators/*/auth/password",
            "/equipment/observing_conditions/*/auth/password",
            "/equipment/domes/*/auth/password",
            "/equipment/mount/auth/password",
            "/equipment/mount/guiding/auth/password",
            "/plate_solver/auth/password",
        ]
    }

    fn override_paths(_overrides: &()) -> Vec<String> {
        Vec::new()
    }

    fn apply_overrides(_config: &mut Config, _overrides: &()) {}

    /// `ca_cert`, the CA rp trusts for every server it presents a stored
    /// credential to: the setup page names it as edited in the
    /// configuration file, as it does `server.tls`, so a page anyone who
    /// reaches rp can save cannot choose whom rp's stored passwords go to.
    fn file_only_pointers() -> &'static [&'static str] {
        &["/ca_cert"]
    }

    /// rp has no in-process reload: persisted changes take effect on the
    /// next rp start.
    fn save_disposition() -> SaveDisposition {
        SaveDisposition::Restart
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use serde_json::Value;

    use super::*;

    fn config_from(value: Value) -> Config {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn validate_delegates_to_shared_config_validation() {
        let config = config_from(serde_json::json!({
            "session": { "data_directory": "/tmp/rp-test" },
            "equipment": {},
            "site": { "latitude_degrees": 91.0, "longitude_degrees": 0.0 },
            "server": { "port": 0 }
        }));
        let errors = RpConfigDriver::validate(&config);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, "site.latitude_degrees");
    }

    #[test]
    fn save_disposition_is_restart() {
        assert_eq!(RpConfigDriver::save_disposition(), SaveDisposition::Restart);
    }

    /// One entry of an equipment list, presenting `password`.
    fn entry(id: &str, password: &str) -> Value {
        serde_json::json!({
            "id": id,
            "alpaca_url": "http://localhost:11120",
            "auth": { "username": "obs", "password": password }
        })
    }

    #[test]
    fn every_secret_pointer_names_a_credential_rp_stores() {
        let config = config_from(serde_json::json!({
            "session": { "data_directory": "/tmp/rp-test" },
            "equipment": {
                "cameras": [entry("main-cam", "cameras")],
                "focusers": [entry("focuser", "focusers")],
                "filter_wheels": [entry("wheel", "filter_wheels")],
                "cover_calibrators": [entry("panel", "cover_calibrators")],
                "safety_monitors": [entry("roof-safe", "safety_monitors")],
                "switches": [entry("ppba", "switches")],
                "rotators": [entry("falcon", "rotators")],
                "observing_conditions": [entry("weather", "observing_conditions")],
                "domes": [entry("roof", "domes")],
                "mount": {
                    "alpaca_url": "http://localhost:11122",
                    "auth": { "username": "obs", "password": "mount" },
                    "guiding": {
                        "url": "http://localhost:11130",
                        "auth": { "username": "obs", "password": "guiding" }
                    }
                }
            },
            "plate_solver": {
                "url": "http://localhost:11131",
                "auth": { "username": "obs", "password": "plate_solver" }
            },
            "server": {
                "port": 0,
                "auth": { "username": "obs", "password_hash": "$argon2id$real" }
            }
        }));
        let value = serde_json::to_value(&config).unwrap();
        for pattern in RpConfigDriver::secret_pointers() {
            // Each list holds one entry, so a wildcard names element 0.
            let pointer = pattern.replace("/*/", "/0/");
            assert!(
                value.pointer(&pointer).is_some_and(Value::is_string),
                "{pattern} names no credential in {value}"
            );
        }
    }
}
