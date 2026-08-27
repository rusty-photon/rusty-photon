//! rp's [`ConfigurableDriver`] implementation — the service-specific half of
//! the shared config protocol ([`rusty_photon_config::actions`]).
//!
//! Unlike the Alpaca drivers, rp is **not** an ASCOM device: the protocol is
//! consumed by the plain-REST endpoints in [`crate::routes`]
//! (`GET/PUT /api/config`, `GET /api/config/schema`) rather than by vendor
//! `Action`s. rp also has no in-process reload (`main.rs` runs
//! `ServiceRunner::run`, not `run_with_reload`), so its
//! [`apply_disposition`](ConfigurableDriver::apply_disposition) is
//! [`ApplyDisposition::Restart`]: every persisted change is classified
//! `restart_required` and takes effect on the next rp start.

use rusty_photon_config::actions::{ApplyDisposition, ConfigurableDriver, FieldError};

use crate::config::{validate_config, Config};

/// Re-exported so routes and tests can name the redaction sentinel without
/// reaching across crates.
pub use rusty_photon_config::actions::REDACTED;

/// Marker wiring rp's [`Config`] into the shared config-action protocol.
pub struct RpConfigDriver;

impl ConfigurableDriver for RpConfigDriver {
    type Config = Config;
    /// rp's `serve` carries no config-overriding CLI flags — `--config`
    /// names the file and `--log-level` is not config — so nothing is
    /// override-pinned.
    type Overrides = ();

    fn normalize(_config: &mut Config) {}

    /// Reuses the exact validation `load_config` runs at startup (site
    /// ranges + camera fields), so a config the REST endpoint accepts is a
    /// config rp will boot from.
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

    /// `server.port`: a rebind the UI could not follow — the same
    /// self-lockout reasoning as the Alpaca drivers.
    fn read_only_paths() -> &'static [&'static str] {
        &["server.port"]
    }

    /// rp has no in-process reload: persisted changes take effect on the
    /// next rp start (`restart_required[]`, status stays `"ok"`).
    fn apply_disposition() -> ApplyDisposition {
        ApplyDisposition::Restart
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use rusty_photon_config::actions::{config_get, config_schema};
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
    fn apply_disposition_is_restart() {
        assert_eq!(
            RpConfigDriver::apply_disposition(),
            ApplyDisposition::Restart
        );
    }

    #[test]
    fn schema_carries_read_only_server_port_and_no_locked_fields() {
        let resp = config_schema::<RpConfigDriver>();
        assert_eq!(resp.read_only_fields, vec!["server.port".to_string()]);
        assert_eq!(resp.locked_fields, Vec::<String>::new());
        // The schema names the top-level config sections.
        let props = resp
            .schema
            .pointer("/properties")
            .and_then(Value::as_object)
            .expect("schema has properties");
        for section in ["session", "equipment", "server", "site", "imaging"] {
            assert!(props.contains_key(section), "schema missing {section}");
        }
    }

    #[test]
    fn config_get_redacts_every_secret_shape() {
        // One of each secret location: server auth hash, a wildcard array
        // element (camera, switch, rotator, observing_conditions, dome),
        // the singular mount and mount.guiding, and the top-level
        // plate_solver block.
        let config = config_from(serde_json::json!({
            "session": { "data_directory": "/tmp/rp-test" },
            "equipment": {
                "cameras": [
                    {
                        "id": "main-cam",
                        "alpaca_url": "http://localhost:11120",
                        "auth": { "username": "obs", "password": "cam-secret" }
                    },
                    { "id": "guide-cam", "alpaca_url": "http://localhost:11121" }
                ],
                "mount": {
                    "alpaca_url": "http://localhost:11122",
                    "auth": { "username": "obs", "password": "mount-secret" },
                    "guiding": {
                        "url": "http://localhost:11130",
                        "auth": { "username": "obs", "password": "guiding-secret" }
                    }
                },
                "switches": [
                    {
                        "id": "ppba",
                        "alpaca_url": "http://localhost:11112",
                        "auth": { "username": "obs", "password": "switch-secret" }
                    }
                ],
                "rotators": [
                    {
                        "id": "falcon",
                        "alpaca_url": "http://localhost:11118",
                        "auth": { "username": "obs", "password": "rotator-secret" }
                    }
                ],
                "observing_conditions": [
                    {
                        "id": "ppba-weather",
                        "alpaca_url": "http://localhost:11112",
                        "auth": { "username": "obs", "password": "oc-secret" }
                    }
                ],
                "domes": [
                    {
                        "id": "roof",
                        "alpaca_url": "http://localhost:11119",
                        "auth": { "username": "obs", "password": "dome-secret" }
                    }
                ]
            },
            "plate_solver": {
                "url": "http://localhost:11131",
                "auth": { "username": "obs", "password": "plate-solver-secret" }
            },
            "server": {
                "port": 0,
                "auth": { "username": "obs", "password_hash": "$argon2id$real" }
            }
        }));

        let resp = config_get::<RpConfigDriver>(&config, &()).unwrap();
        for pointer in [
            "/server/auth/password_hash",
            "/equipment/cameras/0/auth/password",
            "/equipment/mount/auth/password",
            "/equipment/mount/guiding/auth/password",
            "/equipment/switches/0/auth/password",
            "/equipment/rotators/0/auth/password",
            "/equipment/observing_conditions/0/auth/password",
            "/equipment/domes/0/auth/password",
            "/plate_solver/auth/password",
        ] {
            assert_eq!(
                resp.config.pointer(pointer).and_then(Value::as_str),
                Some(REDACTED),
                "expected {pointer} redacted"
            );
        }
        // The auth-less camera stays untouched; usernames are not secrets.
        assert!(resp
            .config
            .pointer("/equipment/cameras/1/auth")
            .is_some_and(Value::is_null));
        assert_eq!(
            resp.config
                .pointer("/equipment/cameras/0/auth/username")
                .and_then(Value::as_str),
            Some("obs")
        );
        assert!(resp.overrides.is_empty(), "rp has no CLI overrides");
    }
}
