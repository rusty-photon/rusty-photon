use rusty_photon_config::driver::FieldError;
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

/// Clear aperture of a light path in millimetres
/// (`optical_trains[].aperture_mm`, rp.md § Train optics).
///
/// Validated at load like [`FocalLengthMm`]; serializes as the inner
/// `f64`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "f64")]
pub struct ApertureMm(f64);

impl ApertureMm {
    /// The single validating constructor.
    ///
    /// # Errors
    ///
    /// Returns a message naming the field if `value` is non-finite or
    /// not positive.
    pub fn try_new(value: f64) -> Result<Self, String> {
        if !value.is_finite() || value <= 0.0 {
            return Err(format!(
                "aperture_mm must be a positive finite number, got {value}"
            ));
        }
        Ok(Self(value))
    }

    /// The aperture in millimetres.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for ApertureMm {
    type Error = String;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

/// Central obstruction of a light path, a diameter in millimetres
/// (`optical_trains[].obstruction_mm`, rp.md § Train optics).
///
/// Validated at load like [`ApertureMm`], except that 0 is allowed: it
/// states an unobstructed path outright. That it is smaller than the
/// train's aperture is a rule across two fields, checked by
/// [`OpticalTrainConfig::field_errors`]. Serializes as the inner `f64`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "f64")]
pub struct ObstructionMm(f64);

impl ObstructionMm {
    /// The single validating constructor.
    ///
    /// # Errors
    ///
    /// Returns a message naming the field if `value` is non-finite or
    /// negative.
    pub fn try_new(value: f64) -> Result<Self, String> {
        if !value.is_finite() || value < 0.0 {
            return Err(format!(
                "obstruction_mm must be a finite number, not negative, got {value}"
            ));
        }
        Ok(Self(value))
    }

    /// The obstruction's diameter in millimetres.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for ObstructionMm {
    type Error = String;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

/// The telescope a train looks through (`optical_trains[].telescope`,
/// rp.md § Optical Trains), e.g. `Takahashi FSQ-106EDX4` — the FITS
/// header's `TELESCOP`.
///
/// Validated at load to what one FITS string card can carry once its
/// typographic dashes, quotes and spaces fold to ASCII, as every
/// free-text card's do: 1 to 68 printable ASCII characters, an
/// apostrophe counting twice because FITS doubles it. A name the card
/// could not hold is a load error rather than a header card silently
/// dropped at capture time. The name itself is kept as written; the
/// sidecar's `telescope` carries it unfolded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct TelescopeName(String);

impl TelescopeName {
    /// The single validating constructor.
    ///
    /// # Errors
    ///
    /// Returns a message naming the field if `value` is empty, carries
    /// a character outside printable ASCII that does not fold, or is
    /// too long for one FITS string card.
    pub fn try_new(value: String) -> Result<Self, String> {
        if value.trim().is_empty() {
            return Err("telescope must not be empty; omit it to leave TELESCOP out".into());
        }
        // The writer's own rule on the value the card will carry — folded
        // like any free text, so an editor's en dash is not a startup
        // failure — so config and header cannot drift apart.
        rp_fits::writer::Keyword::new(
            "TELESCOP",
            rp_fits::writer::KeywordValue::Str(rp_fits::writer::fold_to_ascii(&value)),
        )
        .map_err(|e| {
            // The refusal is about the value, not the keyword: drop the
            // writer's "invalid keyword" framing.
            let reason = match e {
                rp_fits::FitsError::InvalidKeyword(reason) => reason,
                other => other.to_string(),
            };
            format!("telescope must fit one FITS header card: {reason}; got {value:?}")
        })?;
        Ok(Self(value))
    }

