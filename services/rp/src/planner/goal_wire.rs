//! Wire-format parsing for [`rp_targets::AcquisitionGoal`].
//!
//! This is the JSON shape `add_target`/`set_goals`/`targets.default_goals`
//! (config) all share — `binning` as `"AxB"` and `exposure_duration` as
//! a humantime string (`"5m"`). `AcquisitionGoal`'s **derived**
//! `Serialize` produces
//! this exact shape (`Binning` serializes as its canonical string,
//! exposure via `humantime_serde`), so the output side needs no
//! conversion at all; [`GoalWire`] and its `TryFrom` into
//! `AcquisitionGoal` remain the *input* parser for friendlier per-field
//! errors than raw serde's, and a test here locks the two shapes
//! together. Shared by
//! [`crate::mcp::built_in::targets`] (the MCP tool bodies) and
//! [`crate::config::target_store`] (parsing `targets.default_goals`) so
//! the two stay byte-for-byte consistent.

use rp_targets::{AcquisitionGoal, Binning};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The wire shape of one `goals[]` entry, as accepted by `add_target`,
/// `set_goals`, and `targets.default_goals` in config.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct GoalWire {
    pub filter: String,
    /// `"AxB"`, e.g. `"1x1"`, `"2x2"`.
    pub binning: String,
    /// A humantime duration string, e.g. `"5m"` (`"300s"` is accepted too;
    /// humantime rolls whole minutes up, so it reads back as `"5m"`).
    pub exposure_duration: String,
    pub desired_count: u32,
}

impl TryFrom<&GoalWire> for AcquisitionGoal {
    /// A human-readable message naming the offending value — friendlier
    /// than raw serde's, which is why the wire deserializes into
    /// [`GoalWire`] first instead of straight into `AcquisitionGoal`.
    type Error = String;

    /// Parses one wire-format goal, rejecting a `binning` that isn't
    /// `"AxB"` (per [`Binning`]'s `FromStr`) or an `exposure_duration`
    /// that isn't a valid humantime string.
    fn try_from(g: &GoalWire) -> Result<Self, Self::Error> {
        Ok(Self {
            filter: g.filter.clone(),
            binning: g.binning.parse::<Binning>().map_err(|e| e.to_string())?,
            exposure_duration: humantime::parse_duration(&g.exposure_duration)
                .map_err(|e| format!("goal exposure_duration {:?}: {e}", g.exposure_duration))?,
            desired_count: g.desired_count,
        })
    }
}

/// The inverse of the `TryFrom` above. Building the [`GoalWire`] struct —
/// rather than a hand-keyed `json!` map — makes the field names
/// compile-checked, and `GoalWire`'s derived `Serialize` produces the
/// JSON. `exposure_duration` uses [`humantime::format_duration`] — the
/// exact encoding `AcquisitionGoal`'s `humantime_serde` field uses for
/// the redb store, so the MCP wire and the store agree (a 300 s sub is
/// `"5m"`, a 32 µs bias is `"32us"`; the file-naming template renders the
/// same string with humantime's inter-unit spaces removed).
impl From<&AcquisitionGoal> for GoalWire {
    fn from(g: &AcquisitionGoal) -> Self {
        Self {
            filter: g.filter.clone(),
            binning: g.binning.to_string(),
            exposure_duration: humantime::format_duration(g.exposure_duration).to_string(),
            desired_count: g.desired_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    fn wire(filter: &str, binning: &str, exposure_duration: &str, desired_count: u32) -> GoalWire {
        GoalWire {
            filter: filter.to_string(),
            binning: binning.to_string(),
            exposure_duration: exposure_duration.to_string(),
            desired_count,
        }
    }

    #[test]
    fn try_from_wire_round_trips_exposure_duration_through_humantime() {
        // Wire in as `300s`, back out as humantime's `5m` — the same
        // rollup `humantime_serde` applies for the store, so the wire and
        // the store agree. The `Duration` itself round-trips exactly.
        let goal = AcquisitionGoal::try_from(&wire("Ha", "1x1", "300s", 20)).unwrap();
        assert_eq!(goal.binning, Binning { x: 1, y: 1 });
        assert_eq!(goal.exposure_duration, Duration::from_mins(5));
        assert_eq!(
            serde_json::to_value(GoalWire::from(&goal)).unwrap(),
            json!({
                "filter": "Ha", "binning": "1x1", "exposure_duration": "5m", "desired_count": 20
            })
        );
    }

    #[test]
    fn derived_goal_serde_matches_the_goal_wire_shape() {
        // The MCP output contract is `AcquisitionGoal`'s derived
        // `Serialize` (targets.rs `target_to_json`); the input contract is
        // `GoalWire`. This locks the two shapes together so a field rename
        // or encoding change in either one fails here instead of drifting.
        let goal = AcquisitionGoal::try_from(&wire("Ha", "2x2", "5m", 20)).unwrap();
        assert_eq!(
            serde_json::to_value(&goal).unwrap(),
            serde_json::to_value(GoalWire::from(&goal)).unwrap()
        );
    }

    #[test]
    fn wire_from_goal_encodes_a_sub_second_bias_exposure() {
        // The motivating case: a sub-second exposure the old whole-second
        // encoding could not represent now survives the wire.
        let goal = AcquisitionGoal::try_from(&wire("Dark", "1x1", "32us", 50)).unwrap();
        assert_eq!(goal.exposure_duration, Duration::from_micros(32));
        assert_eq!(GoalWire::from(&goal).exposure_duration, "32us");
    }

    #[test]
    fn try_from_wire_rejects_malformed_binning() {
        let err = AcquisitionGoal::try_from(&wire("Ha", "1", "300s", 20)).unwrap_err();
        assert!(err.contains("binning"), "{err}");
    }

    #[test]
    fn try_from_wire_rejects_malformed_exposure_duration() {
        let err = AcquisitionGoal::try_from(&wire("Ha", "1x1", "not-a-duration", 20)).unwrap_err();
        assert!(err.contains("exposure_duration"), "{err}");
    }
}
