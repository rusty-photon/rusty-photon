use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Per-rig estimates feeding the predictive `center_on_target` deadline
/// (§2.5 of the predictive-deadlines plan).
///
/// The watchdog tracks only the outer centering loop — each per-iteration
/// `slew` / `capture` carries its own deadline — so the outer
/// `centering_started` envelope advertises:
///
/// ```text
/// per_iter  = capture_duration + solve_time_estimate + slew_overhead_estimate
/// predicted = per_iter                    // optimistic single-pass convergence
/// max       = max_attempts × per_iter     // every attempt used
/// ```
///
/// `capture_duration` is the operator's per-iteration `duration` parameter;
/// the two estimates below are config because neither plate-solve wall-clock
/// nor per-iteration slew+settle overhead is knowable from an ASCOM property.
/// The deadline is **advisory** — rp does not enforce it; it rides the
/// envelope for the Sentinel watchdog. Omitted block → both defaults apply.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CenteringConfig {
    /// Expected wall-clock time for one plate solve, feeding the centering
    /// deadline. This is the *expected* solve time used to size the
    /// watchdog deadline — distinct from `plate_solver.timeout`, which is
    /// the hard per-solve ceiling. Defaults to 30 s (a conservative ASTAP
    /// blind-ish solve); set per-rig. Accepts a humantime string.
    #[serde(default = "default_solve_time_estimate", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub solve_time_estimate: Duration,
    /// Expected per-iteration slew + settle overhead, feeding the centering
    /// deadline. A centering correction slew is small, so this is short;
    /// defaults to 10 s. Accepts a humantime string.
    #[serde(default = "default_slew_overhead_estimate", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub slew_overhead_estimate: Duration,
}

impl Default for CenteringConfig {
    fn default() -> Self {
        Self {
            solve_time_estimate: default_solve_time_estimate(),
            slew_overhead_estimate: default_slew_overhead_estimate(),
        }
    }
}

const fn default_solve_time_estimate() -> Duration {
    Duration::from_secs(30)
}

const fn default_slew_overhead_estimate() -> Duration {
    Duration::from_secs(10)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use crate::config::load_config;
    use crate::config::test_support::MINIMAL_CONFIG_JSON;

    #[test]
    fn centering_block_omitted_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, MINIMAL_CONFIG_JSON).unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(
            config.centering.solve_time_estimate,
            Duration::from_secs(30)
        );
        assert_eq!(
            config.centering.slew_overhead_estimate,
            Duration::from_secs(10)
        );
    }

    #[test]
    fn centering_block_overrides_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "centering": {
                    "solve_time_estimate": "12s",
                    "slew_overhead_estimate": "3s"
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(
            config.centering.solve_time_estimate,
            Duration::from_secs(12)
        );
        assert_eq!(
            config.centering.slew_overhead_estimate,
            Duration::from_secs(3)
        );
    }

    #[test]
    fn centering_block_partial_override_keeps_other_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "centering": {"solve_time_estimate": "45s"},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(
            config.centering.solve_time_estimate,
            Duration::from_secs(45)
        );
        assert_eq!(
            config.centering.slew_overhead_estimate,
            Duration::from_secs(10),
            "omitted slew_overhead_estimate keeps its default"
        );
    }

    #[test]
    fn centering_block_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "centering": {"bogus_field": 1},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bogus_field") || msg.contains("unknown field"),
            "expected unknown-field diagnostic, got: {msg}"
        );
    }
}
