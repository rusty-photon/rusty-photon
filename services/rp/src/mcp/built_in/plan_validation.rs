//! The single home of rp's plan-data validation rules (rp.md § Plan
//! schema and validation, ADR-019).
//!
//! Every rule a plan write enforces lives here exactly once, and both the
//! writers (`add_target`, `set_goals`) and the read-only `validate_plan`
//! tool call these functions. That is the whole point of the module: a
//! surface that pre-flights a payload with `validate_plan` must not then
//! be surprised by `add_target`, in either direction, and the only way to
//! guarantee that is to have one implementation rather than two that
//! agree by convention.
//!
//! Errors are [`FieldError`] — the same `{path, msg}` shape
//! `config-actions` returns — with dotted, indexed paths into the
//! submitted value (`ra_hours`, `goals[1].binning`,
//! `scheduling.min_altitude_degrees`), so a UI renders a plan error next
//! to its field with the code it already has for driver config.
//!
//! Deliberately **not** here: anything that is a fact about the store
//! rather than about the payload — whether a slug already exists, which
//! suffix a collision would allocate, whether a referenced slug is
//! present. Those need I/O and a moment in time; this module is pure.

use rp_targets::AcquisitionGoal;
use rp_vocabulary::IcrsCoord;
use rusty_photon_config::actions::FieldError;

use crate::equipment::EquipmentRegistry;
use crate::planner::goal_wire::GoalWire;

use crate::config::target_store::GradingWire;

use super::targets::{AddTargetParams, SchedulingWire};

/// A `{path, msg}` pair, for the many small constructions below.
fn err(path: impl Into<String>, msg: impl Into<String>) -> FieldError {
    FieldError {
        path: path.into(),
        msg: msg.into(),
    }
}

/// Validates the `goals[]` payload shared by `add_target` and
/// `set_goals`, returning the parsed goals when every rule passes.
///
/// `roster` is the rig's configured filter names (the union of every
/// `equipment.filter_wheels[].filters`) — config, not live device state,
/// so this never touches hardware. An empty roster is permissive: a rig
/// with no wheels configured has nothing to validate against.
///
/// Both the per-goal rules (parseable binning and exposure, non-zero
/// count) and the set-level rule (no duplicate
/// `(filter, binning, exposure_duration)` key) are checked. The
/// set-level pass delegates to [`rp_targets::validate_goals`] — the
/// store's own write-time invariant — rather than restating it, so a
/// rule the store gains but this module does not mirror still surfaces
/// here instead of passing validation and then failing the write.
///
/// # Errors
///
/// Returns every [`FieldError`] found, each at `<path_prefix>[i].<field>`:
/// an unparseable `binning`, an unparseable or zero `exposure_duration`,
/// a zero `desired_count`, or a `filter` missing from a non-empty
/// roster. Only when every goal parses cleanly is the store's set-level
/// rule consulted, reported at `path_prefix` itself.
pub fn validate_goals(
    wire: &[GoalWire],
    roster: &[String],
    path_prefix: &str,
) -> Result<Vec<AcquisitionGoal>, Vec<FieldError>> {
    let mut errors = Vec::new();
    let mut parsed = Vec::with_capacity(wire.len());

    for (index, goal) in wire.iter().enumerate() {
        let at = |field: &str| format!("{path_prefix}[{index}].{field}");

        let binning = match goal.binning.parse::<rp_vocabulary::Binning>() {
            Ok(b) => Some(b),
            Err(e) => {
                errors.push(err(at("binning"), e.to_string()));
                None
            }
        };

        let exposure = match humantime::parse_duration(&goal.exposure_duration) {
            Ok(d) if d.is_zero() => {
                errors.push(err(at("exposure_duration"), "must be greater than zero"));
                None
            }
            Ok(d) => Some(d),
            Err(e) => {
                errors.push(err(
                    at("exposure_duration"),
                    format!("{:?}: {e}", goal.exposure_duration),
                ));
                None
            }
        };

        if goal.desired_count == 0 {
            errors.push(err(at("desired_count"), "must be greater than zero"));
        }

        if !roster.is_empty() && !roster.iter().any(|f| f == &goal.filter) {
            errors.push(err(
                at("filter"),
                format!(
                    "{:?} is not in the configured filter roster ({})",
                    goal.filter,
                    roster.join(", ")
                ),
            ));
        }

        if let (Some(binning), Some(exposure_duration)) = (binning, exposure) {
            parsed.push(AcquisitionGoal {
                filter: goal.filter.clone(),
                binning,
                exposure_duration,
                desired_count: goal.desired_count,
            });
        }
    }

    // The store's invariant, consulted rather than restated. Only
    // meaningful once every goal parsed, and only reported when the
    // per-goal pass found nothing — otherwise it would restate a
    // problem already located at a precise path.
    if errors.is_empty() && parsed.len() == wire.len() {
        if let Err(e) = rp_targets::validate_goals(&parsed) {
            errors.push(err(path_prefix, e.to_string()));
        }
    }

    if errors.is_empty() {
        Ok(parsed)
    } else {
        Err(errors)
    }
}

