use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

pub use rusty_photon_server_config::ServerConfig;

use crate::error::{PolarAlignError, Result};

/// Geodetic latitude in degrees, north positive. Rejects values
/// outside [-90, 90] and sites within 1° of the equator, where the
/// visible pole sits on the horizon and no meaningful alignment
/// target exists.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(try_from = "f64")]
pub struct LatitudeDeg(f64);

impl LatitudeDeg {
    #[must_use]
    pub const fn degrees(self) -> f64 {
        self.0
    }

    /// +1.0 for a northern site, -1.0 for a southern one.
    #[must_use]
    pub const fn hemisphere_sign(self) -> f64 {
        self.0.signum()
    }
}

impl TryFrom<f64> for LatitudeDeg {
    type Error = String;

    fn try_from(value: f64) -> std::result::Result<Self, Self::Error> {
        if !value.is_finite() || !(-90.0..=90.0).contains(&value) {
            return Err(format!(
                "site.latitude_deg must be in [-90, 90], got {value}"
            ));
        }
        if value.abs() < 1.0 {
            return Err(format!(
                "site.latitude_deg {value} is within 1° of the equator; the visible \
                 celestial pole sits on the horizon and cannot be aligned to"
            ));
        }
        Ok(Self(value))
    }
}

/// Longitude in degrees, east positive, [-180, 180].
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(try_from = "f64")]
pub struct LongitudeDeg(f64);

impl LongitudeDeg {
    #[must_use]
    pub const fn degrees(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for LongitudeDeg {
    type Error = String;

    fn try_from(value: f64) -> std::result::Result<Self, Self::Error> {
        if !value.is_finite() || !(-180.0..=180.0).contains(&value) {
            return Err(format!(
                "site.longitude_deg must be in [-180, 180], got {value}"
            ));
        }
        Ok(Self(value))
    }
}

/// Hour angle of the first measurement point, degrees from the
/// meridian, [1, 60].
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(try_from = "f64")]
pub struct FirstPointHaDeg(f64);

impl FirstPointHaDeg {
    #[must_use]
    pub const fn degrees(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for FirstPointHaDeg {
    type Error = String;

    fn try_from(value: f64) -> std::result::Result<Self, Self::Error> {
        if !value.is_finite() || !(1.0..=60.0).contains(&value) {
            return Err(format!(
                "measurement.first_point_ha_deg must be in [1, 60], got {value}"
            ));
        }
        Ok(Self(value))
    }
}

/// Hour-angle step between measurement points, degrees, [10, 60].
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(try_from = "f64")]
pub struct SweepDeg(f64);

impl SweepDeg {
    #[must_use]
    pub const fn degrees(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for SweepDeg {
    type Error = String;

    fn try_from(value: f64) -> std::result::Result<Self, Self::Error> {
        if !value.is_finite() || !(10.0..=60.0).contains(&value) {
            return Err(format!(
                "measurement.sweep_deg must be in [10, 60], got {value}"
            ));
        }
        Ok(Self(value))
    }
}

/// How the three measurement points are chosen (design doc
/// §Measurement phase; plan D1/D9/D11).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementMode {
    /// The near-pole sweep at the configured declination.
    #[default]
    NearPole,
    /// Sweep the RA axis from wherever the mount points, away from
    /// the meridian. Every measurement solve must carry a
    /// `wcs_matrix` — the axis comes from full camera attitudes.
    CurrentPosition,
    /// The operator rotates a non-GoTo tracker's RA axis by hand
    /// between exposures, confirming each rotation via
    /// `POST /measure/continue`. No mount tool is called; solves are
    /// blind; attitudes are required exactly as in
    /// `current_position`.
    ManualRotation,
}

/// Which side of the meridian the three measurement points sit on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SweepDirection {
    East,
    West,
}

impl SweepDirection {
    /// Sign applied to the hour-angle offsets: hour angle is positive
    /// west of the meridian.
    #[must_use]
    pub const fn ha_sign(self) -> f64 {
        match self {
            Self::West => 1.0,
            Self::East => -1.0,
        }
    }
}

/// Observer site. Latitude fixes the pole target; longitude fixes
/// local sidereal time for the slew targets.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteConfig {
    pub latitude_deg: LatitudeDeg,
    pub longitude_deg: LongitudeDeg,
}

/// The three-point measurement sweep.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasurementConfig {
    #[serde(default)]
    pub mode: MeasurementMode,
    /// Measurement declination in degrees, `near_pole` mode only.
    /// The magnitude is what matters; the sign is folded to the site
    /// hemisphere at load.
    #[serde(default = "default_measurement_dec")]
    pub dec_deg: f64,
    #[serde(default = "default_first_point_ha")]
    pub first_point_ha_deg: FirstPointHaDeg,
    #[serde(default = "default_sweep")]
    pub sweep_deg: SweepDeg,
    #[serde(default = "default_direction")]
    pub direction: SweepDirection,
    #[serde(default = "default_exposure", with = "humantime_serde")]
    pub exposure: Duration,
    #[serde(default = "default_settle", with = "humantime_serde")]
    pub settle: Duration,
    /// `manual_rotation` only: ceiling on each wait for the
    /// operator's `POST /measure/continue`; expiry aborts the
    /// workflow naming the point.
    #[serde(default = "default_manual_timeout", with = "humantime_serde")]
    pub manual_timeout: Duration,
}

