//! The service config (docs/services/focus-model.md § Configuration).
//!
//! The HTTP `server` block, the `rp` client fields, the sweep-sizing
//! knobs, the per-train sweep parameters and the store path. Every
//! bounded value is a newtype checked at deserialize
//! (parse-don't-validate), so a bad file fails the load naming the
//! field rather than a sweep mid-night.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use rusty_photon_server_config::ServerConfig;

use crate::error::{FocusModelError, Result};

/// The file name of the redb store under the state directory.
pub const STORE_FILE_NAME: &str = "focus-model.redb";

/// The most sweeps a run may make; a field that fails five sweeps is
/// not going to fit on the sixth.
pub const MAX_ATTEMPTS_CAP: u32 = 5;

/// The most frames a grid point may be measured from: past this the
/// sweep is not a focus run any more, it is a night spent on one
/// point.
pub const FRAMES_PER_STEP_CAP: u32 = 20;

/// A bounded config value: `$name($inner)` accepted when `$check(value)`
/// holds, refused at deserialize with a message naming the field. The
/// inner type is spelled twice because `serde`'s `try_from` takes the
/// type as a string literal.
macro_rules! bounded {
    (
        $(#[$meta:meta])*
        $name:ident($inner:ty as $inner_lit:literal),
        $field:literal,
        $check:expr,
        $message:literal
        $(, also: $extra:ident)?
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, $($extra,)? Serialize, Deserialize)]
        #[serde(try_from = $inner_lit)]
        pub struct $name($inner);

        impl TryFrom<$inner> for $name {
            type Error = String;

            fn try_from(value: $inner) -> std::result::Result<Self, Self::Error> {
                let check: fn($inner) -> bool = $check;
                if check(value) {
                    Ok(Self(value))
                } else {
                    Err(format!(
                        concat!($field, " must be ", $message, " (got {})"),
                        value
                    ))
                }
            }
        }

        impl $name {
            #[must_use]
            pub const fn get(self) -> $inner {
                self.0
            }
        }
    };
}

bounded!(
    /// Where the sweep ends, as a multiple of the focused HFR.
    EndRatio(f64 as "f64"),
    "sweep.end_ratio",
    |v| v.is_finite() && v > 1.0,
    "a finite number greater than 1"
);
bounded!(
    /// Samples across the sweep.
    Points(u32 as "u32"),
    "sweep.points",
    |v| v >= 3,
    "at least 3",
    also: Eq
);
bounded!(
    /// The focused HFR's stand-in, in arcseconds of seeing FWHM.
    SeeingFwhm(f64 as "f64"),
    "sweep.seeing_fwhm_arcsec",
    |v| v.is_finite() && v > 0.0,
    "a finite positive number"
);
bounded!(
    /// Frames measured per grid point.
    FramesPerStep(u32 as "u32"),
    "frames_per_step",
    |v| (1..=FRAMES_PER_STEP_CAP).contains(&v),
    "an integer from 1 to 20",
    also: Eq
);
bounded!(
    /// Accepted samples the fit needs.
    MinFitPoints(usize as "usize"),
    "min_fit_points",
    |v| v >= 3,
    "at least 3",
    also: Eq
);
bounded!(
    /// The sparse gate fraction; 0 disables the gate.
    MinStarFraction(f64 as "f64"),
    "min_star_fraction",
    |v| v.is_finite() && (0.0..1.0).contains(&v),
    "a finite number in [0, 1)"
);
bounded!(
    /// How much worse than the lowest sample the confirmation may be.
    ConfirmationTolerance(f64 as "f64"),
    "confirmation_tolerance",
    |v| v.is_finite() && v >= 0.0,
    "a finite number of at least 0"
);
bounded!(
    /// Sweeps a run may make.
    MaxAttempts(u32 as "u32"),
    "max_attempts",
    |v| (1..=MAX_ATTEMPTS_CAP).contains(&v),
    "an integer from 1 to 5",
    also: Eq
);
bounded!(
    /// A configured sweep step or half width, in focuser steps.
    Steps(i32 as "i32"),
    "step_size / half_width",
    |v| v > 0,
    "a positive integer",
    also: Eq
);
bounded!(
    /// The smallest prediction worth moving to when the optics are unknown.
    MinPredictionMove(i32 as "i32"),
    "min_prediction_move",
    |v| v >= 1,
    "at least 1",
    also: Eq
);
bounded!(
    /// Runs kept per train.
    RunsKept(usize as "usize"),
    "runs_kept",
    |v| v >= 1,
    "at least 1",
    also: Eq
);

