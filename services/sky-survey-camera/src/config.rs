use rp_auth::config::ClientAuthConfig;
pub use rusty_photon_server_config::AlpacaServerConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::SkySurveyCameraError;

/// The port allocated to sky-survey-camera (`pkg/doctor.toml`, the service
/// docs).
///
/// There is no `Config::default()` — the optics fields are mandatory,
/// so this is the value operators and the packaged catalog wire in, not
/// a serde default.
pub const DEFAULT_PORT: u16 = 11116;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub device: DeviceConfig,
    pub optics: OpticsConfig,
    pub pointing: PointingConfig,
    pub survey: SurveyConfig,
    /// HTTP server settings (the shared `rusty-photon-server-config` shape).
    pub server: AlpacaServerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    pub name: String,
    /// ASCOM `UniqueID`. Omitting it in the config file loads as an
    /// empty string here; `run` then mints a spec-compliant `UUIDv4` via
    /// [`rusty_photon_config::materialize_identity`] and persists it
    /// (`/device/unique_id`), so the next load reads the stable id.
    /// There is no `Config::default()` — optics fields are mandatory,
    /// so a missing config file stays a hard error in [`load_config`].
    #[serde(default)]
    pub unique_id: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpticsConfig {
    pub focal_length_mm: f64,
    pub pixel_size_x_um: f64,
    pub pixel_size_y_um: f64,
    pub sensor_width_px: u32,
    pub sensor_height_px: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PointingConfig {
    pub initial_ra_deg: f64,
    pub initial_dec_deg: f64,
    #[serde(default)]
    pub initial_rotation_deg: f64,
    /// When present, switches the camera into telescope-following mode:
    /// `StartExposure` reads RA/Dec from the configured ASCOM Telescope
    /// instead of from the cached `PointingState`. See the
    /// "Telescope follow mode" section of the service design doc and
    /// the F1–F6 contracts for behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telescope: Option<TelescopeFollowConfig>,
    /// When present, sources `rotation_deg` from a connected ASCOM
    /// Rotator's position angle on every light `StartExposure` instead
    /// of the static `initial_rotation_deg` (F8). Only meaningful in
    /// follow mode — `telescope` must also be set, or config load
    /// fails. See `pointing.rotator` in the service design doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotator: Option<RotatorFollowConfig>,
}

/// Configuration for telescope-following mode. Absent in static mode.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TelescopeFollowConfig {
    pub alpaca_url: String,
    #[serde(default)]
    pub device_number: u32,
    /// Constant offset added to mount RA before the `SkyView` request.
    /// Phase 2 keeps this at the default `0.0`; Phase 3 uses it.
    #[serde(default)]
    pub offset_ra_arcsec: f64,
    /// Constant offset added to mount Dec before the `SkyView` request.
    #[serde(default)]
    pub offset_dec_arcsec: f64,
    /// Per-read timeout on `right_ascension` / `declination` against
    /// the ASCOM Telescope. Bounds the latency a wedged mount can add
    /// to `StartExposure`.
    #[serde(
        default = "default_telescope_request_timeout",
        with = "humantime_serde"
    )]
    #[schemars(with = "String")]
    pub request_timeout: Duration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ClientAuthConfig>,
}

const fn default_telescope_request_timeout() -> Duration {
    Duration::from_secs(2)
}

/// Configuration for the optional follow-mode Rotator.
///
/// Parallel to [`TelescopeFollowConfig`] but with no offset fields —
/// the rotator is read straight through to `rotation_deg`. Absent
/// unless rotator support is wired up, and only valid alongside
/// `telescope`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RotatorFollowConfig {
    pub alpaca_url: String,
    #[serde(default)]
    pub device_number: u32,
    /// Per-read timeout on the `position` read against the ASCOM
    /// Rotator. Bounds the latency a wedged rotator can add to
    /// `StartExposure`, same role as the telescope timeout.
    #[serde(default = "default_rotator_request_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub request_timeout: Duration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ClientAuthConfig>,
}

