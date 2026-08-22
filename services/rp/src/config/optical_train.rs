use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::guiding::GUIDER_METRICS_WINDOW;

/// What a train is for. The guiding train tells rp which camera's
/// focus and rotation state the guider depends on; everything else is
/// an imaging train.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TrainPurpose {
    #[default]
    Imaging,
    Guiding,
}

/// Effective focal length of a light path in millimetres.
///
/// Validated at load (parse-don't-validate): a non-finite or
/// non-positive value is rejected during deserialization, so a bad
/// config fails at startup rather than at capture time. Serializes
/// transparently as the inner `f64` (and the JSON Schema is a plain
/// number), matching the `try_from = "f64"` deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "f64")]
pub struct FocalLengthMm(f64);

impl FocalLengthMm {
    /// The single validating constructor.
    ///
    /// # Errors
    ///
    /// Returns a message naming the field if `value` is non-finite or
    /// not positive.
    pub fn try_new(value: f64) -> Result<Self, String> {
        if !value.is_finite() || value <= 0.0 {
            return Err(format!(
                "focal_length_mm must be a positive finite number, got {value}"
            ));
        }
        Ok(Self(value))
    }

    /// The focal length in millimetres.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for FocalLengthMm {
    type Error = String;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

/// A train's default framing angle in degrees east of north, sky frame
/// (`optical_trains[].default_position_angle_degrees`).
///
/// Layer two of the effective position angle (rp.md § Target Store →
/// Position angle). Same domain as `move_rotator`'s `angle`: `0.0 ≤
/// angle < 360.0`, finite, rejected at load otherwise
/// (parse-don't-validate). Serializes transparently as the inner `f64`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "f64")]
pub struct PositionAngleDegrees(f64);

impl PositionAngleDegrees {
    /// The single validating constructor, shared with the per-target
    /// field's boundary check in the target MCP tools.
    ///
    /// # Errors
    ///
    /// Returns a message if `value` is non-finite or outside `0.0..360.0`.
    pub fn try_new(value: f64) -> Result<Self, String> {
        if !value.is_finite() || !(0.0..360.0).contains(&value) {
            return Err(format!(
                "position angle must be a finite number of degrees at least 0.0 \
                 and below 360.0, got {value}"
            ));
        }
        Ok(Self(value))
    }

    /// The angle in degrees east of north.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for PositionAngleDegrees {
    type Error = String;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

/// A positive focuser-step count for the V-curve sweep grid
/// (`auto_focus.step_size`). Parse-don't-validate: zero, negative,
/// and i32-overflowing values are rejected at deserialize with the
/// field named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "i64")]
pub struct SweepStepSize(i32);

impl SweepStepSize {
    #[must_use]
    pub const fn value(self) -> i32 {
        self.0
    }
}

impl TryFrom<i64> for SweepStepSize {
    type Error = String;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        match i32::try_from(value) {
            Ok(v) if v > 0 => Ok(Self(v)),
            _ => Err(format!(
                "auto_focus.step_size must be a positive integer, got {value}"
            )),
        }
    }
}

/// A positive sweep half-width in focuser steps
/// (`auto_focus.half_width`), validated like [`SweepStepSize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "i64")]
pub struct SweepHalfWidth(i32);

impl SweepHalfWidth {
    #[must_use]
    pub const fn value(self) -> i32 {
        self.0
    }
}

impl TryFrom<i64> for SweepHalfWidth {
    type Error = String;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        match i32::try_from(value) {
            Ok(v) if v > 0 => Ok(Self(v)),
            _ => Err(format!(
                "auto_focus.half_width must be a positive integer, got {value}"
            )),
        }
    }
}

/// A positive fresh-frame count per metric-sweep position
/// (`auto_focus.frames_per_step`), validated like [`SweepStepSize`].
///
/// Capped at the guider service's 50-entry metrics window — a larger
/// value could never be satisfied and would guarantee per-position
/// timeouts, so the misconfiguration fails at load instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "i64")]
pub struct FramesPerStep(u32);