    /// The telescope's name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TelescopeName {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<TelescopeName> for String {
    fn from(name: TelescopeName) -> Self {
        name.0
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

/// The confirmation tolerance of the metric sweep
/// (`auto_focus.confirmation_tolerance`).
///
/// How much worse than the lowest valid sweep sample the
/// confirmation measurement may be, as a fraction, before the fitted
/// position is rejected in favour of that sample's position. Finite,
/// non-negative.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "f64")]
pub struct ConfirmationTolerance(f64);

impl ConfirmationTolerance {
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for ConfirmationTolerance {
    type Error = String;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if crate::imaging::tools::auto_focus::valid_confirmation_tolerance(value) {
            Ok(Self(value))
        } else {
            Err(format!(
                "auto_focus.confirmation_tolerance must be a finite number of at least 0, \
                 got {value}"
            ))
        }
    }
}

/// The guiding train's metric-sweep parameters
/// (`optical_trains[].auto_focus`, rp.md § Optical Trains).
///
/// Only the guiding train carries the block. An imaging train is
/// focused by the focus-model provider, from its own
/// `focus-model.json`, and
/// [`crate::equipment::trains::TrainModel::try_from_equipment`] refuses
/// a block on one, naming the train and that file. The block backs
/// `auto_focus` on the guiding train: per-call parameters override it
/// field by field.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TrainAutoFocusConfig {
    pub step_size: SweepStepSize,
    pub half_width: SweepHalfWidth,
    /// Minimum valid samples for the fit. Omitted → the tool default
    /// (5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_fit_points: Option<usize>,
    /// Fresh guide frames per sweep position. Omitted → the default
    /// (3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames_per_step: Option<FramesPerStep>,
    /// Confirmation tolerance of the fitted position. Omitted → the
    /// tool default (0.25).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation_tolerance: Option<ConfirmationTolerance>,
    /// Every other field the block carries: a capture sweep's
    /// (`duration`, `min_area`, `max_area`, `binning`, …), on a block
    /// written for an imaging train, among them. They parse here so the
    /// load refuses each by name, with the train it belongs to, where an
    /// unknown-field parse error would stop at the first and could not
    /// say that an imaging train's block belongs in `focus-model.json`.
    #[serde(flatten)]
    pub other_fields: std::collections::BTreeMap<String, serde_json::Value>,
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
/// shared by `load_config` and the setup page's save.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpticalTrainConfig {
    pub id: String,
    #[serde(default)]
    pub purpose: TrainPurpose,
    /// The telescope this light path looks through, written as the
    /// FITS `TELESCOP` card and the sidecar's `telescope`. Omitted →
    /// frames through this train carry neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telescope: Option<TelescopeName>,
    /// Effective focal length of this light path. Omitted → captures
    /// through this train's camera record no focal length, pixel scale
    /// or FOV in their `optics` block, exactly like a camera outside
    /// any train.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focal_length_mm: Option<FocalLengthMm>,
    /// Clear aperture of this light path. With `focal_length_mm` it
    /// gives the focal ratio `get_train_info.optics` reports (rp.md §
    /// Train optics). Omitted → `focal_ratio` is null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aperture_mm: Option<ApertureMm>,
    /// Central obstruction of this light path, a diameter like
    /// `aperture_mm` and smaller than it. Omitted → unobstructed, an
    /// `obstruction_ratio` of 0 on `get_train_info.optics` (rp.md §
    /// Train optics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obstruction_mm: Option<ObstructionMm>,
    /// Default framing angle for targets that don't carry their own —
    /// layer two of the effective position angle (rp.md § Target
    /// Store → Position angle). For a rotator-less train this
    /// documents the camera's fixed mounting angle. Omitted → the
    /// fallback skips to 0.0 north-up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_position_angle_degrees: Option<PositionAngleDegrees>,
    /// Roster device ids, objective side first. The last entry must be
    /// a camera; the rest are focusers, rotators, and filter wheels.
    pub devices: Vec<String>,
    /// The guiding train's metric-sweep parameters. Omitted → an
    /// `auto_focus` call on the guiding train passes the sweep geometry
    /// per call. An imaging train carries none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_focus: Option<TrainAutoFocusConfig>,
}

impl OpticalTrainConfig {
    /// This entry's rules across its own fields, for
    /// [`super::validate_config`], which startup and the setup page's
    /// save share. `index` is the entry's position in
    /// `equipment.optical_trains`, for the dotted path.
    ///
    /// An obstruction must be smaller than the aperture: one as wide
    /// leaves no light path, and the ratio a focus provider sizes a
    /// sweep from would be meaningless. A train without an aperture
    /// has nothing to compare against and no focal ratio to size from,
    /// so its obstruction is accepted and inert.
    #[must_use]
    pub fn field_errors(&self, index: usize) -> Vec<FieldError> {
        let mut errors = Vec::new();
        if let (Some(obstruction), Some(aperture)) = (self.obstruction_mm, self.aperture_mm) {
            if obstruction.value() >= aperture.value() {
                errors.push(FieldError {
                    path: format!("equipment.optical_trains.{index}.obstruction_mm"),
                    msg: format!(
                        "must be smaller than aperture_mm ({}), since an obstruction as wide as \
                         the aperture leaves no light path; got {} (train '{}')",
                        aperture.value(),
                        obstruction.value(),
                        self.id
                    ),
                });
            }
        }
        errors
    }
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
    fn obstruction_mm_accepts_zero_and_a_positive_diameter() {
        assert_eq!(ObstructionMm::try_new(0.0).unwrap().value(), 0.0);
        assert_eq!(ObstructionMm::try_new(68.0).unwrap().value(), 68.0);
    }

