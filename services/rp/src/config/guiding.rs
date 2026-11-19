use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// HTTP-client connection to the guider rp-managed service, plus
/// per-rig guiding defaults (rp.md § Guider Service).
///
/// Lives at `equipment.mount.guiding` — guiding is mount-scoped: the
/// guider corrects and dithers by moving the mount, which moves every
/// optical train on it, so the schema makes a guider without a mount
/// unrepresentable.
///
/// `timeout` bounds a request the guider answers at once: a stop, a
/// pause, a resume, a read. The two that wait for a
/// settle outlast it: a `dither` runs under the resolved settle
/// timeout plus 15 s, and a `start_guiding` under
/// `calibration_timeout` plus that, since a start calibrates when the
/// guider holds no calibration it can use. With no settle timeout
/// resolved the guider's own applies, which rp cannot see, and
/// `timeout` stands in for it.
///
/// The `settle_*` fields are operator-set defaults forwarded on every
/// `start_guiding` / `dither` call unless the per-call MCP parameter
/// overrides them; fields left unset are omitted from the wire and the
/// guider's own settle config applies. `settle_pixels` is in
/// guide-camera pixels.
///
/// `dither_pixels` is the default `dither` amount, in guide-camera
/// pixels, when the per-call `pixels` parameter is omitted. How
/// *often* to dither is workflow policy (Tenet 7) and deliberately not
/// configured here.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GuidingConfig {
    pub url: String,
    #[serde(default = "default_guiding_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub timeout: Duration,
    /// How long a `start_guiding` may spend calibrating, on top of its
    /// settle. Set no lower than the guider's own
    /// `calibration.timeout`. Defaults to 10 minutes.
    #[serde(default = "default_calibration_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub calibration_timeout: Duration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle_pixels: Option<f64>,
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    pub settle_time: Option<Duration>,
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    pub settle_timeout: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dither_pixels: Option<f64>,
    /// Rotation threshold for the rotate-while-guiding ladder (rp.md
    /// § Rotator Tool Details): when rp rotates a guide-coupled train,
    /// a |Δθ| above this many degrees empties the guider's calibration
    /// store; at or below it the cross-axis leak (sin Δθ) sits inside
    /// guiding's noise floor. Defaults to 5°.
    #[serde(default)]
    pub recalibrate_above_deg: RecalibrateAboveDeg,
    /// How long the guider may show `star_lost` before the Guide Run
    /// Watch emits `guide_star_lost`, and the spacing of the event's
    /// repeats while the loss lasts (rp.md § Guide Run Watch).
    /// Defaults to 60 s.
    #[serde(default = "default_star_lost_after", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub star_lost_after: Duration,
    /// The Guide Focus Watch (rp.md § Guide Focus Watch). Omitted →
    /// the watch is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_watch: Option<FocusWatchConfig>,
    /// Optional HTTP Basic Auth credentials for connecting to an
    /// auth-enabled guider service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<rp_auth::config::ClientAuthConfig>,
}

/// The `focus_watch` sub-block: thresholds turning a degrading HFD
/// trend into `guide_focus_degraded` / `guide_focus_escalation`
/// events. Every field is optional; the block's presence enables the
/// watch.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FocusWatchConfig {
    /// Frames per median (baseline and trailing). Default 10.
    #[serde(default)]
    pub window: WatchWindow,
    /// Degradation threshold: trailing median > baseline × ratio.
    /// Default 1.25.
    #[serde(default)]
    pub degrade_ratio: DegradeRatio,
    /// Minimum spacing between `guide_focus_degraded` emissions.
    /// Default 10 minutes.
    #[serde(default = "default_watch_cooldown", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub cooldown: Duration,
    /// How long a degradation episode may persist after the degraded
    /// event before `guide_focus_escalation` fires. Default
    /// 10 minutes.
    #[serde(default = "default_watch_escalation", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub escalation_deadline: Duration,
    /// Metrics poll cadence while guiding is active. Default 5 s.
    #[serde(default = "default_watch_poll", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub poll_interval: Duration,
}

impl Default for FocusWatchConfig {
    fn default() -> Self {
        Self {
            window: WatchWindow::default(),
            degrade_ratio: DegradeRatio::default(),
            cooldown: default_watch_cooldown(),
            escalation_deadline: default_watch_escalation(),
            poll_interval: default_watch_poll(),
        }
    }
}

