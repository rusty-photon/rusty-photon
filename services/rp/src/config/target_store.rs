//! The `target_store` config block (`docs/services/rp.md` § Target Store →
//! Configuration).
//!
//! Its fields: `db_path`, `default_goals`, `default_scheduling`
//! (Decision 9's altitude-gating parity,
//! `docs/plans/planetarium-target-import.md`), `import`, and
//! `default_grading` — the thresholds the on-disk frame scan judges each
//! frame's sidecar metrics against (rp.md § Progress derivation).
//!
//! Typed on [`crate::config::Config`] as [`TargetStoreConfigWire`] rather
//! than an untyped `Value`: the block is now the *only* meaning of the key
//! (the legacy `targets[]` planner array was retired — targets live in the
//! redb store, added via `add_target`), so `Config`'s top-level
//! `deny_unknown_fields` rejects a stray legacy `targets` key or a leftover
//! array shape loudly at load, and `GET /api/config/schema` carries the
//! real structure. The wire type stays rp-local (mirroring [`GoalWire`]'s
//! role for goals) so the store leaf keeps its deliberately schemars-free
//! build (`crates/rp-targets/BUILD.bazel`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::planner::goal_wire::GoalWire;

/// Parsed, validated `target_store` config block feeding the target-store
/// MCP tools (`add_target`'s `default_goals` fallback, the store's on-disk
/// location).
///
/// Produced from [`TargetStoreConfigWire`] by
/// [`parse_target_store_config`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TargetStoreConfig {
    /// Overrides the default `<session.data_directory>/targets.redb`
    /// location.
    pub db_path: Option<String>,
    /// Applied by `add_target` when the caller supplies no `goals[]`
    /// (Decision 10 — rp-owned policy, not bridge/UI config).
    pub default_goals: Vec<rp_targets::AcquisitionGoal>,
    /// Fallback scheduling constraints for a target whose own
    /// `scheduling` is `None`. `get_next_target`'s altitude-gating
    /// parity (Decision 9) reads `min_altitude_degrees` from here when
    /// a store-backed target carries no per-target override.
    pub default_scheduling: rp_targets::SchedulingConstraints,
    /// Tunables for `add_target`'s `source` import form (rp.md § Target
    /// Store → Import form).
    pub import: ImportConfig,
    /// Fallback grading thresholds a target's `None` `grading` override
    /// fields fall back to (rp.md § Progress derivation). `None` — the
    /// out-of-the-box default — means no metric is judged, so no frame
    /// is ever rejected and `good == total`.
    pub default_grading: Option<rp_targets::GradingThresholds>,
}

/// Validated import tunables (`target_store.import`): all three are
/// checked finite and positive by [`parse_target_store_config`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImportConfig {
    /// Proximity window, in arcseconds, within which a repeated import
    /// upserts a still-pending, import-owned row instead of creating a
    /// new target. Identity, never naming.
    pub dedup_arcsec: f64,
    /// Deep-sky-class cone radius, in arcminutes, for the add-time
    /// reverse cone-search that names an import. Display only.
    pub naming_tolerance_arcmin: f64,
    /// Star-class cone radius, in arcminutes. A star names an import
    /// only when no deep-sky object is inside its own cone.
    pub star_naming_tolerance_arcmin: f64,
}

impl Default for ImportConfig {
    fn default() -> Self {
        Self {
            dedup_arcsec: 30.0,
            naming_tolerance_arcmin: 10.0,
            star_naming_tolerance_arcmin: 2.0,
        }
    }
}

/// The `target_store` config block as it appears on the wire (config JSON)
/// — the type of [`crate::config::Config::target_store`].
///
/// `db_path` and
/// `default_goals`/`default_scheduling` mirror [`TargetStoreConfig`], but
/// `default_goals` carries the [`GoalWire`] string shape (`binning`
/// `"1x1"`, `exposure_duration` `"5m"`) that the `TryFrom<&GoalWire>`
/// conversion validates, and
/// `default_scheduling` is the schemars-able [`SchedulingWire`] projection.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TargetStoreConfigWire {
    /// See [`TargetStoreConfig::db_path`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_path: Option<String>,
    /// See [`TargetStoreConfig::default_goals`]; wire shape is [`GoalWire`].
    pub default_goals: Vec<GoalWire>,
    /// See [`TargetStoreConfig::default_scheduling`]; wire shape is
    /// [`SchedulingWire`].
    pub default_scheduling: SchedulingWire,
    /// See [`TargetStoreConfig::import`]; wire shape is [`ImportWire`].
    pub import: ImportWire,
    /// See [`TargetStoreConfig::default_grading`]; wire shape is
    /// [`GradingWire`]. Absent (the default) means no metric is judged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_grading: Option<GradingWire>,
}