/// The live adjustment loop.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdjustmentConfig {
    #[serde(default = "default_exposure", with = "humantime_serde")]
    pub exposure: Duration,
    #[serde(default = "default_interval", with = "humantime_serde")]
    pub interval: Duration,
    #[serde(default = "default_max_duration", with = "humantime_serde")]
    pub max_duration: Duration,
    #[serde(default = "default_max_solve_failures")]
    pub max_solve_failures: u32,
    #[serde(default = "default_star_count")]
    pub star_count: usize,
}

/// Parameters forwarded to rp's `plate_solve` tool.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SolveConfig {
    #[serde(default = "default_search_radius")]
    pub search_radius_deg: f64,
    #[serde(default = "default_solve_timeout", with = "humantime_serde")]
    pub timeout: Duration,
}

/// Refraction model applied to the axis conversion (see the design
/// doc's Geometry Reference; the pole target itself carries no
/// refraction term).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefractionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_temperature")]
    pub temperature_c: f64,
    #[serde(default = "default_pressure")]
    pub pressure_hpa: f64,
}

impl Default for RefractionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            temperature_c: default_temperature(),
            pressure_hpa: default_pressure(),
        }
    }
}

/// The polar-align service configuration. Doubles as the plugin's
/// standalone config file, so it carries the HTTP `server` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolarAlignConfig {
    /// The HTTP server for `/invoke`, `/health`, `/status`,
    /// `/measure/continue`, `/adjust/finish`, `/preview.png`. Files
    /// without a `server` block keep loading via the default.
    #[serde(default = "default_server")]
    pub server: ServerConfig,
    pub camera_id: String,
    pub mount_id: String,
    /// Observer site. Optional since D10: when absent, each workflow
    /// resolves the site from rp's `get_site` tool; an explicit
    /// block wins.
    #[serde(default)]
    pub site: Option<SiteConfig>,
    #[serde(default = "default_measurement")]
    pub measurement: MeasurementConfig,
    #[serde(default = "default_adjustment")]
    pub adjustment: AdjustmentConfig,
    #[serde(default = "default_solve")]
    pub solve: SolveConfig,
    #[serde(default)]
    pub refraction: RefractionConfig,
    /// HTTP Basic credentials presented to `rp` — MCP calls and the
    /// completion POST alike. The D6 observatory credential; doctor
    /// `--fix` wires it (ADR-017).
    #[serde(default)]
    pub service_auth: Option<rp_mcp_client::ClientAuthConfig>,
    /// PEM CA path used to trust a TLS-enabled `rp`. Per the ADR-017
    /// policy, `service_auth` is only sent when this is set and the
    /// URL is https.
    #[serde(default)]
    pub ca_cert: Option<String>,
}

impl PolarAlignConfig {
    #[must_use]
    pub const fn rp_auth(&self) -> Option<&rp_mcp_client::ClientAuthConfig> {
        self.service_auth.as_ref()
    }

    pub fn rp_ca(&self) -> Option<&std::path::Path> {
        self.ca_cert.as_deref().map(std::path::Path::new)
    }

    /// Measurement declination with its sign folded to the observer's
    /// hemisphere: a northern site measures at +|dec|, a southern one
    /// at -|dec|. The hemisphere sign comes from the *resolved* site
    /// (config or rp), so the fold happens at workflow start.
    #[must_use]
    pub fn measurement_dec_deg(&self, hemisphere_sign: f64) -> f64 {
        self.measurement.dec_deg.abs() * hemisphere_sign.signum()
    }

