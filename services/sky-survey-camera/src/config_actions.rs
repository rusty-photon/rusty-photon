//! sky-survey-camera's [`ConfigurableDriver`] implementation.
//!
//! The generic `config.get` / `config.apply` / `config.schema` action dispatch
//! the camera device delegates to lives in [`rusty_photon_driver`]; this module
//! supplies only what varies for the camera — its `Config`, validation, secrets,
//! and editability tiers.
//!
//! Single ASCOM device (the camera); follow-mode telescope/rotator blocks are
//! *client* config, so their plaintext credentials are treated as secrets
//! (redacted on read, carried forward on apply). The driver has no CLI
//! overrides, so `Overrides = ()`. See [`docs/services/sky-survey-camera.md`]
//! "Config Actions".
//!
//! [`docs/services/sky-survey-camera.md`]: ../../../docs/services/sky-survey-camera.md

use rusty_photon_config::actions::{ConfigurableDriver, FieldError};

use crate::config::Config;

/// Re-exported so the camera and tests can name the redaction sentinel.
pub use rusty_photon_config::actions::REDACTED;

/// Driver marker wiring the camera's `Config` into the generic protocol.
pub struct SkySurveyCameraDriver;

impl ConfigurableDriver for SkySurveyCameraDriver {
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
        if let Some(t) = &config.pointing.telescope {
            if !t.offset_ra_arcsec.is_finite() {
                errors.push(FieldError {
                    path: "pointing.telescope.offset_ra_arcsec".to_string(),
                    msg: "must be finite".to_string(),
                });
            }
            if !t.offset_dec_arcsec.is_finite() {
                errors.push(FieldError {
                    path: "pointing.telescope.offset_dec_arcsec".to_string(),
                    msg: "must be finite".to_string(),
                });
            }
            if t.request_timeout.is_zero() {
                errors.push(FieldError {
                    path: "pointing.telescope.request_timeout".to_string(),
                    msg: "must be greater than 0".to_string(),
                });
            }
        }
        // The rotator only feeds rotation inside follow mode; reject the orphan
        // config rather than silently ignore it (mirrors `config::validate`).
        if config.pointing.rotator.is_some() && config.pointing.telescope.is_none() {
            errors.push(FieldError {
                path: "pointing.rotator".to_string(),
                msg: "requires pointing.telescope (rotator-driven rotation only applies in follow mode)"
                    .to_string(),
            });
        }
        if let Some(r) = &config.pointing.rotator {
            if r.request_timeout.is_zero() {
                errors.push(FieldError {
                    path: "pointing.rotator.request_timeout".to_string(),
                    msg: "must be greater than 0".to_string(),
                });
            }
        }
        errors
    }

    /// Follow-mode client credentials are plaintext passwords and the inbound
    /// Basic-Auth credential is a hash; redact them on read and carry them
    /// forward on apply so a round-tripped form never blanks them. Absent
    /// (`None`) blocks are simply skipped by the redaction.
    fn secret_pointers() -> &'static [&'static str] {
        &[
            "/pointing/telescope/auth/password",
            "/pointing/rotator/auth/password",
            "/server/auth/password_hash",
        ]
    }

    fn override_paths(_overrides: &()) -> Vec<String> {
        Vec::new()
    }

    fn apply_overrides(_config: &mut Config, _overrides: &()) {}

    fn locked_paths() -> &'static [&'static str] {
        &["device.unique_id"]
    }

    fn read_only_paths() -> &'static [&'static str] {
        &["server.port"]
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::{
        AlpacaServerConfig, DeviceConfig, OpticsConfig, PointingConfig, RotatorFollowConfig,
        SurveyConfig, TelescopeFollowConfig,
    };
    use std::path::PathBuf as P;
    use std::time::Duration;

    fn base() -> Config {
        Config {
            device: DeviceConfig {
                name: "cam".into(),
                unique_id: "id".into(),
                description: "d".into(),
            },
            optics: OpticsConfig {
                focal_length_mm: 1000.0,
                pixel_size_x_um: 3.76,
                pixel_size_y_um: 3.76,
                sensor_width_px: 100,
                sensor_height_px: 100,
            },
            pointing: PointingConfig {
                initial_ra_deg: 0.0,
                initial_dec_deg: 0.0,
                initial_rotation_deg: 0.0,
                telescope: None,
                rotator: None,
            },
            survey: SurveyConfig {
                name: "DSS2 Red".into(),
                request_timeout: Duration::from_secs(30),
                cache_dir: P::from("/tmp"),
                endpoint: "http://x/".into(),
            },
            server: AlpacaServerConfig::new(0),
        }
    }

    fn telescope() -> TelescopeFollowConfig {
        TelescopeFollowConfig {
            alpaca_url: "http://x/".into(),
            device_number: 0,
            offset_ra_arcsec: 0.0,
            offset_dec_arcsec: 0.0,
            request_timeout: Duration::from_secs(2),
            auth: None,
        }
    }

    #[test]
    fn validate_accepts_static_mode() {
        assert_eq!(
            SkySurveyCameraDriver::validate(&base()),
            Vec::<rusty_photon_config::actions::FieldError>::new()
        );
    }

    #[test]
    fn validate_rejects_empty_unique_id() {
        let mut c = base();
        c.device.unique_id = String::new();
        assert!(SkySurveyCameraDriver::validate(&c)
            .iter()
            .any(|e| e.path == "device.unique_id"));
    }

    #[test]
    fn validate_rejects_nan_offset_and_orphan_rotator() {
        let mut c = base();
        let mut t = telescope();
        t.offset_ra_arcsec = f64::NAN;
        c.pointing.telescope = Some(t);
        assert!(SkySurveyCameraDriver::validate(&c)
            .iter()
            .any(|e| e.path == "pointing.telescope.offset_ra_arcsec"));

        let mut c2 = base();
        c2.pointing.rotator = Some(RotatorFollowConfig {
            alpaca_url: "http://x/".into(),
            device_number: 0,
            request_timeout: Duration::from_secs(2),
            auth: None,
        });
        assert!(SkySurveyCameraDriver::validate(&c2)
            .iter()
            .any(|e| e.path == "pointing.rotator"));
    }

    #[test]
    fn editability_tiers_and_secrets() {
        assert_eq!(SkySurveyCameraDriver::locked_paths(), &["device.unique_id"]);
        assert_eq!(SkySurveyCameraDriver::read_only_paths(), &["server.port"]);
        assert_eq!(
            SkySurveyCameraDriver::secret_pointers(),
            &[
                "/pointing/telescope/auth/password",
                "/pointing/rotator/auth/password",
                "/server/auth/password_hash"
            ]
        );
    }
}