/// JsonSchema-able wire projection of [`rp_targets::GradingThresholds`].
///
/// Exists for the same reason [`SchedulingWire`] does: the store leaf is
/// deliberately schemars-free. Field-for-field identical; each `None`
/// means "don't judge this metric".
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GradingWire {
    /// Maximum half-flux radius, in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_hfr_pixels: Option<f64>,
    /// Minimum detected star count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_star_count: Option<u32>,
    /// Maximum PSF eccentricity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_eccentricity: Option<f64>,
    /// Minimum signal-to-noise ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_snr: Option<f64>,
}

impl From<GradingWire> for rp_targets::GradingThresholds {
    fn from(w: GradingWire) -> Self {
        Self {
            max_hfr_pixels: w.max_hfr_pixels,
            min_star_count: w.min_star_count,
            max_eccentricity: w.max_eccentricity,
            min_snr: w.min_snr,
        }
    }
}

/// The `target_store.import` block as it appears on the wire — mirrors
/// [`ImportConfig`], whose range validation lives in
/// [`parse_target_store_config`].
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ImportWire {
    /// See [`ImportConfig::dedup_arcsec`].
    pub dedup_arcsec: f64,
    /// See [`ImportConfig::naming_tolerance_arcmin`].
    pub naming_tolerance_arcmin: f64,
    /// See [`ImportConfig::star_naming_tolerance_arcmin`].
    pub star_naming_tolerance_arcmin: f64,
}

impl Default for ImportWire {
    fn default() -> Self {
        let d = ImportConfig::default();
        Self {
            dedup_arcsec: d.dedup_arcsec,
            naming_tolerance_arcmin: d.naming_tolerance_arcmin,
            star_naming_tolerance_arcmin: d.star_naming_tolerance_arcmin,
        }
    }
}

/// JsonSchema-able wire projection of [`rp_targets::SchedulingConstraints`].
///
/// The store leaf is deliberately schemars-free
/// (`crates/rp-targets/BUILD.bazel`), so the config layer owns the
/// schema-bearing shape (the same reason [`GoalWire`] exists for goals).
/// Field-for-field identical; each `None` falls back to the rp-config
/// global default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SchedulingWire {
    /// Minimum altitude, in degrees, the target must be above.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_altitude_degrees: Option<f64>,
    /// Minimum angular separation from the Moon, in degrees.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_moon_separation_degrees: Option<f64>,
    /// Maximum Moon illumination fraction (`0.0`-`1.0`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_moon_illumination_fraction: Option<f64>,
    /// Maximum `|hour angle|` from the meridian, in hours.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meridian_window_hours: Option<f64>,
}

impl From<SchedulingWire> for rp_targets::SchedulingConstraints {
    fn from(w: SchedulingWire) -> Self {
        Self {
            min_altitude_degrees: w.min_altitude_degrees,
            min_moon_separation_degrees: w.min_moon_separation_degrees,
            max_moon_illumination_fraction: w.max_moon_illumination_fraction,
            meridian_window_hours: w.meridian_window_hours,
        }
    }
}