    #[test]
    fn obstruction_mm_rejects_a_negative_or_non_finite_value() {
        for bad in [-1.0, f64::NAN, f64::INFINITY] {
            let err = ObstructionMm::try_new(bad).unwrap_err();
            assert!(
                err.contains("obstruction_mm must be a finite number, not negative"),
                "{bad}: {err}"
            );
        }
    }

    fn train(json: serde_json::Value) -> OpticalTrainConfig {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn an_obstruction_as_wide_as_the_aperture_is_a_field_error_naming_the_path() {
        let errors = train(serde_json::json!({
            "id": "main", "aperture_mm": 200.0, "obstruction_mm": 200.0,
            "devices": ["main-cam"]
        }))
        .field_errors(2);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "equipment.optical_trains.2.obstruction_mm");
        assert!(
            errors[0]
                .msg
                .contains("must be smaller than aperture_mm (200)"),
            "{}",
            errors[0].msg
        );
        assert!(errors[0].msg.contains("train 'main'"), "{}", errors[0].msg);
    }

    #[test]
    fn an_obstruction_inside_the_aperture_or_without_one_is_accepted() {
        for json in [
            serde_json::json!({"id": "main", "aperture_mm": 200.0, "obstruction_mm": 68.0,
                               "devices": ["main-cam"]}),
            serde_json::json!({"id": "main", "obstruction_mm": 68.0, "devices": ["main-cam"]}),
            serde_json::json!({"id": "main", "aperture_mm": 200.0, "devices": ["main-cam"]}),
        ] {
            assert_eq!(train(json.clone()).field_errors(0), Vec::new(), "{json}");
        }
    }

    #[test]
    fn load_config_refuses_an_obstruction_as_wide_as_the_aperture() {
        let (_dir, path) = write_config(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {"id": "main-cam", "alpaca_url": "http://localhost:11120"}
                    ],
                    "optical_trains": [
                        {"id": "main", "aperture_mm": 100.0, "obstruction_mm": 120.0,
                         "devices": ["main-cam"]}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        );
        let err = load_config(&path).unwrap_err().to_string();
        assert!(
            err.contains("equipment.optical_trains.0.obstruction_mm"),
            "{err}"
        );
    }

    #[test]
    fn telescope_name_accepts_a_fits_card_value() {
        let name = TelescopeName::try_new("Takahashi FSQ-106EDX4".to_string()).unwrap();
        assert_eq!(name.as_str(), "Takahashi FSQ-106EDX4");
        // An editor's en dash folds on the card; the name keeps it.
        let name = TelescopeName::try_new("Celestron C11 \u{2013} EdgeHD".to_string()).unwrap();
        assert_eq!(name.as_str(), "Celestron C11 \u{2013} EdgeHD");
        TelescopeName::try_new("x".repeat(68)).unwrap();
        // 66 characters plus an apostrophe is exactly 68 on the card.
        TelescopeName::try_new(format!("{}'", "x".repeat(66))).unwrap();
    }

    #[test]
    fn telescope_name_rejects_what_a_fits_card_cannot_hold() {
        for (value, needle) in [
            (String::new(), "empty"),
            ("   ".to_string(), "empty"),
            ("Celestron C11 \u{2605}".to_string(), "printable ASCII"),
            ("x".repeat(69), "too long"),
            // 68 characters, but the apostrophe doubles to 69 on the card,
            // and the message says why.
            (
                format!("{}'", "x".repeat(67)),
                "69 characters with apostrophes doubled",
            ),
        ] {
            let err = TelescopeName::try_new(value.clone()).unwrap_err();
            assert!(err.contains(needle), "{value:?}: {err}");
            assert!(!err.contains("invalid keyword"), "{value:?}: {err}");
        }
    }