impl FramesPerStep {
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl TryFrom<i64> for FramesPerStep {
    type Error = String;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        match u32::try_from(value) {
            Ok(v) if v > 0 && i64::from(v) <= GUIDER_METRICS_WINDOW => Ok(Self(v)),
            _ => Err(format!(
                "auto_focus.frames_per_step must be a positive integer at most \
                 {GUIDER_METRICS_WINDOW} (the guider metrics window), got {value}"
            )),
        }
    }
}

/// Per-train V-curve sweep parameters (`optical_trains[].auto_focus`,
/// rp.md § Optical Trains).
///
/// Which fields apply depends on the train's
/// purpose — imaging trains run the capture sweep (`duration`,
/// `min_area`, `max_area` required, `threshold_sigma` optional), the
/// guiding train the PHD2-metric sweep (`frames_per_step` optional;
/// the capture fields rejected) — enforced with dotted-path errors in
/// [`crate::equipment::trains::TrainModel::try_from_equipment`], so
/// everything purpose-dependent is `Option` at the serde level. Backs
/// train-addressed `auto_focus` calls (per-call parameters override
/// field by field) and is required on every train a `refocus_train`
/// expansion runs in.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TrainAutoFocusConfig {
    /// Per-frame exposure for every point of a capture sweep.
    #[serde(default, with = "humantime_serde::option")]
    #[schemars(with = "Option<String>")]
    pub duration: Option<Duration>,
    pub step_size: SweepStepSize,
    pub half_width: SweepHalfWidth,
    /// Minimum component pixel area for the per-frame `measure_basic`
    /// (capture sweeps).
    #[serde(default)]
    pub min_area: Option<usize>,
    /// Maximum component pixel area for the per-frame `measure_basic`
    /// (capture sweeps).
    #[serde(default)]
    pub max_area: Option<usize>,
    /// Per-frame `measure_basic` threshold in sigma units (capture
    /// sweeps). Omitted → the tool default (5.0).
    #[serde(default)]
    pub threshold_sigma: Option<f64>,
    /// Minimum valid samples for the parabolic fit. Omitted → the
    /// tool default (5). Applies to both sweep variants.
    #[serde(default)]
    pub min_fit_points: Option<usize>,
    /// Fresh guide frames per metric-sweep position (guiding train
    /// only). Omitted → the default (3).
    #[serde(default)]
    pub frames_per_step: Option<FramesPerStep>,
}