/// Validates a [`TargetStoreConfigWire`] into [`TargetStoreConfig`].
///
/// Serde has already checked the block's shape at config-load (typed
/// field + `deny_unknown_fields`), so the remaining fallible steps are
/// parsing each `default_goals` entry's `binning` / `exposure_duration`
/// strings and range-checking the `import` tunables.
///
/// # Errors
///
/// Returns a human-readable message if a `default_goals` entry fails
/// its `TryFrom<&GoalWire>` conversion into [`rp_targets::AcquisitionGoal`],
/// an `import` field is not a finite positive number, or a supplied
/// `default_grading` threshold is not a finite non-negative number.
pub fn parse_target_store_config(
    wire: &TargetStoreConfigWire,
) -> Result<TargetStoreConfig, String> {
    let mut default_goals = Vec::with_capacity(wire.default_goals.len());
    for g in &wire.default_goals {
        default_goals.push(rp_targets::AcquisitionGoal::try_from(g)?);
    }
    for (field, value) in [
        ("target_store.import.dedup_arcsec", wire.import.dedup_arcsec),
        (
            "target_store.import.naming_tolerance_arcmin",
            wire.import.naming_tolerance_arcmin,
        ),
        (
            "target_store.import.star_naming_tolerance_arcmin",
            wire.import.star_naming_tolerance_arcmin,
        ),
    ] {
        if !value.is_finite() || value <= 0.0 {
            return Err(format!(
                "{field} must be a finite positive number, got {value}"
            ));
        }
    }
    if let Some(grading) = &wire.default_grading {
        for (field, value) in [
            ("max_hfr_pixels", grading.max_hfr_pixels),
            ("max_eccentricity", grading.max_eccentricity),
            ("min_snr", grading.min_snr),
        ] {
            // `None` is "don't judge this metric"; a supplied value has
            // to be a number a comparison can mean something against.
            if let Some(value) = value {
                if !value.is_finite() || value < 0.0 {
                    return Err(format!(
                        "target_store.default_grading.{field} must be a finite \
                         non-negative number, got {value}"
                    ));
                }
            }
        }
    }
    Ok(TargetStoreConfig {
        db_path: wire.db_path.clone(),
        default_goals,
        default_scheduling: wire.default_scheduling.into(),
        import: ImportConfig {
            dedup_arcsec: wire.import.dedup_arcsec,
            naming_tolerance_arcmin: wire.import.naming_tolerance_arcmin,
            star_naming_tolerance_arcmin: wire.import.star_naming_tolerance_arcmin,
        },
        default_grading: wire.default_grading.map(Into::into),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_wire_gives_defaults() {
        let config = parse_target_store_config(&TargetStoreConfigWire::default()).unwrap();
        assert_eq!(config, TargetStoreConfig::default());
    }

    #[test]
    fn object_shape_parses_db_path_and_default_goals() {
        let wire: TargetStoreConfigWire = serde_json::from_value(serde_json::json!({
            "db_path": "/data/lights/targets.redb",
            "default_goals": [
                {"filter": "L", "binning": "1x1", "exposure_duration": "5m", "desired_count": 20}
            ]
        }))
        .unwrap();
        let config = parse_target_store_config(&wire).unwrap();
        assert_eq!(config.db_path.as_deref(), Some("/data/lights/targets.redb"));
        assert_eq!(config.default_goals.len(), 1);
        assert_eq!(config.default_goals[0].filter, "L");
    }

    #[test]
    fn object_shape_parses_default_scheduling() {
        let wire: TargetStoreConfigWire = serde_json::from_value(serde_json::json!({
            "default_scheduling": { "min_altitude_degrees": 25.0 }
        }))
        .unwrap();
        let config = parse_target_store_config(&wire).unwrap();
        assert_eq!(config.default_scheduling.min_altitude_degrees, Some(25.0));
    }

    #[test]
    fn wire_rejects_unknown_field() {
        let err =
            serde_json::from_value::<TargetStoreConfigWire>(serde_json::json!({"bogus": true}))
                .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn wire_rejects_a_leftover_array_shape() {
        // The retired legacy `targets[]` planner array — an un-migrated
        // config must fail loudly at load, not silently yield empty
        // target-store settings.
        let err = serde_json::from_value::<TargetStoreConfigWire>(serde_json::json!([
            {"name": "M31", "ra_hours": 0.7, "dec_degrees": 41.0}
        ]))
        .unwrap_err();
        assert!(
            err.to_string().contains("invalid type"),
            "expected a type-mismatch error, got: {err}"
        );
    }

    #[test]
    fn object_shape_rejects_bad_default_goal() {
        let wire: TargetStoreConfigWire = serde_json::from_value(serde_json::json!({
            "default_goals": [{"filter": "L", "binning": "bad", "exposure_duration": "5m", "desired_count": 20}]
        }))
        .unwrap();
        let err = parse_target_store_config(&wire).unwrap_err();
        assert!(err.contains("binning"), "{err}");
    }

    #[test]
    fn import_defaults_are_the_documented_tunables() {
        let config = parse_target_store_config(&TargetStoreConfigWire::default()).unwrap();
        assert_eq!(config.import.dedup_arcsec, 30.0);
        assert_eq!(config.import.naming_tolerance_arcmin, 10.0);
        assert_eq!(config.import.star_naming_tolerance_arcmin, 2.0);
    }

    #[test]
    fn import_block_overrides_parse() {
        let wire: TargetStoreConfigWire = serde_json::from_value(serde_json::json!({
            "import": { "dedup_arcsec": 45.0, "star_naming_tolerance_arcmin": 1.0 }
        }))
        .unwrap();
        let config = parse_target_store_config(&wire).unwrap();
        assert_eq!(config.import.dedup_arcsec, 45.0);
        assert_eq!(config.import.naming_tolerance_arcmin, 10.0);
        assert_eq!(config.import.star_naming_tolerance_arcmin, 1.0);
    }

    #[test]
    fn import_block_rejects_nonpositive_tolerance() {
        let wire: TargetStoreConfigWire = serde_json::from_value(serde_json::json!({
            "import": { "dedup_arcsec": 0.0 }
        }))
        .unwrap();
        let err = parse_target_store_config(&wire).unwrap_err();
        assert!(err.contains("target_store.import.dedup_arcsec"), "{err}");
    }

    #[test]
    fn default_grading_is_absent_out_of_the_box() {
        let config = parse_target_store_config(&TargetStoreConfigWire::default()).unwrap();
        assert_eq!(
            config.default_grading, None,
            "no thresholds configured means no frame is ever rejected"
        );
    }

    #[test]
    fn default_grading_parses_a_partial_block() {
        let wire: TargetStoreConfigWire = serde_json::from_value(serde_json::json!({
            "default_grading": { "max_hfr_pixels": null, "min_star_count": 20 }
        }))
        .unwrap();
        let grading = parse_target_store_config(&wire)
            .unwrap()
            .default_grading
            .unwrap();
        assert_eq!(grading.min_star_count, Some(20));
        assert_eq!(grading.max_hfr_pixels, None, "null means don't judge it");
    }

    #[test]
    fn default_grading_rejects_a_non_finite_threshold() {
        let wire = TargetStoreConfigWire {
            default_grading: Some(GradingWire {
                max_hfr_pixels: Some(f64::NAN),
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = parse_target_store_config(&wire).unwrap_err();
        assert!(
            err.contains("target_store.default_grading.max_hfr_pixels"),
            "{err}"
        );
    }

    #[test]
    fn default_grading_rejects_a_negative_threshold() {
        let wire = TargetStoreConfigWire {
            default_grading: Some(GradingWire {
                min_snr: Some(-1.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = parse_target_store_config(&wire).unwrap_err();
        assert!(
            err.contains("target_store.default_grading.min_snr"),
            "{err}"
        );
    }

    #[test]
    fn default_grading_rejects_unknown_field() {
        let err = serde_json::from_value::<TargetStoreConfigWire>(serde_json::json!({
            "default_grading": { "max_fwhm_pixels": 3.0 }
        }))
        .unwrap_err();
        assert!(err.to_string().contains("max_fwhm_pixels"), "{err}");
    }

    #[test]
    fn import_block_rejects_unknown_field() {
        let err = serde_json::from_value::<TargetStoreConfigWire>(serde_json::json!({
            "import": { "dedup_arcmin": 1.0 }
        }))
        .unwrap_err();
        assert!(err.to_string().contains("dedup_arcmin"), "{err}");
    }

    #[test]
    fn wire_round_trips_through_json() {
        let wire: TargetStoreConfigWire = serde_json::from_value(serde_json::json!({
            "db_path": "/data/targets.redb",
            "default_goals": [
                {"filter": "L", "binning": "1x1", "exposure_duration": "5m", "desired_count": 20}
            ],
            "default_scheduling": { "min_altitude_degrees": 20.0 }
        }))
        .unwrap();
        let round_tripped: TargetStoreConfigWire =
            serde_json::from_value(serde_json::to_value(&wire).unwrap()).unwrap();
        assert_eq!(wire, round_tripped);
    }
}