const fn default_rotator_request_timeout() -> Duration {
    Duration::from_secs(2)
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SurveyConfig {
    pub name: String,
    #[serde(with = "humantime_serde")]
    #[schemars(with = "String")]
    pub request_timeout: Duration,
    #[schemars(with = "String")]
    pub cache_dir: PathBuf,
    /// Base URL the `SurveyClient` hits. Defaults to NASA `SkyView`; tests
    /// override it with a stub server.
    #[serde(default = "default_survey_endpoint")]
    pub endpoint: String,
}

fn default_survey_endpoint() -> String {
    "https://skyview.gsfc.nasa.gov/current/cgi/runquery.pl".to_string()
}

/// Load, parse, and validate the configuration file.
///
/// # Errors
///
/// Returns [`SkySurveyCameraError::ConfigIo`] if the file cannot be
/// read, [`SkySurveyCameraError::ConfigParse`] if it is not valid JSON
/// for a [`Config`], and [`SkySurveyCameraError::ConfigInvalid`] for a
/// value the validator rejects (a non-finite follow-mode offset
/// included).
pub async fn load_config(path: &Path) -> Result<Config, SkySurveyCameraError> {
    let bytes = tokio::fs::read(path).await?;
    let config: Config = serde_json::from_slice(&bytes)?;
    validate(&config)?;
    Ok(config)
}

fn validate(config: &Config) -> Result<(), SkySurveyCameraError> {
    if let Some(t) = &config.pointing.telescope {
        if !t.offset_ra_arcsec.is_finite() {
            return Err(SkySurveyCameraError::ConfigInvalid(
                "pointing.telescope.offset_ra_arcsec must be finite".into(),
            ));
        }
        if !t.offset_dec_arcsec.is_finite() {
            return Err(SkySurveyCameraError::ConfigInvalid(
                "pointing.telescope.offset_dec_arcsec must be finite".into(),
            ));
        }
        if t.request_timeout.is_zero() {
            return Err(SkySurveyCameraError::ConfigInvalid(
                "pointing.telescope.request_timeout must be > 0".into(),
            ));
        }
    }
    // The rotator only feeds `rotation_deg` inside follow mode's
    // `TelescopeFollow`; in static mode there is nowhere to plug it in
    // (rotation comes from the static value / POST). Reject the orphan
    // config rather than silently ignore it.
    if config.pointing.rotator.is_some() && config.pointing.telescope.is_none() {
        return Err(SkySurveyCameraError::ConfigInvalid(
            "pointing.rotator requires pointing.telescope (rotator-driven rotation only applies in follow mode)".into(),
        ));
    }
    if let Some(r) = &config.pointing.rotator {
        if r.request_timeout.is_zero() {
            return Err(SkySurveyCameraError::ConfigInvalid(
                "pointing.rotator.request_timeout must be > 0".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn base_config_with_telescope(telescope: Option<TelescopeFollowConfig>) -> Config {
        Config {
            device: DeviceConfig {
                name: "n".into(),
                unique_id: "u".into(),
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
                telescope,
                rotator: None,
            },
            survey: SurveyConfig {
                name: "DSS2 Red".into(),
                request_timeout: Duration::from_secs(30),
                cache_dir: PathBuf::from("/tmp"),
                endpoint: default_survey_endpoint(),
            },
            server: AlpacaServerConfig::new(0),
        }
    }

    fn telescope_config() -> TelescopeFollowConfig {
        TelescopeFollowConfig {
            alpaca_url: "http://127.0.0.1:32323".into(),
            device_number: 0,
            offset_ra_arcsec: 0.0,
            offset_dec_arcsec: 0.0,
            request_timeout: Duration::from_secs(2),
            auth: None,
        }
    }

    fn rotator_config() -> RotatorFollowConfig {
        RotatorFollowConfig {
            alpaca_url: "http://127.0.0.1:32324".into(),
            device_number: 0,
            request_timeout: Duration::from_secs(2),
            auth: None,
        }
    }

    /// `base_config_with_telescope` plus a rotator block.
    fn config_with_telescope_and_rotator(
        telescope: Option<TelescopeFollowConfig>,
        rotator: Option<RotatorFollowConfig>,
    ) -> Config {
        let mut config = base_config_with_telescope(telescope);
        config.pointing.rotator = rotator;
        config
    }

    #[test]
    fn validate_accepts_static_mode() {
        validate(&base_config_with_telescope(None)).unwrap();
    }

    #[test]
    fn validate_accepts_zero_offset() {
        validate(&base_config_with_telescope(Some(telescope_config()))).unwrap();
    }

    #[test]
    fn validate_rejects_nan_ra_offset() {
        let mut t = telescope_config();
        t.offset_ra_arcsec = f64::NAN;
        let err = validate(&base_config_with_telescope(Some(t))).unwrap_err();
        assert!(format!("{err}").contains("offset_ra_arcsec"));
    }

    #[test]
    fn validate_rejects_infinite_dec_offset() {
        let mut t = telescope_config();
        t.offset_dec_arcsec = f64::INFINITY;
        let err = validate(&base_config_with_telescope(Some(t))).unwrap_err();
        assert!(format!("{err}").contains("offset_dec_arcsec"));
    }

    #[test]
    fn validate_rejects_zero_request_timeout() {
        let mut t = telescope_config();
        t.request_timeout = Duration::ZERO;
        let err = validate(&base_config_with_telescope(Some(t))).unwrap_err();
        assert!(format!("{err}").contains("request_timeout"));
    }

    #[test]
    fn telescope_block_round_trips() {
        let json = r#"{
            "alpaca_url": "http://example/",
            "device_number": 1,
            "offset_ra_arcsec": 60.0,
            "offset_dec_arcsec": -45.0,
            "request_timeout": "5s"
        }"#;
        let cfg: TelescopeFollowConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.alpaca_url, "http://example/");
        assert_eq!(cfg.device_number, 1);
        assert_eq!(cfg.offset_ra_arcsec, 60.0);
        assert_eq!(cfg.offset_dec_arcsec, -45.0);
        assert_eq!(cfg.request_timeout, Duration::from_secs(5));
        assert!(cfg.auth.is_none());
    }

    #[test]
    fn telescope_defaults_when_optional_fields_omitted() {
        let json = r#"{ "alpaca_url": "http://x/" }"#;
        let cfg: TelescopeFollowConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.device_number, 0);
        assert_eq!(cfg.offset_ra_arcsec, 0.0);
        assert_eq!(cfg.offset_dec_arcsec, 0.0);
        assert_eq!(cfg.request_timeout, Duration::from_secs(2));
    }

    #[test]
    fn pointing_telescope_absent_by_default() {
        let json = r#"{
            "initial_ra_deg": 0.0,
            "initial_dec_deg": 0.0
        }"#;
        let cfg: PointingConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.telescope.is_none());
    }

    #[test]
    fn validate_accepts_rotator_with_telescope() {
        validate(&config_with_telescope_and_rotator(
            Some(telescope_config()),
            Some(rotator_config()),
        ))
        .unwrap();
    }

    #[test]
    fn validate_rejects_rotator_without_telescope() {
        let err = validate(&config_with_telescope_and_rotator(
            None,
            Some(rotator_config()),
        ))
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("pointing.rotator requires pointing.telescope"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn validate_rejects_zero_rotator_request_timeout() {
        let mut r = rotator_config();
        r.request_timeout = Duration::ZERO;
        let err = validate(&config_with_telescope_and_rotator(
            Some(telescope_config()),
            Some(r),
        ))
        .unwrap_err();
        assert!(format!("{err}").contains("pointing.rotator.request_timeout"));
    }

    #[test]
    fn rotator_block_round_trips() {
        let json = r#"{
            "alpaca_url": "http://example/",
            "device_number": 2,
            "request_timeout": "7s"
        }"#;
        let cfg: RotatorFollowConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.alpaca_url, "http://example/");
        assert_eq!(cfg.device_number, 2);
        assert_eq!(cfg.request_timeout, Duration::from_secs(7));
        assert!(cfg.auth.is_none());
    }

    #[test]
    fn rotator_defaults_when_optional_fields_omitted() {
        let json = r#"{ "alpaca_url": "http://x/" }"#;
        let cfg: RotatorFollowConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.device_number, 0);
        assert_eq!(cfg.request_timeout, Duration::from_secs(2));
    }

    #[test]
    fn pointing_rotator_absent_by_default() {
        let json = r#"{
            "initial_ra_deg": 0.0,
            "initial_dec_deg": 0.0
        }"#;
        let cfg: PointingConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.rotator.is_none());
    }

    #[test]
    fn config_rejects_unknown_top_level_field() {
        let json = r#"{
            "device": {"name": "n", "description": "d"},
            "optics": {"focal_length_mm": 1000.0, "pixel_size_x_um": 3.76, "pixel_size_y_um": 3.76, "sensor_width_px": 100, "sensor_height_px": 100},
            "pointing": {"initial_ra_deg": 0.0, "initial_dec_deg": 0.0},
            "survey": {"name": "DSS2 Red", "request_timeout": "30s", "cache_dir": "/tmp"},
            "server": {"port": 0},
            "mock": true
        }"#;
        let err = serde_json::from_str::<Config>(json).unwrap_err();
        assert!(err.to_string().contains("mock"), "{err}");
    }

    #[test]
    fn device_config_rejects_unknown_field() {
        let json = r#"{"name": "n", "description": "d", "vendor": "acme"}"#;
        let err = serde_json::from_str::<DeviceConfig>(json).unwrap_err();
        assert!(err.to_string().contains("vendor"), "{err}");
    }

    #[test]
    fn optics_config_rejects_unknown_field() {
        let json = r#"{"focal_length_mm": 1000.0, "pixel_size_x_um": 3.76, "pixel_size_y_um": 3.76, "sensor_width_px": 100, "sensor_height_px": 100, "aperture_mm": 200.0}"#;
        let err = serde_json::from_str::<OpticsConfig>(json).unwrap_err();
        assert!(err.to_string().contains("aperture_mm"), "{err}");
    }

    #[test]
    fn pointing_config_rejects_unknown_field() {
        let json = r#"{"initial_ra_deg": 0.0, "initial_dec_deg": 0.0, "flip": true}"#;
        let err = serde_json::from_str::<PointingConfig>(json).unwrap_err();
        assert!(err.to_string().contains("flip"), "{err}");
    }

    #[test]
    fn telescope_follow_config_rejects_unknown_field() {
        let json = r#"{"alpaca_url": "http://x/", "poll_interval": "1s"}"#;
        let err = serde_json::from_str::<TelescopeFollowConfig>(json).unwrap_err();
        assert!(err.to_string().contains("poll_interval"), "{err}");
    }

    #[test]
    fn rotator_follow_config_rejects_unknown_field() {
        let json = r#"{"alpaca_url": "http://x/", "poll_interval": "1s"}"#;
        let err = serde_json::from_str::<RotatorFollowConfig>(json).unwrap_err();
        assert!(err.to_string().contains("poll_interval"), "{err}");
    }

    #[test]
    fn survey_config_rejects_unknown_field() {
        let json =
            r#"{"name": "DSS2 Red", "request_timeout": "30s", "cache_dir": "/tmp", "retries": 3}"#;
        let err = serde_json::from_str::<SurveyConfig>(json).unwrap_err();
        assert!(err.to_string().contains("retries"), "{err}");
    }

    #[test]
    fn server_block_without_bind_address_defaults_to_all_interfaces() {
        let json = r#"{
            "device": {"name": "n", "description": "d"},
            "optics": {"focal_length_mm": 1000.0, "pixel_size_x_um": 3.76, "pixel_size_y_um": 3.76, "sensor_width_px": 100, "sensor_height_px": 100},
            "pointing": {"initial_ra_deg": 0.0, "initial_dec_deg": 0.0},
            "survey": {"name": "DSS2 Red", "request_timeout": "30s", "cache_dir": "/tmp"},
            "server": {"port": 11116}
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.server.port, 11116);
        assert_eq!(config.server.bind_address.to_string(), "0.0.0.0");
        assert!(config.server.tls.is_none());
        assert!(config.server.auth.is_none());
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod doctor_toml_parity {
    use rusty_photon_server_config::doctor_toml::{parse, ServerClass};

    use super::DEFAULT_PORT;

    /// `pkg/doctor.toml` is this service's catalog entry for
    /// `rusty-photon-doctor` and must match the config defaults
    /// (docs/services/doctor.md §The derived catalog).
    #[test]
    fn pkg_doctor_toml_matches_config_defaults() {
        let meta = parse(include_str!("../pkg/doctor.toml")).unwrap();
        assert_eq!(meta.port, DEFAULT_PORT);
        assert_eq!(meta.class, ServerClass::Alpaca);
        assert!(
            meta.config_gated,
            "sky-survey-camera has no sensible default config"
        );
    }
}