    #[test]
    fn telescope_loads_from_the_train_entry() {
        let (_dir, path) = write_config(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {"id": "main-cam", "alpaca_url": "http://localhost:11120"}
                    ],
                    "optical_trains": [
                        {"id": "main", "telescope": "Takahashi FSQ-106EDX4",
                         "devices": ["main-cam"]}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        );
        let config = load_config(&path).unwrap();
        assert_eq!(
            config.equipment.optical_trains[0]
                .telescope
                .as_ref()
                .map(TelescopeName::as_str),
            Some("Takahashi FSQ-106EDX4")
        );
    }

    #[test]
    fn a_telescope_no_card_can_hold_fails_the_load() {
        let (_dir, path) = write_config(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {"id": "main-cam", "alpaca_url": "http://localhost:11120"}
                    ],
                    "optical_trains": [
                        {"id": "main", "telescope": "", "devices": ["main-cam"]}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        );
        let err = format!("{:#}", load_config(&path).unwrap_err());
        assert!(err.contains("telescope"), "{err}");
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
    fn auto_focus_block_round_trips_the_metric_sweep_fields() {
        let (_dir, path) = write_config(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cameras": [
                        {"id": "guide-cam", "alpaca_url": "http://localhost:11120"}
                    ],
                    "mount": {
                        "alpaca_url": "http://localhost:11121",
                        "guiding": {"url": "http://localhost:11130"}
                    },
                    "optical_trains": [
                        {"id": "guide", "purpose": "guiding", "devices": ["guide-cam"],
                         "auto_focus": {"step_size": 50, "half_width": 500,
                                        "frames_per_step": 3}}
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
        assert_eq!(block.step_size.value(), 50);
        assert_eq!(block.half_width.value(), 500);
        assert_eq!(block.frames_per_step.map(FramesPerStep::value), Some(3));
        assert!(block.min_fit_points.is_none());
        assert!(block.confirmation_tolerance.is_none());
        assert!(block.other_fields.is_empty(), "{:?}", block.other_fields);

        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(
            value.pointer("/equipment/optical_trains/0/auto_focus"),
            Some(&serde_json::json!({"step_size": 50, "half_width": 500, "frames_per_step": 3}))
        );
    }

    /// A capture sweep's fields parse, so the train model can refuse
    /// them by name (an imaging train's whole block, a guiding train's
    /// field by field), and are written back as they were.
    #[test]
    fn auto_focus_block_keeps_every_other_field_for_the_load_to_refuse() {
        let json = serde_json::json!({
            "duration": "3s", "step_size": 100, "half_width": 1000,
            "min_area": 4, "max_area": 500, "binning": "2x2"
        });
        let block: TrainAutoFocusConfig = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            block.other_fields.keys().collect::<Vec<_>>(),
            ["binning", "duration", "max_area", "min_area"]
        );
        assert_eq!(serde_json::to_value(&block).unwrap(), json);
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
                        {"id": "guide", "devices": ["guide-cam"],
                         "auto_focus": {"step_size": 50, "half_width": 500}}
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

    #[test]
    fn confirmation_tolerance_newtype_validation_boundaries() {
        assert_eq!(ConfirmationTolerance::try_from(0.0).unwrap().value(), 0.0);
        assert_eq!(ConfirmationTolerance::try_from(0.25).unwrap().value(), 0.25);
        assert!(ConfirmationTolerance::try_from(-0.5)
            .unwrap_err()
            .contains("confirmation_tolerance must be a finite number of at least 0"));
        assert!(ConfirmationTolerance::try_from(f64::INFINITY).is_err());
    }

    #[test]
    fn auto_focus_block_parses_the_tolerance_and_names_a_bad_one() {
        let block: TrainAutoFocusConfig = serde_json::from_value(serde_json::json!({
            "step_size": 10, "half_width": 50, "confirmation_tolerance": 0.5
        }))
        .unwrap();
        assert_eq!(
            block
                .confirmation_tolerance
                .map(ConfirmationTolerance::value),
            Some(0.5)
        );

        let err = serde_json::from_value::<TrainAutoFocusConfig>(serde_json::json!({
            "step_size": 10, "half_width": 50, "confirmation_tolerance": -0.5
        }))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("auto_focus.confirmation_tolerance must be a finite number of at least 0"),
            "{err}"
        );
    }
}