/// One `equipment.optical_trains[]` entry (rp.md § Optical Trains): an
/// ordered list of roster device ids, objective side first,
/// terminating in a camera.
///
/// Membership expresses coupling, position
/// expresses optical order. The cross-array graph rules (roster
/// existence, terminal camera, order consistency, the
/// one-guiding-train rule) live in
/// [`crate::equipment::trains::TrainModel::try_from_equipment`],
/// shared by `load_config` and `PUT /api/config`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpticalTrainConfig {
    pub id: String,
    #[serde(default)]
    pub purpose: TrainPurpose,
    /// Effective focal length of this light path. Omitted → captures
    /// through this train's camera carry no `optics` block, exactly
    /// like a camera outside any train.
    #[serde(default)]
    pub focal_length_mm: Option<FocalLengthMm>,
    /// Default framing angle for targets that don't carry their own —
    /// layer two of the effective position angle (rp.md § Target
    /// Store → Position angle). For a rotator-less train this
    /// documents the camera's fixed mounting angle. Omitted → the
    /// fallback skips to 0.0 north-up.
    #[serde(default)]
    pub default_position_angle_degrees: Option<PositionAngleDegrees>,
    /// Roster device ids, objective side first. The last entry must be
    /// a camera; the rest are focusers, rotators, and filter wheels.
    pub devices: Vec<String>,
    /// V-curve sweep parameters for focusing this train. Omitted → the
    /// train cannot be auto-focused by `refocus_train`, and
    /// train-addressed `auto_focus` calls must pass every sweep
    /// parameter per call.
    #[serde(default)]
    pub auto_focus: Option<TrainAutoFocusConfig>,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::load_config;

    fn write_config(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn optical_train_minimal_defaults_to_imaging_without_focal_length() {
        let (_dir, path) = write_config(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {"id": "main-cam", "alpaca_url": "http://localhost:11120"}
                    ],
                    "optical_trains": [
                        {"id": "main", "devices": ["main-cam"]}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        );

        let config = load_config(&path).unwrap();
        let train = &config.equipment.optical_trains[0];
        assert_eq!(train.id, "main");
        assert_eq!(train.purpose, TrainPurpose::Imaging);
        assert!(train.focal_length_mm.is_none());
        assert_eq!(train.devices, vec!["main-cam"]);
    }

    #[test]
    fn optical_train_full_entry_round_trips() {
        let (_dir, path) = write_config(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {"id": "guide-cam", "alpaca_url": "http://localhost:11121"}
                    ],
                    "focusers": [
                        {"id": "guide-focuser", "alpaca_url": "http://localhost:11113"}
                    ],
                    "mount": {
                        "alpaca_url": "http://localhost:11122",
                        "guiding": {"url": "http://localhost:11130"}
                    },
                    "optical_trains": [
                        {
                            "id": "guide",
                            "purpose": "guiding",
                            "focal_length_mm": 200.0,
                            "devices": ["guide-focuser", "guide-cam"]
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        );

        let config = load_config(&path).unwrap();
        let train = &config.equipment.optical_trains[0];
        assert_eq!(train.purpose, TrainPurpose::Guiding);
        assert_eq!(train.focal_length_mm.map(FocalLengthMm::value), Some(200.0));

        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(
            value.pointer("/equipment/optical_trains/0/purpose"),
            Some(&serde_json::json!("guiding"))
        );
        assert_eq!(
            value.pointer("/equipment/optical_trains/0/focal_length_mm"),
            Some(&serde_json::json!(200.0))
        );
    }

    #[test]
    fn optical_train_rejects_unknown_purpose_at_parse() {
        let (_dir, path) = write_config(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "optical_trains": [
                        {"id": "main", "purpose": "solar", "devices": ["main-cam"]}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        );

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("unknown variant `solar`"), "{err}");
    }

    #[test]
    fn optical_train_rejects_non_positive_focal_length_at_parse() {
        let (_dir, path) = write_config(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "optical_trains": [
                        {"id": "main", "focal_length_mm": -100.0, "devices": ["main-cam"]}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        );

        let err = load_config(&path).unwrap_err().to_string();
        assert!(
            err.contains("focal_length_mm must be a positive finite number"),
            "{err}"
        );
    }

    #[test]
    fn optical_train_accepts_a_default_position_angle_in_range() {
        let (_dir, path) = write_config(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {"id": "main-cam", "alpaca_url": "http://localhost:11120"}
                    ],
                    "optical_trains": [
                        {"id": "main", "default_position_angle_degrees": 254.0,
                         "devices": ["main-cam"]}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        );

        let config = load_config(&path).unwrap();
        let train = &config.equipment.optical_trains[0];
        assert_eq!(
            train
                .default_position_angle_degrees
                .map(PositionAngleDegrees::value),
            Some(254.0)
        );
    }

    #[test]
    fn optical_train_rejects_an_out_of_range_default_position_angle_at_parse() {
        let (_dir, path) = write_config(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "optical_trains": [
                        {"id": "main", "default_position_angle_degrees": 360.0,
                         "devices": ["main-cam"]}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        );

        let err = load_config(&path).unwrap_err().to_string();
        assert!(
            err.contains("position angle must be a finite number of degrees"),
            "{err}"
        );
    }

    #[test]
    fn optical_train_rejects_unknown_field() {
        let (_dir, path) = write_config(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "optical_trains": [
                        {"id": "main", "devices": ["main-cam"], "camera_id": "main-cam"}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        );

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("camera_id"), "{err}");
    }

    #[test]
    fn optical_train_auto_focus_block_round_trips() {
        let (_dir, path) = write_config(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {"id": "main-cam", "alpaca_url": "http://localhost:11120"}
                    ],
                    "optical_trains": [
                        {"id": "main", "devices": ["main-cam"],
                         "auto_focus": {"duration": "3s", "step_size": 100,
                                        "half_width": 1000, "min_area": 4,
                                        "max_area": 500}}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        );

        let config = load_config(&path).unwrap();
        let block = config.equipment.optical_trains[0]
            .auto_focus
            .as_ref()
            .unwrap();
        assert_eq!(block.duration, Some(Duration::from_secs(3)));
        assert_eq!(block.step_size.value(), 100);
        assert_eq!(block.half_width.value(), 1000);
        assert_eq!(block.min_area, Some(4));
        assert_eq!(block.max_area, Some(500));
        assert!(block.threshold_sigma.is_none());
        assert!(block.min_fit_points.is_none());
        assert!(block.frames_per_step.is_none());

        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(
            value.pointer("/equipment/optical_trains/0/auto_focus/duration"),
            Some(&serde_json::json!("3s"))
        );
        assert_eq!(
            value.pointer("/equipment/optical_trains/0/auto_focus/step_size"),
            Some(&serde_json::json!(100))
        );
    }

    #[test]
    fn auto_focus_block_rejects_non_positive_sweep_fields_at_parse() {
        for (field, bad, named) in [
            (
                "step_size",
                serde_json::json!(0),
                "auto_focus.step_size must be a positive integer",
            ),
            (
                "half_width",
                serde_json::json!(-5),
                "auto_focus.half_width must be a positive integer",
            ),
        ] {
            let mut config = serde_json::json!({
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "optical_trains": [
                        {"id": "main", "devices": ["main-cam"],
                         "auto_focus": {"duration": "3s", "step_size": 100,
                                        "half_width": 1000, "min_area": 4,
                                        "max_area": 500}}
                    ]
                },
                "server": { "port": 0 }
            });
            config["equipment"]["optical_trains"][0]["auto_focus"][field] = bad;
            let (_dir, path) = write_config(&config.to_string());

            let err = load_config(&path).unwrap_err().to_string();
            assert!(err.contains(named), "{field}: {err}");
        }
    }

    #[test]
    fn auto_focus_block_rejects_unknown_field() {
        let (_dir, path) = write_config(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "optical_trains": [
                        {"id": "main", "devices": ["main-cam"],
                         "auto_focus": {"duration": "3s", "step_size": 100,
                                        "half_width": 1000, "min_area": 4,
                                        "max_area": 500, "sweep_mode": "fast"}}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        );

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("sweep_mode"), "{err}");
    }

    #[test]
    fn sweep_newtype_validation_boundaries() {
        assert_eq!(SweepStepSize::try_from(1i64).unwrap().value(), 1);
        assert!(SweepStepSize::try_from(0i64)
            .unwrap_err()
            .contains("auto_focus.step_size"));
        assert!(SweepStepSize::try_from(i64::from(i32::MAX) + 1).is_err());
        assert_eq!(SweepHalfWidth::try_from(200i64).unwrap().value(), 200);
        assert!(SweepHalfWidth::try_from(-1i64)
            .unwrap_err()
            .contains("auto_focus.half_width"));
        assert_eq!(FramesPerStep::try_from(3i64).unwrap().value(), 3);
        assert_eq!(FramesPerStep::try_from(50i64).unwrap().value(), 50);
        assert!(FramesPerStep::try_from(0i64)
            .unwrap_err()
            .contains("auto_focus.frames_per_step"));
        assert!(FramesPerStep::try_from(-2i64).is_err());
        assert!(FramesPerStep::try_from(51i64)
            .unwrap_err()
            .contains("guider metrics window"));
    }

    #[test]
    fn focal_length_newtype_validation_boundaries() {
        assert_eq!(FocalLengthMm::try_new(360.0).unwrap().value(), 360.0);
        // The `<= 0.0` edge and the non-finite branch (unreachable from
        // JSON, defensive-only) are rejected and name the field.
        assert!(FocalLengthMm::try_new(0.0)
            .unwrap_err()
            .contains("focal_length_mm"));
        assert!(FocalLengthMm::try_new(-1.0).is_err());
        assert!(FocalLengthMm::try_new(f64::NAN).is_err());
        assert!(FocalLengthMm::try_new(f64::INFINITY).is_err());
    }
}
