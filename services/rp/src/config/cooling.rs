use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Tuning knobs for the camera-cooling controller (rp.md § Camera
/// Cooling → Tuning).
///
/// The which-temperatures question lives per camera
/// (`equipment.cameras[].cooler_targets_c`); this block only shapes how
/// the single cooldown pass detects stabilization and plateaus, and how
/// the end-of-session warm-up ramps. Every field has a default and the
/// block is normally omitted; the BDD harness pins the timing knobs
/// short so a pass completes in test time.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoolingConfig {
    /// Cadence of `CCDTemperature`/`CoolerPower` polling during the
    /// cooldown pass. Accepts a humantime string.
    #[serde(default = "default_poll_interval", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub poll_interval: Duration,
    /// How long a trajectory must persist to count as stable (at the
    /// rung) or plateaued (above it). Accepts a humantime string.
    #[serde(default = "default_plateau_window", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub plateau_window: Duration,
    /// Total movement below this across a full `plateau_window` counts
    /// as a plateau (°C).
    #[serde(default = "default_plateau_threshold_c")]
    pub plateau_threshold_c: f64,
    /// "At the rung" means within this of the setpoint (°C).
    #[serde(default = "default_tolerance_c")]
    pub tolerance_c: f64,
    /// Stabilization requires cooler power at or below this (percent) —
    /// a rung held at pegged power has no regulation authority left.
    /// Ignored when the camera cannot report power.
    #[serde(default = "default_max_cooler_power_pct")]
    pub max_cooler_power_pct: f64,
    /// The chosen rung must sit at least this far above the measured
    /// floor (°C).
    #[serde(default = "default_regulation_margin_c")]
    pub regulation_margin_c: f64,
    /// Hard bound on the whole selection pass; on expiry the current
    /// temperature is treated as the floor. Accepts a humantime string.
    #[serde(default = "default_max_cooldown", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub max_cooldown: Duration,
    /// Time between +5 °C warm-up steps at session end. Accepts a
    /// humantime string.
    #[serde(default = "default_warmup_step_interval", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub warmup_step_interval: Duration,
    /// Warm-up endpoint when the camera does not report
    /// `HeatSinkTemperature` (°C).
    #[serde(default = "default_warm_target_c")]
    pub warm_target_c: f64,
}

impl Default for CoolingConfig {
    fn default() -> Self {
        Self {
            poll_interval: default_poll_interval(),
            plateau_window: default_plateau_window(),
            plateau_threshold_c: default_plateau_threshold_c(),
            tolerance_c: default_tolerance_c(),
            max_cooler_power_pct: default_max_cooler_power_pct(),
            regulation_margin_c: default_regulation_margin_c(),
            max_cooldown: default_max_cooldown(),
            warmup_step_interval: default_warmup_step_interval(),
            warm_target_c: default_warm_target_c(),
        }
    }
}

const fn default_poll_interval() -> Duration {
    Duration::from_secs(10)
}

const fn default_plateau_window() -> Duration {
    Duration::from_mins(2)
}

const fn default_plateau_threshold_c() -> f64 {
    0.5
}

const fn default_tolerance_c() -> f64 {
    1.0
}

const fn default_max_cooler_power_pct() -> f64 {
    90.0
}

const fn default_regulation_margin_c() -> f64 {
    3.0
}

const fn default_max_cooldown() -> Duration {
    Duration::from_mins(20)
}

const fn default_warmup_step_interval() -> Duration {
    Duration::from_mins(2)
}

const fn default_warm_target_c() -> f64 {
    10.0
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use crate::config::load_config;
    use crate::config::test_support::MINIMAL_CONFIG_JSON;

    #[test]
    fn cooling_block_omitted_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, MINIMAL_CONFIG_JSON).unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.cooling.poll_interval, Duration::from_secs(10));
        assert_eq!(config.cooling.plateau_window, Duration::from_mins(2));
        assert_eq!(config.cooling.plateau_threshold_c, 0.5);
        assert_eq!(config.cooling.tolerance_c, 1.0);
        assert_eq!(config.cooling.max_cooler_power_pct, 90.0);
        assert_eq!(config.cooling.regulation_margin_c, 3.0);
        assert_eq!(config.cooling.max_cooldown, Duration::from_mins(20));
        assert_eq!(config.cooling.warmup_step_interval, Duration::from_mins(2));
        assert_eq!(config.cooling.warm_target_c, 10.0);
    }

    #[test]
    fn cooling_block_partial_override_keeps_other_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "cooling": {"poll_interval": "250ms", "plateau_window": "1s"},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.cooling.poll_interval, Duration::from_millis(250));
        assert_eq!(config.cooling.plateau_window, Duration::from_secs(1));
        assert_eq!(
            config.cooling.tolerance_c, 1.0,
            "omitted tolerance_c keeps its default"
        );
    }

    #[test]
    fn cooling_block_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "cooling": {"bogus_knob": 1},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bogus_knob") || msg.contains("unknown field"),
            "expected unknown-field diagnostic, got: {msg}"
        );
    }
}