    /// Cross-field rules that single-field newtypes cannot express.
    /// Deliberately mode-unconditional — near-pole geometry is
    /// checked even in modes that ignore it: the mode is a
    /// per-session choice, and a latent config error should fail at
    /// load, not on the night the operator switches back to
    /// `near_pole`.
    #[expect(
        clippy::suboptimal_flops,
        reason = "first + 2 × sweep matches the span formula the error message spells out"
    )]
    fn validate_cross_field(&self) -> Result<()> {
        let dec = self.measurement.dec_deg;
        if !dec.is_finite() || !(60.0..90.0).contains(&dec.abs()) {
            return Err(PolarAlignError::Config(format!(
                "measurement.dec_deg magnitude must be in [60, 90) (near-pole sweep, \
                 never the exact pole), got {dec}"
            )));
        }
        let span = self.measurement.first_point_ha_deg.degrees()
            + 2.0 * self.measurement.sweep_deg.degrees();
        if span > 150.0 {
            return Err(PolarAlignError::Config(format!(
                "measurement sweep spans {span}° of hour angle \
                 (first_point_ha_deg + 2 × sweep_deg); the limit is 150° so all \
                 three points stay on one side of the meridian with margin"
            )));
        }
        Ok(())
    }
}

/// polar-align's default `server` block when the config file omits
/// it: port 11172 on all interfaces, plain HTTP.
pub(crate) const fn default_server() -> ServerConfig {
    ServerConfig::new(11172)
}

/// CLI overrides layered over the file config after load.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `--port` → `server.port`.
    pub port: Option<u16>,
    /// `--bind-address` → `server.bind_address`.
    pub bind_address: Option<IpAddr>,
}

impl CliOverrides {
    /// Apply the overrides onto `config` in place.
    pub const fn apply(&self, config: &mut PolarAlignConfig) {
        if let Some(port) = self.port {
            config.server.port = port;
        }
        if let Some(bind_address) = self.bind_address {
            config.server.bind_address = bind_address;
        }
    }
}

fn default_measurement() -> MeasurementConfig {
    MeasurementConfig {
        mode: MeasurementMode::default(),
        dec_deg: default_measurement_dec(),
        first_point_ha_deg: default_first_point_ha(),
        sweep_deg: default_sweep(),
        direction: default_direction(),
        exposure: default_exposure(),
        settle: default_settle(),
        manual_timeout: default_manual_timeout(),
    }
}

const fn default_adjustment() -> AdjustmentConfig {
    AdjustmentConfig {
        exposure: default_exposure(),
        interval: default_interval(),
        max_duration: default_max_duration(),
        max_solve_failures: default_max_solve_failures(),
        star_count: default_star_count(),
    }
}

const fn default_solve() -> SolveConfig {
    SolveConfig {
        search_radius_deg: default_search_radius(),
        timeout: default_solve_timeout(),
    }
}

const fn default_measurement_dec() -> f64 {
    85.0
}

const fn default_first_point_ha() -> FirstPointHaDeg {
    FirstPointHaDeg(15.0)
}

const fn default_sweep() -> SweepDeg {
    SweepDeg(45.0)
}

const fn default_direction() -> SweepDirection {
    SweepDirection::West
}

const fn default_exposure() -> Duration {
    Duration::from_secs(2)
}

const fn default_settle() -> Duration {
    Duration::from_secs(2)
}

const fn default_interval() -> Duration {
    Duration::from_secs(1)
}

const fn default_max_duration() -> Duration {
    Duration::from_mins(30)
}

const fn default_manual_timeout() -> Duration {
    Duration::from_mins(10)
}

const fn default_max_solve_failures() -> u32 {
    10
}

const fn default_star_count() -> usize {
    10
}

const fn default_search_radius() -> f64 {
    5.0
}

const fn default_solve_timeout() -> Duration {
    Duration::from_secs(30)
}

const fn default_temperature() -> f64 {
    10.0
}

const fn default_pressure() -> f64 {
    1010.0
}

const fn default_true() -> bool {
    true
}