/// The sweep-sizing knobs (docs/services/focus-model.md § Sweep sizing).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepConfig {
    #[serde(default = "default_end_ratio")]
    pub end_ratio: EndRatio,
    #[serde(default = "default_points")]
    pub points: Points,
    #[serde(default = "default_seeing_fwhm")]
    pub seeing_fwhm_arcsec: SeeingFwhm,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            end_ratio: default_end_ratio(),
            points: default_points(),
            seeing_fwhm_arcsec: default_seeing_fwhm(),
        }
    }
}

/// One train's sweep parameters: what the sweep needs that is not a
/// fact of the optics. A train absent from the map uses every default.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrainConfig {
    /// Per-frame exposure (humantime, e.g. `"3s"`).
    #[serde(default = "default_duration", with = "humantime_serde")]
    pub duration: Duration,
    #[serde(default = "default_min_area")]
    pub min_area: usize,
    #[serde(default = "default_max_area")]
    pub max_area: usize,
    /// Detection threshold; `None` leaves `rp`'s default.
    #[serde(default)]
    pub threshold_sigma: Option<f64>,
    #[serde(default = "default_frames_per_step")]
    pub frames_per_step: FramesPerStep,
    #[serde(default = "default_min_fit_points")]
    pub min_fit_points: MinFitPoints,
    #[serde(default = "default_min_star_fraction")]
    pub min_star_fraction: MinStarFraction,
    #[serde(default = "default_confirmation_tolerance")]
    pub confirmation_tolerance: ConfirmationTolerance,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: MaxAttempts,
    /// Overrides the derived step.
    #[serde(default)]
    pub step_size: Option<Steps>,
    /// Overrides the derived half width.
    #[serde(default)]
    pub half_width: Option<Steps>,
}

impl Default for TrainConfig {
    fn default() -> Self {
        Self {
            duration: default_duration(),
            min_area: default_min_area(),
            max_area: default_max_area(),
            threshold_sigma: None,
            frames_per_step: default_frames_per_step(),
            min_fit_points: default_min_fit_points(),
            min_star_fraction: default_min_star_fraction(),
            confirmation_tolerance: default_confirmation_tolerance(),
            max_attempts: default_max_attempts(),
            step_size: None,
            half_width: None,
        }
    }
}

/// The service configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The HTTP server for `/mcp` and `/health`. Files without a
    /// `server` block keep loading via the default.
    #[serde(default = "default_server")]
    pub server: ServerConfig,
    /// `rp`'s MCP endpoint, dialed per tool call. Required: it has no
    /// sensible default.
    pub mcp_server_url: String,
    /// HTTP Basic credentials presented to `rp` on MCP calls. The
    /// observatory credential; doctor `--fix` wires it (ADR-017).
    #[serde(default)]
    pub service_auth: Option<rp_mcp_client::ClientAuthConfig>,
    /// PEM CA path used to trust a TLS-enabled `rp`. Per the ADR-017
    /// policy, `service_auth` is only sent when this is set and the URL
    /// is https.
    #[serde(default)]
    pub ca_cert: Option<String>,
    #[serde(default)]
    pub sweep: SweepConfig,
    /// Per-train sweep parameters, keyed by `rp`'s train id.
    #[serde(default)]
    pub trains: BTreeMap<String, TrainConfig>,
    #[serde(default = "default_min_prediction_move")]
    pub min_prediction_move: MinPredictionMove,
    #[serde(default = "default_runs_kept")]
    pub runs_kept: RunsKept,
    /// Override for the redb store file; `None` resolves to the platform
    /// state directory ([`Config::store_path`]).
    #[serde(default)]
    pub store_path: Option<PathBuf>,
}