#[cfg(test)]
mod persisted_config_shape {
    use rusty_photon_server_config::unset::explicit_nulls;

    use super::Config;

    /// An unset optional field is spelled by its key's absence, never by an
    /// explicit `null` — see [`rusty_photon_server_config::unset`] for why.
    /// A field that grows without `skip_serializing_if` trips here rather
    /// than filling operators' config files with nulls.
    ///
    /// There is no `Config::default()` (the optics fields are mandatory), so
    /// the subject is a minimal operator-written file: every optional field
    /// is left out, which is exactly the state that must not come back as a
    /// null when `config.apply` persists it.
    #[test]
    fn a_minimal_config_persists_no_explicit_nulls() {
        let minimal = r#"{
            "device": {"name": "n", "description": "d"},
            "optics": {
                "focal_length_mm": 1000.0,
                "pixel_size_x_um": 3.76,
                "pixel_size_y_um": 3.76,
                "sensor_width_px": 100,
                "sensor_height_px": 100
            },
            "pointing": {"initial_ra_deg": 0.0, "initial_dec_deg": 0.0},
            "survey": {"name": "DSS2 Red", "request_timeout": "30s", "cache_dir": "/tmp"},
            "server": {"port": 0}
        }"#;
        let config: Config = serde_json::from_str(minimal).unwrap();
        let persisted = serde_json::to_value(config).unwrap();
        assert_eq!(
            explicit_nulls(&persisted),
            Vec::<String>::new(),
            "unset optional fields must be omitted, not written as null: {persisted}"
        );
    }
}