pub fn load_config(path: &Path) -> Result<PolarAlignConfig> {
    let contents = std::fs::read_to_string(path).map_err(|e| {
        PolarAlignError::Config(format!(
            "failed to read config file '{}': {}",
            path.display(),
            e
        ))
    })?;
    let config: PolarAlignConfig = serde_json::from_str(&contents).map_err(|e| {
        PolarAlignError::Config(format!(
            "failed to parse config file '{}': {}",
            path.display(),
            e
        ))
    })?;
    config.validate_cross_field()?;
    Ok(config)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn minimal_json() -> String {
        r#"{
            "camera_id": "main-cam",
            "mount_id": "mount",
            "site": { "latitude_deg": 48.1, "longitude_deg": -122.8 }
        }"#
        .to_string()
    }

    fn parse_with_cross_field(json: &str) -> Result<PolarAlignConfig> {
        let config: PolarAlignConfig =
            serde_json::from_str(json).map_err(|e| PolarAlignError::Config(e.to_string()))?;
        config.validate_cross_field()?;
        Ok(config)
    }

    #[test]
    fn minimal_config_gets_documented_defaults() {
        let config = parse_with_cross_field(&minimal_json()).unwrap();
        assert_eq!(config.server.port, 11172);
        assert_eq!(config.measurement.mode, MeasurementMode::NearPole);
        assert_eq!(config.measurement.dec_deg, 85.0);
        assert_eq!(config.measurement.first_point_ha_deg.degrees(), 15.0);
        assert_eq!(config.measurement.sweep_deg.degrees(), 45.0);
        assert_eq!(config.measurement.direction, SweepDirection::West);
        assert_eq!(config.measurement.exposure, Duration::from_secs(2));
        assert_eq!(config.measurement.manual_timeout, Duration::from_mins(10));
        assert_eq!(config.adjustment.max_duration, Duration::from_mins(30));
        assert_eq!(config.adjustment.max_solve_failures, 10);
        assert_eq!(config.adjustment.star_count, 10);
        assert_eq!(config.solve.search_radius_deg, 5.0);
        assert!(config.refraction.enabled);
        assert_eq!(config.refraction.pressure_hpa, 1010.0);
        assert!(config.service_auth.is_none());
    }

    #[test]
    fn measurement_dec_folds_to_southern_hemisphere() {
        let config = parse_with_cross_field(&minimal_json()).unwrap();
        assert_eq!(config.measurement_dec_deg(-1.0), -85.0);
    }

    #[test]
    fn measurement_dec_folds_positive_in_the_north() {
        let json = r#"{
            "camera_id": "c", "mount_id": "m",
            "site": { "latitude_deg": 48.0, "longitude_deg": 0.0 },
            "measurement": { "dec_deg": -85.0 }
        }"#;
        let config = parse_with_cross_field(json).unwrap();
        assert_eq!(config.measurement_dec_deg(1.0), 85.0);
    }

    #[test]
    fn site_block_is_optional_since_d10() {
        let json = r#"{ "camera_id": "c", "mount_id": "m" }"#;
        let config = parse_with_cross_field(json).unwrap();
        assert!(config.site.is_none(), "absent site resolves from rp");
    }

    #[test]
    fn measurement_mode_manual_rotation_parses_with_its_timeout() {
        let json = r#"{
            "camera_id": "c", "mount_id": "m",
            "measurement": { "mode": "manual_rotation", "manual_timeout": "3m" }
        }"#;
        let config = parse_with_cross_field(json).unwrap();
        assert_eq!(config.measurement.mode, MeasurementMode::ManualRotation);
        assert_eq!(config.measurement.manual_timeout, Duration::from_mins(3));
    }

    #[test]
    fn latitude_out_of_range_names_the_field() {
        let json = r#"{
            "camera_id": "c", "mount_id": "m",
            "site": { "latitude_deg": 91.0, "longitude_deg": 0.0 }
        }"#;
        let err = serde_json::from_str::<PolarAlignConfig>(json).unwrap_err();
        assert!(err.to_string().contains("site.latitude_deg"), "{err}");
    }

    #[test]
    fn equatorial_latitude_is_rejected() {
        let json = r#"{
            "camera_id": "c", "mount_id": "m",
            "site": { "latitude_deg": 0.5, "longitude_deg": 0.0 }
        }"#;
        let err = serde_json::from_str::<PolarAlignConfig>(json).unwrap_err();
        assert!(err.to_string().contains("equator"), "{err}");
    }

    #[test]
    fn longitude_out_of_range_names_the_field() {
        let json = r#"{
            "camera_id": "c", "mount_id": "m",
            "site": { "latitude_deg": 48.0, "longitude_deg": 181.0 }
        }"#;
        let err = serde_json::from_str::<PolarAlignConfig>(json).unwrap_err();
        assert!(err.to_string().contains("site.longitude_deg"), "{err}");
    }

    #[test]
    fn first_point_ha_out_of_range_is_rejected() {
        let json = r#"{
            "camera_id": "c", "mount_id": "m",
            "site": { "latitude_deg": 48.0, "longitude_deg": 0.0 },
            "measurement": { "first_point_ha_deg": 0.5 }
        }"#;
        let err = serde_json::from_str::<PolarAlignConfig>(json).unwrap_err();
        assert!(err.to_string().contains("first_point_ha_deg"), "{err}");
    }

    #[test]
    fn sweep_out_of_range_is_rejected() {
        let json = r#"{
            "camera_id": "c", "mount_id": "m",
            "site": { "latitude_deg": 48.0, "longitude_deg": 0.0 },
            "measurement": { "sweep_deg": 80.0 }
        }"#;
        let err = serde_json::from_str::<PolarAlignConfig>(json).unwrap_err();
        assert!(err.to_string().contains("sweep_deg"), "{err}");
    }

    #[test]
    fn sweep_span_over_150_degrees_is_rejected() {
        let json = r#"{
            "camera_id": "c", "mount_id": "m",
            "site": { "latitude_deg": 48.0, "longitude_deg": 0.0 },
            "measurement": { "first_point_ha_deg": 40.0, "sweep_deg": 60.0 }
        }"#;
        let err = parse_with_cross_field(json).unwrap_err();
        assert!(err.to_string().contains("150"), "{err}");
    }

    #[test]
    fn dec_at_the_exact_pole_is_rejected() {
        let json = r#"{
            "camera_id": "c", "mount_id": "m",
            "site": { "latitude_deg": 48.0, "longitude_deg": 0.0 },
            "measurement": { "dec_deg": 90.0 }
        }"#;
        let err = parse_with_cross_field(json).unwrap_err();
        assert!(err.to_string().contains("dec_deg"), "{err}");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let json = r#"{
            "camera_id": "c", "mount_id": "m",
            "site": { "latitude_deg": 48.0, "longitude_deg": 0.0 },
            "polar_scope": true
        }"#;
        let err = serde_json::from_str::<PolarAlignConfig>(json).unwrap_err();
        assert!(err.to_string().contains("polar_scope"), "{err}");
    }

    #[test]
    fn measurement_mode_current_position_parses() {
        let json = r#"{
            "camera_id": "c", "mount_id": "m",
            "site": { "latitude_deg": 48.0, "longitude_deg": 0.0 },
            "measurement": { "mode": "current_position" }
        }"#;
        let config = parse_with_cross_field(json).unwrap();
        assert_eq!(config.measurement.mode, MeasurementMode::CurrentPosition);
    }

    #[test]
    fn measurement_mode_unknown_variant_is_rejected() {
        let json = r#"{
            "camera_id": "c", "mount_id": "m",
            "site": { "latitude_deg": 48.0, "longitude_deg": 0.0 },
            "measurement": { "mode": "somewhere_nice" }
        }"#;
        let err = serde_json::from_str::<PolarAlignConfig>(json).unwrap_err();
        assert!(err.to_string().contains("somewhere_nice"), "{err}");
    }

    #[test]
    fn direction_east_flips_the_ha_sign() {
        assert_eq!(SweepDirection::West.ha_sign(), 1.0);
        assert_eq!(SweepDirection::East.ha_sign(), -1.0);
    }

    #[test]
    fn cli_overrides_pin_port_and_bind_address() {
        let mut config = parse_with_cross_field(&minimal_json()).unwrap();
        let overrides = CliOverrides {
            port: Some(12345),
            bind_address: Some("127.0.0.1".parse().unwrap()),
        };
        overrides.apply(&mut config);
        assert_eq!(config.server.socket_addr().to_string(), "127.0.0.1:12345");
    }

    #[test]
    fn load_config_missing_file() {
        let err = load_config(Path::new("/nonexistent/polar-align/config.json")).unwrap_err();
        assert!(err.to_string().contains("failed to read config file"));
    }

    #[test]
    fn load_config_from_file_runs_cross_field_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "camera_id": "c", "mount_id": "m",
                "site": { "latitude_deg": 48.0, "longitude_deg": 0.0 },
                "measurement": { "dec_deg": 30.0 }
            }"#,
        )
        .unwrap();
        let err = load_config(&path).unwrap_err();
        assert!(err.to_string().contains("dec_deg"), "{err}");
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod doctor_toml_parity {
    use rusty_photon_server_config::doctor_toml::{parse, ServerClass};

    use super::default_server;

    /// `pkg/doctor.toml` is this service's catalog entry for
    /// `rusty-photon-doctor` and must match the config defaults
    /// (docs/services/doctor.md §The derived catalog).
    #[test]
    fn pkg_doctor_toml_matches_config_defaults() {
        let meta = parse(include_str!("../pkg/doctor.toml")).unwrap();
        assert_eq!(meta.port, default_server().port);
        assert_eq!(meta.class, ServerClass::Core);
        assert!(
            meta.config_gated,
            "polar-align has no sensible default config"
        );
    }
}