/// Validates the per-target scheduling overrides.
#[must_use]
pub fn validate_scheduling(wire: &SchedulingWire, path_prefix: &str) -> Vec<FieldError> {
    let mut errors = Vec::new();
    let at = |field: &str| format!("{path_prefix}.{field}");

    if let Some(v) = wire.min_altitude_degrees {
        if !(-90.0..=90.0).contains(&v) {
            errors.push(err(at("min_altitude_degrees"), "must be in [-90, 90]"));
        }
    }
    if let Some(v) = wire.min_moon_separation_degrees {
        if !(0.0..=180.0).contains(&v) {
            errors.push(err(
                at("min_moon_separation_degrees"),
                "must be in [0, 180]",
            ));
        }
    }
    if let Some(v) = wire.max_moon_illumination_fraction {
        if !(0.0..=1.0).contains(&v) {
            errors.push(err(
                at("max_moon_illumination_fraction"),
                "must be in [0, 1]",
            ));
        }
    }
    if let Some(v) = wire.meridian_window_hours {
        if !(0.0..=12.0).contains(&v) {
            errors.push(err(at("meridian_window_hours"), "must be in [0, 12]"));
        }
    }
    errors
}

/// Validates the per-target grading overrides: every supplied threshold
/// must be finite and non-negative (the same rule config load applies to
/// `target_store.default_grading`).
#[must_use]
pub fn validate_grading(wire: &GradingWire, path_prefix: &str) -> Vec<FieldError> {
    let mut errors = Vec::new();
    let at = |field: &str| format!("{path_prefix}.{field}");

    let mut finite_non_negative = |value: Option<f64>, field: &str| {
        if let Some(v) = value {
            if !v.is_finite() || v < 0.0 {
                errors.push(err(at(field), "must be a finite, non-negative number"));
            }
        }
    };
    finite_non_negative(wire.max_hfr_pixels, "max_hfr_pixels");
    finite_non_negative(wire.max_eccentricity, "max_eccentricity");
    finite_non_negative(wire.min_snr, "min_snr");
    errors
}

/// Validates a framing angle against the same domain as the per-train
/// config default ([`crate::config::PositionAngleDegrees`]).
#[must_use]
pub fn validate_position_angle(value: Option<f64>, path: &str) -> Vec<FieldError> {
    value.map_or_else(
        Vec::new,
        |v| match crate::config::PositionAngleDegrees::try_new(v) {
            Ok(_) => Vec::new(),
            Err(e) => vec![err(path, e)],
        },
    )
}

/// Validates a coordinate pair through the one validator
/// ([`IcrsCoord::try_new`]), attributing the failure to whichever field
/// the typed error names.
#[must_use]
pub fn validate_coord(ra_hours: f64, dec_degrees: f64) -> Vec<FieldError> {
    match IcrsCoord::try_new(ra_hours, dec_degrees) {
        Ok(_) => Vec::new(),
        Err(e @ rp_vocabulary::CoordError::RaOutOfRange { .. }) => {
            vec![err("ra_hours", e.to_string())]
        }
        Err(e @ rp_vocabulary::CoordError::DecOutOfRange { .. }) => {
            vec![err("dec_degrees", e.to_string())]
        }
    }
}

/// Validates the naming-pattern payload against the same token rules
/// config load applies (`crate::config::naming_template`).
#[must_use]
pub fn validate_naming_patterns(
    file_naming_pattern: Option<&str>,
    directory_pattern: Option<&str>,
) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if let Some(pattern) = file_naming_pattern {
        if let Err(msg) = crate::config::naming_template::validate_pattern(pattern) {
            errors.push(err("file_naming_pattern", msg));
        }
    }
    if let Some(pattern) = directory_pattern {
        if let Err(msg) = crate::config::naming_template::validate_directory_pattern(pattern) {
            errors.push(err("directory_pattern", msg));
        }
    }
    errors
}