const fn default_watch_cooldown() -> Duration {
    Duration::from_mins(10)
}

const fn default_watch_escalation() -> Duration {
    Duration::from_mins(10)
}

const fn default_watch_poll() -> Duration {
    Duration::from_secs(5)
}

/// The guider's per-frame metrics window (guider.md
/// § `GET /api/v1/guiding/metrics`). Every config value counted in
/// guide frames is bounded by it: the window is the whole supply, so a
/// larger request can never be satisfied.
pub(crate) const GUIDER_METRICS_WINDOW: i64 = 50;

/// The focus watch's median window in frames.
///
/// Parse-don't-validate: a parabola of medians needs history — fewer
/// than 3 frames is noise — and the metrics window holds at most
/// [`GUIDER_METRICS_WINDOW`] frames, so a larger window would leave
/// the watch permanently below its own threshold and silently never
/// fire. Both bounds are rejected at load. Defaults to 10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "i64")]
pub struct WatchWindow(usize);

impl WatchWindow {
    #[must_use]
    pub const fn value(self) -> usize {
        self.0
    }
}

impl Default for WatchWindow {
    fn default() -> Self {
        Self(10)
    }
}

impl TryFrom<i64> for WatchWindow {
    type Error = String;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        match usize::try_from(value) {
            Ok(v) if v >= 3 && value <= GUIDER_METRICS_WINDOW => Ok(Self(v)),
            _ => Err(format!(
                "focus_watch.window must be an integer between 3 and \
                 {GUIDER_METRICS_WINDOW} (the guider metrics window), got {value}"
            )),
        }
    }
}

/// The focus watch's degradation ratio. Must be a finite number
/// strictly above 1.0 — at or below it the watch would fire on
/// noise. Defaults to 1.25.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "f64")]
pub struct DegradeRatio(f64);

impl DegradeRatio {
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

impl Default for DegradeRatio {
    fn default() -> Self {
        Self(1.25)
    }
}

impl TryFrom<f64> for DegradeRatio {
    type Error = String;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if !value.is_finite() || value <= 1.0 {
            return Err(format!(
                "focus_watch.degrade_ratio must be a finite number > 1.0, got {value}"
            ));
        }
        Ok(Self(value))
    }
}

/// Degrees of rotation above which the guiding calibration is cleared.
///
/// Validated at load (parse-don't-validate): must be a finite number
/// in `0..=180` — 0 means "always recalibrate", and a rotator delta
/// never exceeds 180° by shortest path. Serializes transparently as
/// the inner `f64`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "f64")]
pub struct RecalibrateAboveDeg(f64);

impl RecalibrateAboveDeg {
    /// The single validating constructor.
    ///
    /// # Errors
    ///
    /// Returns a message naming the field if `value` is non-finite or
    /// outside `0..=180`.
    pub fn try_new(value: f64) -> Result<Self, String> {
        if !value.is_finite() || !(0.0..=180.0).contains(&value) {
            return Err(format!(
                "recalibrate_above_deg must be a finite number of degrees in 0..=180, got {value}"
            ));
        }
        Ok(Self(value))
    }