impl Config {
    #[must_use]
    pub const fn rp_auth(&self) -> Option<&rp_mcp_client::ClientAuthConfig> {
        self.service_auth.as_ref()
    }

    pub fn rp_ca(&self) -> Option<&Path> {
        self.ca_cert.as_deref().map(Path::new)
    }

    /// The sweep parameters of `train_id`: its block, or every default.
    #[must_use]
    pub fn train(&self, train_id: &str) -> TrainConfig {
        self.trains.get(train_id).cloned().unwrap_or_default()
    }

    /// Where the focus store lives: `store_path` when set, otherwise
    /// `focus-model.redb` in the platform state directory
    /// (docs/services/focus-model.md § Store).
    ///
    /// # Errors
    ///
    /// Returns [`FocusModelError::Config`] if the platform state
    /// directory cannot be resolved (no `store_path`, and no platform
    /// config directory on macOS / Windows).
    pub fn store_path(&self) -> Result<PathBuf> {
        match &self.store_path {
            Some(path) => Ok(path.clone()),
            None => Ok(default_state_dir()?.join(STORE_FILE_NAME)),
        }
    }
}

/// The Linux state directory, provisioned and owned by the packaged
/// unit's systemd `StateDirectory=`.
#[cfg(not(any(windows, target_os = "macos")))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the macOS / Windows variant can fail to resolve the platform directory; one signature for both"
)]
fn default_state_dir() -> Result<PathBuf> {
    Ok(PathBuf::from("/var/lib/rusty-photon/focus-model"))
}

/// macOS and Windows keep state beside the config, as `rp` does:
/// `~/Library/Application Support/rusty-photon/focus-model/` and
/// `%PROGRAMDATA%\rusty-photon\focus-model\`.
#[cfg(any(windows, target_os = "macos"))]
fn default_state_dir() -> Result<PathBuf> {
    rusty_photon_config::default_config_dir()
        .map(|dir| dir.join("focus-model"))
        .map_err(|e| {
            FocusModelError::Config(format!(
                "cannot resolve the platform state directory for the store: {e}"
            ))
        })
}

/// focus-model's default `server` block when the file omits it:
/// port 11173 on all interfaces, plain HTTP.
pub(crate) const fn default_server() -> ServerConfig {
    ServerConfig::new(11173)
}

/// CLI overrides layered over the file config after load: `--port` and
/// `--bind-address` pin `server.port` / `server.bind_address` over
/// whatever the file (or the `default_server()` fallback) supplied.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `--port` → `server.port`.
    pub port: Option<u16>,
    /// `--bind-address` → `server.bind_address`.
    pub bind_address: Option<IpAddr>,
}

impl CliOverrides {
    /// Apply the overrides onto `config` in place.
    pub const fn apply(&self, config: &mut Config) {
        if let Some(port) = self.port {
            config.server.port = port;
        }
        if let Some(bind_address) = self.bind_address {
            config.server.bind_address = bind_address;
        }
    }
}

const fn default_end_ratio() -> EndRatio {
    EndRatio(4.0)
}

const fn default_points() -> Points {
    Points(9)
}

const fn default_seeing_fwhm() -> SeeingFwhm {
    SeeingFwhm(2.5)
}

const fn default_duration() -> Duration {
    Duration::from_secs(3)
}

const fn default_min_area() -> usize {
    4
}

const fn default_max_area() -> usize {
    500
}

const fn default_frames_per_step() -> FramesPerStep {
    FramesPerStep(1)
}

const fn default_min_fit_points() -> MinFitPoints {
    MinFitPoints(5)
}