/// The rig's configured filter names, the union across every wheel.
/// Empty when no wheels are configured, which the goal rules read as
/// permissive.
#[must_use]
pub fn filter_roster(equipment: &EquipmentRegistry) -> Vec<String> {
    equipment
        .filter_wheels
        .iter()
        .flat_map(|fw| fw.config.filters.iter().cloned())
        .collect()
}

/// Validates a whole `add_target` payload — every rule the write applies
/// to the payload itself, in one pass, so a form can be reported on as a
/// whole rather than one field per round trip.
///
/// `defaults` supplies the goals used when the payload omits them
/// (`target_store.default_goals`), matching what the write would store.
#[must_use]
pub fn validate_add_target(
    params: &AddTargetParams,
    defaults: &[AcquisitionGoal],
    equipment: &EquipmentRegistry,
) -> Vec<FieldError> {
    let mut errors = Vec::new();

    // Form selection, mirroring add_target's own dispatch.
    if params.source.is_some() {
        for (present, field) in [
            (params.catalog_ref.is_some(), "catalog_ref"),
            (params.display_name.is_some(), "display_name"),
            (params.active.is_some(), "active"),
            (params.notes.is_some(), "notes"),
            (
                params.position_angle_degrees.is_some(),
                "position_angle_degrees",
            ),
            (params.grading.is_some(), "grading"),
        ] {
            if present {
                errors.push(err(field, "not accepted with `source` (the import form)"));
            }
        }
        if params.ra_hours.is_none() || params.dec_degrees.is_none() {
            errors.push(err(
                "ra_hours",
                "the import form requires ra_hours and dec_degrees",
            ));
        }
    } else {
        match (&params.catalog_ref, &params.display_name) {
            (Some(_), Some(_)) | (None, None) => errors.push(err(
                "display_name",
                "supply exactly one of catalog_ref or display_name",
            )),
            (None, Some(_)) => {
                if params.ra_hours.is_none() || params.dec_degrees.is_none() {
                    errors.push(err(
                        "ra_hours",
                        "the display_name form requires ra_hours and dec_degrees",
                    ));
                }
            }
            (Some(_), None) => {
                if params.ra_hours.is_some() != params.dec_degrees.is_some() {
                    errors.push(err(
                        "ra_hours",
                        "ra_hours and dec_degrees must both be supplied together",
                    ));
                }
            }
        }
    }

    if let (Some(ra), Some(dec)) = (params.ra_hours, params.dec_degrees) {
        errors.extend(validate_coord(ra, dec));
    }

    errors.extend(validate_position_angle(
        params.position_angle_degrees,
        "position_angle_degrees",
    ));

    if let Some(scheduling) = &params.scheduling {
        errors.extend(validate_scheduling(scheduling, "scheduling"));
    }
    if let Some(grading) = &params.grading {
        errors.extend(validate_grading(grading, "grading"));
    }

    let roster = filter_roster(equipment);
    match &params.goals {
        Some(goals) => {
            if let Err(goal_errors) = validate_goals(goals, &roster, "goals") {
                errors.extend(goal_errors);
            }
        }
        // Config defaults still have to satisfy the roster: a rig whose
        // default_goals name a filter it no longer has would otherwise
        // fail at the write rather than here.
        None => {
            if !roster.is_empty() {
                for (index, goal) in defaults.iter().enumerate() {
                    if !roster.iter().any(|f| f == &goal.filter) {
                        errors.push(err(
                            format!("goals[{index}].filter"),
                            format!(
                                "default goal filter {:?} is not in the configured filter roster ({})",
                                goal.filter,
                                roster.join(", ")
                            ),
                        ));
                    }
                }
            }
        }
    }

    errors
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn goal(filter: &str, binning: &str, exposure: &str, count: u32) -> GoalWire {
        GoalWire {
            filter: filter.to_string(),
            binning: binning.to_string(),
            exposure_duration: exposure.to_string(),
            desired_count: count,
        }
    }

    fn paths(errors: &[FieldError]) -> Vec<&str> {
        errors.iter().map(|e| e.path.as_str()).collect()
    }

    #[test]
    fn a_well_formed_goal_set_parses() {
        let goals = validate_goals(&[goal("Red", "1x1", "2m", 10)], &[], "goals").unwrap();
        assert_eq!(goals.len(), 1);
        assert_eq!(goals[0].desired_count, 10);
    }

    #[test]
    fn each_malformed_goal_field_reports_its_own_indexed_path() {
        let errors = validate_goals(
            &[
                goal("Red", "1x1", "2m", 10),
                goal("Red", "nope", "not-a-duration", 0),
            ],
            &[],
            "goals",
        )
        .unwrap_err();
        let paths = paths(&errors);
        assert!(paths.contains(&"goals[1].binning"), "{paths:?}");
        assert!(paths.contains(&"goals[1].exposure_duration"), "{paths:?}");
        assert!(paths.contains(&"goals[1].desired_count"), "{paths:?}");
        assert!(
            !paths.iter().any(|p| p.starts_with("goals[0]")),
            "the well-formed goal must not be blamed: {paths:?}"
        );
    }

    /// The set-level rule is the store's, consulted rather than
    /// restated — so a duplicate key cannot pass validation and then
    /// fail the write.
    #[test]
    fn a_duplicate_goal_key_is_caught_by_the_stores_own_invariant() {
        let errors = validate_goals(
            &[goal("Red", "1x1", "2m", 10), goal("Red", "1x1", "2m", 5)],
            &[],
            "goals",
        )
        .unwrap_err();
        assert_eq!(paths(&errors), vec!["goals"]);
        assert!(errors[0].msg.contains("duplicate"), "{:?}", errors[0]);
    }

    #[test]
    fn an_empty_roster_accepts_any_filter_name() {
        assert!(validate_goals(&[goal("Whatever", "1x1", "2m", 1)], &[], "goals").is_ok());
    }

    #[test]
    fn a_filter_outside_a_configured_roster_is_reported_against_the_goal() {
        let roster = vec!["Red".to_string(), "Blue".to_string()];
        let errors =
            validate_goals(&[goal("Sulphur", "1x1", "2m", 1)], &roster, "goals").unwrap_err();
        assert_eq!(paths(&errors), vec!["goals[0].filter"]);
    }

    #[test]
    fn a_zero_exposure_is_rejected_before_it_reaches_the_store() {
        let errors = validate_goals(&[goal("Red", "1x1", "0s", 1)], &[], "goals").unwrap_err();
        assert_eq!(paths(&errors), vec!["goals[0].exposure_duration"]);
    }

    #[test]
    fn coordinate_failures_are_attributed_to_the_field_the_typed_error_names() {
        assert_eq!(paths(&validate_coord(25.0, 0.0)), vec!["ra_hours"]);
        assert_eq!(paths(&validate_coord(5.0, 100.0)), vec!["dec_degrees"]);
        assert!(validate_coord(5.0, 10.0).is_empty());
    }

    #[test]
    fn grading_thresholds_must_be_finite_and_non_negative() {
        let bad = GradingWire {
            max_hfr_pixels: Some(-1.0),
            min_star_count: Some(10),
            max_eccentricity: Some(f64::NAN),
            min_snr: Some(3.0),
        };
        let errors = validate_grading(&bad, "grading");
        let paths = paths(&errors);
        assert!(paths.contains(&"grading.max_hfr_pixels"), "{paths:?}");
        assert!(paths.contains(&"grading.max_eccentricity"), "{paths:?}");
        assert!(!paths.contains(&"grading.min_snr"), "{paths:?}");
    }

    #[test]
    fn scheduling_overrides_are_range_checked_per_field() {
        let bad = SchedulingWire {
            min_altitude_degrees: Some(120.0),
            min_moon_separation_degrees: Some(-5.0),
            max_moon_illumination_fraction: Some(1.5),
            meridian_window_hours: Some(2.0),
        };
        let errors = validate_scheduling(&bad, "scheduling");
        let paths = paths(&errors);
        assert!(
            paths.contains(&"scheduling.min_altitude_degrees"),
            "{paths:?}"
        );
        assert!(
            paths.contains(&"scheduling.min_moon_separation_degrees"),
            "{paths:?}"
        );
        assert!(
            paths.contains(&"scheduling.max_moon_illumination_fraction"),
            "{paths:?}"
        );
        assert!(
            !paths.contains(&"scheduling.meridian_window_hours"),
            "the in-range field must not be blamed: {paths:?}"
        );
    }

    #[test]
    fn naming_patterns_are_checked_by_the_same_validators_config_load_uses() {
        let errors = validate_naming_patterns(Some("{target}_{bogus_token}"), None);
        assert_eq!(paths(&errors), vec!["file_naming_pattern"]);
        assert!(validate_naming_patterns(None, None).is_empty());
    }

    /// `validate_add_target` is the whole-payload entry point both
    /// `add_target` and `validate_plan` call, and its form-selection
    /// matrix is the branchiest thing in this module. BDD exercises the
    /// happy paths through the real tools; these pin the rejections
    /// directly, where each branch is one line to state.
    fn params(value: serde_json::Value) -> AddTargetParams {
        serde_json::from_value(value).expect("test payload must deserialize")
    }

    fn add_target_paths(value: serde_json::Value) -> Vec<String> {
        let errors = validate_add_target(&params(value), &[], &EquipmentRegistry::default());
        errors.into_iter().map(|e| e.path).collect()
    }

    #[test]
    fn a_minimal_display_name_payload_validates_clean() {
        assert_eq!(
            add_target_paths(serde_json::json!({
                "display_name": "Field", "ra_hours": 5.0, "dec_degrees": 10.0
            })),
            Vec::<String>::new()
        );
    }

    #[test]
    fn exactly_one_naming_form_is_required() {
        for value in [
            serde_json::json!({"ra_hours": 5.0, "dec_degrees": 10.0}),
            serde_json::json!({
                "catalog_ref": "M 31", "display_name": "Field",
                "ra_hours": 5.0, "dec_degrees": 10.0
            }),
        ] {
            assert_eq!(add_target_paths(value), vec!["display_name".to_string()]);
        }
    }

    #[test]
    fn the_display_name_form_requires_both_coordinates() {
        assert_eq!(
            add_target_paths(serde_json::json!({"display_name": "Field", "ra_hours": 5.0})),
            vec!["ra_hours".to_string()]
        );
    }

    /// A catalog add may omit coordinates entirely (the centroid is
    /// used) but may not supply half of a framing override.
    #[test]
    fn the_catalog_form_takes_coordinates_together_or_not_at_all() {
        assert_eq!(
            add_target_paths(serde_json::json!({"catalog_ref": "M 31"})),
            Vec::<String>::new()
        );
        assert_eq!(
            add_target_paths(serde_json::json!({"catalog_ref": "M 31", "ra_hours": 5.0})),
            vec!["ra_hours".to_string()]
        );
    }

    /// rp names, activates, annotates and frames an import itself, so
    /// every operator-surface field is refused rather than silently
    /// dropped.
    #[test]
    fn the_import_form_refuses_every_operator_field() {
        let source = serde_json::json!({
            "kind": "planetarium-bridge", "client": "192.0.2.10:52441",
            "received_at": "2026-08-01T00:00:00Z"
        });
        for (field, value) in [
            ("catalog_ref", serde_json::json!("M 31")),
            ("display_name", serde_json::json!("Field")),
            ("active", serde_json::json!(true)),
            ("notes", serde_json::json!("hi")),
            ("position_angle_degrees", serde_json::json!(10.0)),
            ("grading", serde_json::json!({"max_hfr_pixels": 3.0})),
        ] {
            let payload = serde_json::json!({
                "ra_hours": 5.0, "dec_degrees": 10.0, "source": source, field: value
            });
            let paths = add_target_paths(payload);
            assert!(
                paths.contains(&field.to_string()),
                "import form must refuse {field:?}, got {paths:?}"
            );
        }
    }

    #[test]
    fn the_import_form_requires_coordinates() {
        let paths = add_target_paths(serde_json::json!({
            "source": {
                "kind": "planetarium-bridge", "client": "192.0.2.10:52441",
                "received_at": "2026-08-01T00:00:00Z"
            }
        }));
        assert_eq!(paths, vec!["ra_hours".to_string()]);
    }

    /// Payload rules compose: a bad coordinate and a bad angle are both
    /// reported, which is what lets a form be filled in once rather
    /// than one round trip per field.
    #[test]
    fn every_payload_violation_is_reported_in_one_pass() {
        let paths = add_target_paths(serde_json::json!({
            "display_name": "Field", "ra_hours": 25.0, "dec_degrees": 10.0,
            "position_angle_degrees": 400.0
        }));
        assert!(paths.contains(&"ra_hours".to_string()), "{paths:?}");
        assert!(
            paths.contains(&"position_angle_degrees".to_string()),
            "{paths:?}"
        );
    }

    #[test]
    fn a_position_angle_outside_its_domain_is_reported_at_its_path() {
        assert!(validate_position_angle(None, "position_angle_degrees").is_empty());
        assert!(validate_position_angle(Some(0.0), "position_angle_degrees").is_empty());
        let errors = validate_position_angle(Some(400.0), "position_angle_degrees");
        assert_eq!(paths(&errors), vec!["position_angle_degrees"]);
    }
}