    /// The threshold in degrees.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

impl Default for RecalibrateAboveDeg {
    fn default() -> Self {
        Self(5.0)
    }
}

impl TryFrom<f64> for RecalibrateAboveDeg {
    type Error = String;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

/// The guiding defaults carried onto `McpHandler` (parallel to
/// `plate_solver_default_search_radius_deg`).
///
/// Everything from [`GuidingConfig`] the tools and the watches read:
/// the url and the credentials live inside the built client. `Default`
/// (no settle or dither default, the deadlines and the threshold at
/// their defaults) is the not-configured shape tests start from.
#[derive(Debug, Clone, Copy)]
pub struct GuiderDefaults {
    /// The deadline of a request the guider answers at once.
    pub timeout: Duration,
    /// What a `start_guiding` may spend calibrating.
    pub calibration_timeout: Duration,
    /// The Guide Run Watch's `star_lost` threshold.
    pub star_lost_after: Duration,
    pub settle_pixels: Option<f64>,
    pub settle_time: Option<Duration>,
    pub settle_timeout: Option<Duration>,
    pub dither_pixels: Option<f64>,
    /// The rotate-while-guiding ladder's recalibration threshold, in
    /// degrees (rp.md § Rotator Tool Details).
    pub recalibrate_above_deg: f64,
}

impl Default for GuiderDefaults {
    fn default() -> Self {
        Self {
            timeout: default_guiding_timeout(),
            calibration_timeout: default_calibration_timeout(),
            star_lost_after: default_star_lost_after(),
            settle_pixels: None,
            settle_time: None,
            settle_timeout: None,
            dither_pixels: None,
            recalibrate_above_deg: RecalibrateAboveDeg::default().value(),
        }
    }
}

impl GuidingConfig {
    /// The per-call defaults to carry onto the MCP handler.
    #[must_use]
    pub const fn defaults(&self) -> GuiderDefaults {
        GuiderDefaults {
            timeout: self.timeout,
            calibration_timeout: self.calibration_timeout,
            star_lost_after: self.star_lost_after,
            settle_pixels: self.settle_pixels,
            settle_time: self.settle_time,
            settle_timeout: self.settle_timeout,
            dither_pixels: self.dither_pixels,
            recalibrate_above_deg: self.recalibrate_above_deg.value(),
        }
    }
}

/// Default deadline of a request the guider answers at once. It also
/// stands in for a settle timeout rp cannot see, so it sits past the
/// guider's default settle timeout of 60 s.
const fn default_guiding_timeout() -> Duration {
    Duration::from_secs(90)
}

const fn default_calibration_timeout() -> Duration {
    Duration::from_mins(10)
}

const fn default_star_lost_after() -> Duration {
    Duration::from_mins(1)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use super::{FocusWatchConfig, RecalibrateAboveDeg, GUIDER_METRICS_WINDOW};
    use crate::config::load_config;
    use crate::config::test_support::MINIMAL_CONFIG_JSON;

    #[test]
    fn guiding_block_omitted_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, MINIMAL_CONFIG_JSON).unwrap();

        let config = load_config(&path).unwrap();
        assert!(
            config.equipment.mount.is_none(),
            "the minimal config has no mount, hence nowhere to hang guiding"
        );
    }