const fn default_min_star_fraction() -> MinStarFraction {
    MinStarFraction(0.1)
}

const fn default_confirmation_tolerance() -> ConfirmationTolerance {
    ConfirmationTolerance(0.25)
}

const fn default_max_attempts() -> MaxAttempts {
    MaxAttempts(2)
}

const fn default_min_prediction_move() -> MinPredictionMove {
    MinPredictionMove(5)
}

const fn default_runs_kept() -> RunsKept {
    RunsKept(500)
}

/// Parse a config document.
///
/// # Errors
///
/// Returns [`FocusModelError::Config`] if `contents` is not JSON or
/// does not parse as a [`Config`] — a bounded value outside its range
/// names its field. `origin` names the source in the message.
pub fn parse_config(contents: &str, origin: &str) -> Result<Config> {
    let config: Config = serde_json::from_str(contents).map_err(|e| {
        FocusModelError::Config(format!("failed to parse config file '{origin}': {e}"))
    })?;
    config.check_area_windows(origin)?;
    Ok(config)
}

impl Config {
    /// The one bound no single field can carry: a detection window
    /// that admits nothing. `rp` reads a frame with no component in
    /// the window as a starless frame, so an inverted pair would be a
    /// season of `not_enough_stars` rather than a configuration
    /// error.
    fn check_area_windows(&self, origin: &str) -> Result<()> {
        for (train_id, train) in &self.trains {
            if train.min_area > train.max_area {
                return Err(FocusModelError::Config(format!(
                    "config file '{origin}': trains.{train_id}.min_area is {} and max_area is \
                     {}; no star can be both",
                    train.min_area, train.max_area
                )));
            }
        }
        Ok(())
    }
}

