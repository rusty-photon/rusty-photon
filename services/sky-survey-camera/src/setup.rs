//! sky-survey-camera's [`ConfigurableDriver`] implementation — the
//! driver-specific half behind its setup pages.
//!
//! The pages themselves (the render, the save, the origin check, the
//! Security section, the reload) are `rusty-photon-setup`'s; this module
//! supplies only what varies for the camera: its `Config` type, validation,
//! its secrets, and which config section the camera's page edits. The
//! binary has no command-line overrides beyond `--config` and `--log-level`,
//! so `Overrides = ()` and no field is ever pinned. See
//! [`docs/services/sky-survey-camera.md`] "Setup pages".
//!
//! [`docs/services/sky-survey-camera.md`]: ../../../docs/services/sky-survey-camera.md

use rusty_photon_config::actions::{ConfigurableDriver, DeviceSection, FieldError};

use crate::config::Config;

/// The camera's device type in its Alpaca path spelling.
pub const DEVICE_TYPE: &str = "camera";

/// Driver marker wiring the camera's `Config` into the shared setup pages.
pub struct SkySurveyCameraDriver;

impl ConfigurableDriver for SkySurveyCameraDriver {
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

    /// The inbound Basic-Auth hash, which the pages' Security section edits
    /// and never shows, and the follow-mode client passwords. The follow-mode
    /// blocks (`pointing.telescope`, `pointing.rotator`) are optional
    /// objects, which the pages name as edited in the file rather than
    /// render, so those two passwords reach no page today; the pointers
    /// stay so a page that one day renders the blocks hides them.
    fn secret_pointers() -> &'static [&'static str] {
        &[
            "/pointing/telescope/auth/password",
            "/pointing/rotator/auth/password",
            "/server/auth/password_hash",
        ]
    }

    fn override_paths((): &()) -> Vec<String> {
        Vec::new()
    }

    fn apply_overrides(_config: &mut Config, (): &()) {}

    /// The camera's page edits the `device` section; the server page holds
    /// `optics`, `pointing`, `survey` and `server`.
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
            Vec::<FieldError>::new()
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
    fn the_cameras_page_edits_the_device_section() {
        let mut config = base();
        config.device.name = "Survey camera".to_string();
        assert_eq!(
            SkySurveyCameraDriver::device_sections(&config),
            vec![DeviceSection {
                device_type: "camera".to_string(),
                device_number: 0,
                pointer: "/device".to_string(),
                name: "Survey camera".to_string(),
                enabled: true,
            }]
        );
    }

    #[test]
    fn the_secrets_are_the_auth_hash_and_the_follow_mode_passwords() {
        assert_eq!(
            SkySurveyCameraDriver::secret_pointers(),
            &[
                "/pointing/telescope/auth/password",
                "/pointing/rotator/auth/password",
                "/server/auth/password_hash"
            ]
        );
        assert_eq!(
            SkySurveyCameraDriver::pinned_paths(&base(), &()),
            Vec::<String>::new()
        );
    }
}