    #[test]
    fn guiding_url_only_applies_defaults_and_no_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "mount": {
                        "alpaca_url": "http://localhost:11122",
                        "guiding": {"url": "http://127.0.0.1:11130"}
                    }
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let g = config
            .equipment
            .mount
            .as_ref()
            .unwrap()
            .guiding
            .as_ref()
            .expect("guiding should parse");
        assert_eq!(g.url, "http://127.0.0.1:11130");
        assert_eq!(g.timeout, Duration::from_secs(90));
        assert_eq!(g.calibration_timeout, Duration::from_mins(10));
        assert_eq!(g.star_lost_after, Duration::from_mins(1));
        assert!(g.settle_pixels.is_none());
        assert!(g.settle_time.is_none());
        assert!(g.settle_timeout.is_none());
        assert!(g.dither_pixels.is_none());
        assert_eq!(g.recalibrate_above_deg.value(), 5.0);
        assert!(g.auth.is_none());
    }

    #[test]
    fn guiding_with_full_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "mount": {
                        "alpaca_url": "http://localhost:11122",
                        "guiding": {
                            "url": "http://127.0.0.1:11130",
                            "timeout": "2m",
                            "calibration_timeout": "15m",
                            "star_lost_after": "30s",
                            "settle_pixels": 0.8,
                            "settle_time": "8s",
                            "settle_timeout": "40s",
                            "dither_pixels": 5.0,
                            "recalibrate_above_deg": 10.0,
                            "auth": {"username": "observatory", "password": "secret"}
                        }
                    }
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let mount = config.equipment.mount.as_ref().unwrap();
        let g = mount.guiding.as_ref().expect("guiding should parse");
        assert_eq!(g.timeout, Duration::from_mins(2));
        assert_eq!(g.calibration_timeout, Duration::from_mins(15));
        assert_eq!(g.star_lost_after, Duration::from_secs(30));
        assert_eq!(g.settle_pixels, Some(0.8));
        assert_eq!(g.settle_time, Some(Duration::from_secs(8)));
        assert_eq!(g.settle_timeout, Some(Duration::from_secs(40)));
        assert_eq!(g.dither_pixels, Some(5.0));
        assert_eq!(g.recalibrate_above_deg.value(), 10.0);
        let auth = g.auth.as_ref().unwrap();
        assert_eq!(auth.username, "observatory");
        assert_eq!(auth.password, "secret");

        let defaults = g.defaults();
        assert_eq!(defaults.timeout, Duration::from_mins(2));
        assert_eq!(defaults.calibration_timeout, Duration::from_mins(15));
        assert_eq!(defaults.star_lost_after, Duration::from_secs(30));
        assert_eq!(defaults.settle_pixels, Some(0.8));
        assert_eq!(defaults.settle_time, Some(Duration::from_secs(8)));
        assert_eq!(defaults.settle_timeout, Some(Duration::from_secs(40)));
        assert_eq!(defaults.dither_pixels, Some(5.0));
    }

    #[test]
    fn guiding_rejects_out_of_range_recalibrate_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "mount": {
                        "alpaca_url": "http://localhost:11122",
                        "guiding": {
                            "url": "http://127.0.0.1:11130",
                            "recalibrate_above_deg": 181.0
                        }
                    }
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err().to_string();
        assert!(
            err.contains("recalibrate_above_deg must be a finite number of degrees in 0..=180"),
            "{err}"
        );
    }

    #[test]
    fn guiding_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "mount": {
                        "alpaca_url": "http://localhost:11122",
                        "guiding": {
                            "url": "http://127.0.0.1:11130",
                            "dither_every_n_exposures": 3
                        }
                    }
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("dither_every_n_exposures") || msg.contains("unknown field"),
            "expected unknown-field diagnostic, got: {msg}"
        );
    }

    #[test]
    fn focus_watch_defaults_and_newtype_boundaries() {
        let watch = FocusWatchConfig::default();
        assert_eq!(watch.window.value(), 10);
        assert_eq!(watch.degrade_ratio.value(), 1.25);
        assert_eq!(watch.cooldown, Duration::from_mins(10));
        assert_eq!(watch.escalation_deadline, Duration::from_mins(10));
        assert_eq!(watch.poll_interval, Duration::from_secs(5));

        let parsed: FocusWatchConfig = serde_json::from_value(serde_json::json!({
            "window": 3, "degrade_ratio": 1.5, "cooldown": "1m",
            "escalation_deadline": "2m", "poll_interval": "250ms"
        }))
        .unwrap();
        assert_eq!(parsed.window.value(), 3);
        assert_eq!(parsed.degrade_ratio.value(), 1.5);
        assert_eq!(parsed.poll_interval, Duration::from_millis(250));

        // The upper bound is the guider's metrics window: a window past it
        // could never be filled, so the watch would never fire.
        let at_cap: FocusWatchConfig =
            serde_json::from_value(serde_json::json!({ "window": GUIDER_METRICS_WINDOW })).unwrap();
        assert_eq!(at_cap.window.value(), 50);

        for bad_window in [2, GUIDER_METRICS_WINDOW + 1] {
            let err = serde_json::from_value::<FocusWatchConfig>(
                serde_json::json!({ "window": bad_window }),
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("focus_watch.window must be an integer between 3 and 50"),
                "window {bad_window} gave: {err}"
            );
        }
        for bad_ratio in [1.0, 0.5, f64::NAN] {
            let err = serde_json::from_value::<FocusWatchConfig>(
                serde_json::json!({ "degrade_ratio": bad_ratio }),
            );
            assert!(err.is_err(), "degrade_ratio {bad_ratio} must be rejected");
        }
    }

    #[test]
    fn recalibrate_above_deg_newtype_validation_boundaries() {
        assert_eq!(RecalibrateAboveDeg::default().value(), 5.0);
        assert_eq!(RecalibrateAboveDeg::try_new(0.0).unwrap().value(), 0.0);
        assert_eq!(RecalibrateAboveDeg::try_new(180.0).unwrap().value(), 180.0);
        assert!(RecalibrateAboveDeg::try_new(-0.1)
            .unwrap_err()
            .contains("recalibrate_above_deg"));
        assert!(RecalibrateAboveDeg::try_new(180.1).is_err());
        assert!(RecalibrateAboveDeg::try_new(f64::NAN).is_err());
        assert!(RecalibrateAboveDeg::try_new(f64::INFINITY).is_err());
    }
}