/// Load a [`Config`] from the JSON file at `path`.
///
/// # Errors
///
/// Returns [`FocusModelError::Config`] if the file cannot be read or
/// does not parse as a [`Config`].
pub fn load_config(path: &Path) -> Result<Config> {
    let contents = std::fs::read_to_string(path).map_err(|e| {
        FocusModelError::Config(format!(
            "failed to read config file '{}': {}",
            path.display(),
            e
        ))
    })?;
    parse_config(&contents, &path.display().to_string())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"{ "mcp_server_url": "http://localhost:11115/mcp" }"#;

    #[test]
    fn a_minimal_config_takes_the_documented_defaults() {
        let config = parse_config(MINIMAL, "test").unwrap();
        assert_eq!(config.mcp_server_url, "http://localhost:11115/mcp");
        assert_eq!(config.sweep.end_ratio.get(), 4.0);
        assert_eq!(config.sweep.points.get(), 9);
        assert_eq!(config.sweep.seeing_fwhm_arcsec.get(), 2.5);
        assert!(config.trains.is_empty());
        assert_eq!(config.min_prediction_move.get(), 5);
        assert_eq!(config.runs_kept.get(), 500);
        assert!(config.store_path.is_none());
        assert!(config.service_auth.is_none());
        assert!(config.ca_cert.is_none());
        // A file without a `server` block keeps loading via the default.
        assert_eq!(config.server.port, 11173);
        assert_eq!(config.server.bind_address.to_string(), "0.0.0.0");
        assert!(config.server.tls.is_none());
        assert!(config.server.auth.is_none());
    }

    #[test]
    fn a_train_absent_from_the_map_uses_every_default() {
        let config = parse_config(MINIMAL, "test").unwrap();
        let train = config.train("imaging");
        assert_eq!(train.duration, Duration::from_secs(3));
        assert_eq!(train.min_area, 4);
        assert_eq!(train.max_area, 500);
        assert_eq!(train.threshold_sigma, None);
        assert_eq!(train.frames_per_step.get(), 1);
        assert_eq!(train.min_fit_points.get(), 5);
        assert_eq!(train.min_star_fraction.get(), 0.1);
        assert_eq!(train.confirmation_tolerance.get(), 0.25);
        assert_eq!(train.max_attempts.get(), 2);
        assert_eq!(train.step_size, None);
        assert_eq!(train.half_width, None);
    }

    #[test]
    fn mcp_server_url_is_required() {
        let err = parse_config("{}", "test").unwrap_err();
        assert!(err.to_string().contains("mcp_server_url"), "{err}");
    }

    #[test]
    fn a_full_config_parses() {
        let json = r#"{
            "server": { "port": 12000, "bind_address": "127.0.0.1" },
            "mcp_server_url": "https://rig:11115/mcp",
            "service_auth": { "username": "observatory", "password": "s3cret" },
            "ca_cert": "/etc/rusty-photon/pki/ca.pem",
            "sweep": { "end_ratio": 3.0, "points": 11, "seeing_fwhm_arcsec": 3.5 },
            "trains": {
                "imaging": {
                    "duration": "2s", "min_area": 6, "max_area": 800,
                    "threshold_sigma": 4.0, "frames_per_step": 2,
                    "min_fit_points": 6, "min_star_fraction": 0.2,
                    "confirmation_tolerance": 0.5, "max_attempts": 3,
                    "step_size": 25, "half_width": 100
                }
            },
            "min_prediction_move": 10,
            "runs_kept": 50,
            "store_path": "/data/focus.redb"
        }"#;
        let config = parse_config(json, "test").unwrap();
        assert_eq!(config.server.socket_addr().to_string(), "127.0.0.1:12000");
        assert_eq!(config.rp_auth().unwrap().username, "observatory");
        assert_eq!(
            config.rp_ca(),
            Some(Path::new("/etc/rusty-photon/pki/ca.pem"))
        );
        assert_eq!(config.sweep.end_ratio.get(), 3.0);
        assert_eq!(config.sweep.points.get(), 11);
        assert_eq!(config.sweep.seeing_fwhm_arcsec.get(), 3.5);
        let train = config.train("imaging");
        assert_eq!(train.duration, Duration::from_secs(2));
        assert_eq!(train.min_area, 6);
        assert_eq!(train.max_area, 800);
        assert_eq!(train.threshold_sigma, Some(4.0));
        assert_eq!(train.frames_per_step.get(), 2);
        assert_eq!(train.min_fit_points.get(), 6);
        assert_eq!(train.min_star_fraction.get(), 0.2);
        assert_eq!(train.confirmation_tolerance.get(), 0.5);
        assert_eq!(train.max_attempts.get(), 3);
        assert_eq!(train.step_size.map(Steps::get), Some(25));
        assert_eq!(train.half_width.map(Steps::get), Some(100));
        assert_eq!(config.min_prediction_move.get(), 10);
        assert_eq!(config.runs_kept.get(), 50);
        assert_eq!(
            config.store_path().unwrap(),
            PathBuf::from("/data/focus.redb")
        );
    }

    /// Every bounded field refuses a value outside its range, naming
    /// the field in the error.
    #[test]
    fn every_bound_is_checked_at_load_naming_the_field() {
        let cases: [(&str, &str); 11] = [
            (r#""sweep": { "end_ratio": 1.0 }"#, "sweep.end_ratio"),
            (r#""sweep": { "points": 2 }"#, "sweep.points"),
            (
                r#""sweep": { "seeing_fwhm_arcsec": 0 }"#,
                "sweep.seeing_fwhm_arcsec",
            ),
            (
                r#""trains": { "t": { "frames_per_step": 0 } }"#,
                "frames_per_step",
            ),
            (
                r#""trains": { "t": { "min_fit_points": 2 } }"#,
                "min_fit_points",
            ),
            (
                r#""trains": { "t": { "min_star_fraction": 1.0 } }"#,
                "min_star_fraction",
            ),
            (
                r#""trains": { "t": { "confirmation_tolerance": -0.1 } }"#,
                "confirmation_tolerance",
            ),
            (
                r#""trains": { "t": { "max_attempts": 6 } }"#,
                "max_attempts",
            ),
            (r#""trains": { "t": { "step_size": 0 } }"#, "step_size"),
            (r#""min_prediction_move": 0"#, "min_prediction_move"),
            (r#""runs_kept": 0"#, "runs_kept"),
        ];
        for (fragment, field) in cases {
            let json = format!(r#"{{ "mcp_server_url": "http://x/mcp", {fragment} }}"#);
            let err = parse_config(&json, "test").unwrap_err();
            assert!(err.to_string().contains(field), "{fragment}: {err}");
        }
    }

    /// The one rule that spans two fields: a detection window no
    /// component can be inside would read as a starless sky all
    /// night, so it fails the load instead.
    #[test]
    fn a_detection_window_that_admits_nothing_is_refused() {
        let json = r#"{ "mcp_server_url": "http://x/mcp",
            "trains": { "imaging": { "min_area": 500, "max_area": 4 } } }"#;
        let err = parse_config(json, "test").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("trains.imaging.min_area"), "{message}");
        assert!(message.contains("max_area"), "{message}");

        let ok = r#"{ "mcp_server_url": "http://x/mcp",
            "trains": { "imaging": { "min_area": 4, "max_area": 4 } } }"#;
        parse_config(ok, "test").unwrap();
    }

    #[test]
    fn an_unknown_key_is_rejected_naming_it_at_every_level() {
        let top = r#"{ "mcp_server_url": "http://x/mcp", "dither_pixels": 5.0 }"#;
        let err = parse_config(top, "test").unwrap_err();
        assert!(err.to_string().contains("dither_pixels"), "{err}");

        let train =
            r#"{ "mcp_server_url": "http://x/mcp", "trains": { "t": { "exposure": "3s" } } }"#;
        let err = parse_config(train, "test").unwrap_err();
        assert!(err.to_string().contains("exposure"), "{err}");

        let sweep = r#"{ "mcp_server_url": "http://x/mcp", "sweep": { "width": 3 } }"#;
        let err = parse_config(sweep, "test").unwrap_err();
        assert!(err.to_string().contains("width"), "{err}");
    }

    #[test]
    fn the_default_store_path_ends_in_the_store_file_under_the_service_dir() {
        let config = parse_config(MINIMAL, "test").unwrap();
        let path = config.store_path().unwrap();
        assert_eq!(path.file_name().unwrap(), STORE_FILE_NAME);
        assert_eq!(
            path.parent().unwrap().file_name().unwrap(),
            "focus-model",
            "{}",
            path.display()
        );
    }

    #[test]
    fn cli_overrides_pin_port_and_bind_address() {
        let mut config = parse_config(MINIMAL, "test").unwrap();
        let overrides = CliOverrides {
            port: Some(12345),
            bind_address: Some("127.0.0.1".parse().unwrap()),
        };
        overrides.apply(&mut config);
        assert_eq!(config.server.socket_addr().to_string(), "127.0.0.1:12345");
    }

    #[test]
    fn empty_cli_overrides_leave_the_config_untouched() {
        let mut config = parse_config(MINIMAL, "test").unwrap();
        CliOverrides::default().apply(&mut config);
        assert_eq!(config.server.socket_addr().to_string(), "0.0.0.0:11173");
    }

    #[test]
    fn load_config_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focus-model.json");
        std::fs::write(&path, MINIMAL).unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(config.mcp_server_url, "http://localhost:11115/mcp");
    }

    #[test]
    fn load_config_missing_file() {
        let err = load_config(Path::new("/nonexistent/focus-model/config.json")).unwrap_err();
        assert!(err.to_string().contains("failed to read config file"));
    }

    #[test]
    fn load_config_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "not valid json").unwrap();
        let err = load_config(&path).unwrap_err();
        assert!(err.to_string().contains("failed to parse config file"));
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
            "focus-model has no sensible default config"
        );
    }
}
