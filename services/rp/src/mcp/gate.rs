//! Tool classes for safety enforcement (rp.md § Safety → In-Flight
//! Tool Calls).
//!
//! Every tool in the catalog is either **gated** — it moves the mount
//! towards the sky or exposes the optics to it — or **ungated**. The
//! class drives exactly two behaviours: which calls the safety gate
//! refuses with [`safety_unsafe_error`] while conditions are unsafe
//! (checked at dispatch in `ServerHandler::call_tool`), and which
//! in-flight calls the unsafe transition cancels
//! ([`super::inflight::InFlight::cancel_for_safety`], see
//! [`cancelled_on_unsafe`]). A tool that stops or secures (`park`,
//! `abort_slew`, `close_cover`, ...) is never gated, because it is what
//! the transition itself does, and every indoor actuator stays
//! available so an unsafe hour can still go to darks, bias, panel flats
//! and cooling (tenet 1).
//!
//! There is no default class: [`class_of`] returns `None` for a name
//! that is in neither list, and the unit test below walks the merged
//! tool router so a new tool that forgets to name its class fails the
//! build rather than silently landing in one class or the other.
//!
//! The built-in table is a default, not a verdict: the operator's
//! `safety.gate` config (rp.md § Configuration) moves tools across the
//! line, and [`ClassTable`] is the effective table after those
//! overrides — the one the dispatch, the registry and
//! `get_safety_status` all read.

use std::collections::BTreeMap;

use rmcp::model::{ErrorCode, ErrorData};
use rusty_photon_config::actions::FieldError;

use crate::config::GateOverrides;

/// Whether a tool call is refused while unsafe and cancelled by the
/// unsafe transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolClass {
    /// Moves the mount towards the sky or exposes the optics: refused
    /// with `SafetyUnsafe` while conditions are unsafe, cancelled by
    /// the unsafe transition.
    Gated,
    /// Reads, stop/secure commands, and indoor actuators: answers
    /// whatever the conditions and runs to completion through an
    /// unsafe transition.
    Ungated,
}

/// Tools the safety gate refuses and the unsafe transition cancels.
///
/// `unpark` and `set_tracking` move nothing by themselves but are the
/// door to motion; the guiding trio are guide pulses (mount motion)
/// through optics that should be covered; `open_cover` exposes the
/// optics.
pub const GATED: &[&str] = &[
    "slew",
    "center_on_target",
    "unpark",
    "set_tracking",
    "dither",
    "start_guiding",
    "resume_guiding",
    "open_cover",
];

/// Every other tool in the catalog. Listed explicitly rather than
/// derived as "not gated" so the no-default invariant holds: a tool
/// missing from both lists has no class.
pub const UNGATED: &[&str] = &[
    // camera
    "capture",
    "get_camera_info",
    // imaging
    "compute_image_stats",
    "measure_basic",
    "estimate_background",
    "detect_stars",
    "measure_stars",
    "compute_snr",
    // filter wheel
    "set_filter",
    "get_filter",
    // cover / calibrator
    "get_cover_state",
    "close_cover",
    "calibrator_on",
    "calibrator_off",
    // focuser
    "move_focuser",
    "get_focuser_position",
    "get_focuser_temperature",
    // mount
    "sync_mount",
    "get_mount_position",
    "get_tracking",
    "park",
    "get_park_state",
    "abort_slew",
    // auto-focus
    "auto_focus",
    "refocus_train",
    // rotator
    "move_rotator",
    "get_rotator_position",
    // plate solve
    "plate_solve",
    // guider
    "stop_guiding",
    "pause_guiding",
    "get_guiding_stats",
    // safety
    "get_safety_status",
    // planner
    "resolve_target",
    "compute_alt_az",
    "compute_transit",
    "compute_rise_set",
    "compute_meridian_flip",
    "get_sun_position",
    "get_twilight",
    "get_moon_position",
    "compute_moon_separation",
    "get_site",
    "get_local_sidereal_time",
    "get_target_status",
    "get_next_target",
    "record_exposure",
    "get_session_progress",
    "get_meridian_status",
    // targets
    "add_target",
    "get_target",
    "list_targets",
    "update_target",
    "delete_target",
    "set_goals",
    // plan schema / validation
    "get_plan_schema",
    "validate_plan",
];

/// The built-in class of a catalog tool.
///
/// `None` for a name in neither list (an unknown tool, or a new one
/// that has not yet been classified). The effective class after
/// operator overrides is [`ClassTable::class_of`].
#[must_use]
pub fn class_of(tool: &str) -> Option<ToolClass> {
    if GATED.contains(&tool) {
        Some(ToolClass::Gated)
    } else if UNGATED.contains(&tool) {
        Some(ToolClass::Ungated)
    } else {
        None
    }
}

/// Whether an in-flight call of `tool` is cancelled by the unsafe
/// transition.
///
/// Every gated call is, plus `capture`: that is the transition's
/// abort-exposure step delivered through the body, so the caller learns
/// its frame died for safety (`cancelled: safety`) rather than seeing a
/// bare "exposure aborted" hardware error. `capture` itself stays
/// ungated — darks and bias while unsafe are tenet 1. Every other
/// ungated body runs to completion.
#[must_use]
pub fn cancelled_on_unsafe(tool: &str, class: ToolClass) -> bool {
    class == ToolClass::Gated || tool == "capture"
}

/// The JSON-RPC error code of a safety refusal (`SafetyUnsafe`).
///
/// In the implementation-defined range the MCP spec leaves to servers
/// (`-32000..=-32019`). Pinned by rp.md § Safety → In-Flight Tool Calls
/// and mirrored by `rp-mcp-client`'s `SAFETY_UNSAFE_CODE`.
pub const SAFETY_UNSAFE_CODE: ErrorCode = ErrorCode(-32010);

/// The one-line message on every safety refusal.
pub const SAFETY_UNSAFE_MESSAGE: &str = "safety: conditions are unsafe";

/// The `SafetyUnsafe` error a gated tool is answered with while unsafe.
///
/// Code [`SAFETY_UNSAFE_CODE`], `data.reason` `"safety"`, `data.monitor`
/// naming an unsafe monitor (`null` when none can be named).
#[must_use]
pub fn safety_unsafe_error(monitor: Option<&str>) -> ErrorData {
    ErrorData::new(
        SAFETY_UNSAFE_CODE,
        SAFETY_UNSAFE_MESSAGE,
        Some(serde_json::json!({ "reason": "safety", "monitor": monitor })),
    )
}

/// The effective class table: the built-in [`GATED`] / [`UNGATED`]
/// default with the operator's `safety.gate` overrides applied. One
/// table drives the gate and the cancellation alike — there is one
/// class.
#[derive(Debug, Clone)]
pub struct ClassTable {
    classes: BTreeMap<String, ToolClass>,
}

impl Default for ClassTable {
    fn default() -> Self {
        Self::built_in()
    }
}

impl ClassTable {
    /// The built-in table, no overrides.
    #[must_use]
    pub fn built_in() -> Self {
        let classes = GATED
            .iter()
            .map(|name| ((*name).to_owned(), ToolClass::Gated))
            .chain(
                UNGATED
                    .iter()
                    .map(|name| ((*name).to_owned(), ToolClass::Ungated)),
            )
            .collect();
        Self { classes }
    }

    /// The built-in table with `overrides` applied: every name in
    /// `gated` becomes gated, every name in `ungated` ungated.
    ///
    /// # Errors
    ///
    /// Returns every offending entry, with its dotted config path
    /// (`safety.gate.gated.0`): a name that is not in the catalog, or a
    /// name listed on both sides. Same rules as [`override_errors`] —
    /// this is the constructor the startup path uses after the config
    /// loader already ran them, so a failure here is a programming
    /// error rather than an operator one.
    pub fn with_overrides(overrides: &GateOverrides) -> Result<Self, Vec<FieldError>> {
        let errors = override_errors(overrides);
        if !errors.is_empty() {
            return Err(errors);
        }
        let mut table = Self::built_in();
        for name in &overrides.gated {
            table.classes.insert(name.clone(), ToolClass::Gated);
        }
        for name in &overrides.ungated {
            table.classes.insert(name.clone(), ToolClass::Ungated);
        }
        Ok(table)
    }

    /// The effective class of `tool`, or `None` for a name that is not
    /// in the catalog.
    #[must_use]
    pub fn class_of(&self, tool: &str) -> Option<ToolClass> {
        self.classes.get(tool).copied()
    }

    /// The gated tools, in name order — what `get_safety_status`
    /// reports and the startup log prints.
    #[must_use]
    pub fn gated(&self) -> Vec<&str> {
        self.classes
            .iter()
            .filter(|(_, class)| **class == ToolClass::Gated)
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

/// Validate a `safety.gate` block against the catalog.
///
/// For the config loader and `PUT /api/config` (rp.md § In-Flight Tool
/// Calls → Operator overrides): each name must exist in the catalog,
/// and a name may not be on both sides. Every offending entry is
/// reported with its dotted path so a UI can point at it. Empty means
/// valid.
#[must_use]
pub fn override_errors(overrides: &GateOverrides) -> Vec<FieldError> {
    let mut errors = Vec::new();
    for (index, name) in overrides.gated.iter().enumerate() {
        if overrides.ungated.contains(name) {
            errors.push(FieldError {
                path: format!("safety.gate.gated.{index}"),
                msg: format!("tool `{name}` is listed as both gated and ungated"),
            });
        } else if class_of(name).is_none() {
            errors.push(unknown_tool("gated", index, name));
        }
    }
    for (index, name) in overrides.ungated.iter().enumerate() {
        // The both-sides case was already reported on the gated side.
        if !overrides.gated.contains(name) && class_of(name).is_none() {
            errors.push(unknown_tool("ungated", index, name));
        }
    }
    errors
}

fn unknown_tool(list: &str, index: usize, name: &str) -> FieldError {
    FieldError {
        path: format!("safety.gate.{list}.{index}"),
        msg: format!("unknown tool `{name}`: safety.gate names must exist in the tool catalog"),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::mcp::McpHandler;

    fn catalog() -> BTreeSet<String> {
        McpHandler::merged_tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    fn overrides(gated: &[&str], ungated: &[&str]) -> GateOverrides {
        GateOverrides {
            gated: gated.iter().map(|s| (*s).to_owned()).collect(),
            ungated: ungated.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// The no-default invariant: a tool that reaches the catalog
    /// without a class fails here, naming the offender.
    #[test]
    fn every_catalog_tool_has_a_class() {
        let unclassified: Vec<String> = catalog()
            .into_iter()
            .filter(|name| class_of(name).is_none())
            .collect();
        assert!(
            unclassified.is_empty(),
            "tools without a class (add them to GATED or UNGATED in mcp/gate.rs): {unclassified:?}"
        );
    }

    /// The reverse: a classified name that no longer exists in the
    /// catalog is a stale entry, not a harmless one.
    #[test]
    fn every_classified_tool_is_in_the_catalog() {
        let catalog = catalog();
        let stale: Vec<&str> = GATED
            .iter()
            .chain(UNGATED.iter())
            .copied()
            .filter(|name| !catalog.contains(*name))
            .collect();
        assert!(
            stale.is_empty(),
            "classified tools missing from the catalog: {stale:?}"
        );
    }

    #[test]
    fn no_tool_is_in_both_lists() {
        let both: Vec<&str> = GATED
            .iter()
            .copied()
            .filter(|name| UNGATED.contains(name))
            .collect();
        assert!(
            both.is_empty(),
            "tools listed as both gated and ungated: {both:?}"
        );
    }

    /// The D5 table, pinned: a change to the gated set is a design
    /// change (rp.md § In-Flight Tool Calls) and should show up here.
    #[test]
    fn the_gated_set_matches_the_design_table() {
        let gated: BTreeSet<&str> = GATED.iter().copied().collect();
        let expected: BTreeSet<&str> = [
            "slew",
            "center_on_target",
            "unpark",
            "set_tracking",
            "dither",
            "start_guiding",
            "resume_guiding",
            "open_cover",
        ]
        .into_iter()
        .collect();
        assert_eq!(gated, expected);
    }

    #[test]
    fn the_unsafe_transition_cancels_gated_calls_and_captures_only() {
        assert!(cancelled_on_unsafe("slew", ToolClass::Gated));
        assert!(cancelled_on_unsafe("capture", ToolClass::Ungated));
        assert!(!cancelled_on_unsafe("park", ToolClass::Ungated));
        assert!(!cancelled_on_unsafe("set_filter", ToolClass::Ungated));
    }

    #[test]
    fn class_of_reports_each_class_and_unknown() {
        assert_eq!(class_of("slew"), Some(ToolClass::Gated));
        assert_eq!(class_of("park"), Some(ToolClass::Ungated));
        assert_eq!(class_of("no_such_tool"), None);
    }

    /// The wire shape rp.md pins: `-32010`, `data.reason`, `data.monitor`.
    #[test]
    fn the_safety_unsafe_error_carries_code_reason_and_monitor() {
        let error = safety_unsafe_error(Some("weather-watcher"));
        assert_eq!(error.code, ErrorCode(-32010));
        assert_eq!(error.message, SAFETY_UNSAFE_MESSAGE);
        let data = error.data.expect("data expected");
        assert_eq!(data["reason"], "safety");
        assert_eq!(data["monitor"], "weather-watcher");
    }

    #[test]
    fn the_safety_unsafe_error_names_no_monitor_as_null() {
        let error = safety_unsafe_error(None);
        let data = error.data.expect("data expected");
        assert!(data["monitor"].is_null(), "got: {data}");
    }

    #[test]
    fn the_built_in_table_matches_the_lists() {
        let table = ClassTable::built_in();
        assert_eq!(table.class_of("slew"), Some(ToolClass::Gated));
        assert_eq!(table.class_of("capture"), Some(ToolClass::Ungated));
        assert_eq!(table.class_of("no_such_tool"), None);
        let gated: BTreeSet<&str> = table.gated().into_iter().collect();
        assert_eq!(gated, GATED.iter().copied().collect());
    }

    #[test]
    fn an_override_moves_a_tool_across_the_gate_in_both_directions() {
        let table = ClassTable::with_overrides(&overrides(&["auto_focus"], &["open_cover"]))
            .expect("valid overrides");
        assert_eq!(table.class_of("auto_focus"), Some(ToolClass::Gated));
        assert_eq!(table.class_of("open_cover"), Some(ToolClass::Ungated));
        // Everything else is untouched.
        assert_eq!(table.class_of("slew"), Some(ToolClass::Gated));
        assert_eq!(table.class_of("capture"), Some(ToolClass::Ungated));
        let gated = table.gated();
        assert!(gated.contains(&"auto_focus"), "got: {gated:?}");
        assert!(!gated.contains(&"open_cover"), "got: {gated:?}");
        assert!(
            gated.windows(2).all(|pair| pair[0] < pair[1]),
            "gated list must be sorted: {gated:?}"
        );
    }

    #[test]
    fn an_unknown_tool_is_rejected_naming_its_entry() {
        let errors = override_errors(&overrides(&["slew", "no_such_tool"], &["nor_this"]));
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["safety.gate.gated.1", "safety.gate.ungated.0"]);
        assert!(errors[0].msg.contains("no_such_tool"), "{}", errors[0].msg);
        assert!(errors[1].msg.contains("nor_this"), "{}", errors[1].msg);
        assert!(ClassTable::with_overrides(&overrides(&["no_such_tool"], &[])).is_err());
    }

    #[test]
    fn a_tool_on_both_sides_is_rejected_once_on_the_gated_side() {
        let errors = override_errors(&overrides(&["open_cover"], &["open_cover"]));
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert_eq!(errors[0].path, "safety.gate.gated.0");
        assert!(
            errors[0].msg.contains("both gated and ungated"),
            "{}",
            errors[0].msg
        );
    }

    #[test]
    fn empty_overrides_are_valid_and_change_nothing() {
        assert!(override_errors(&GateOverrides::default()).is_empty());
        let table = ClassTable::with_overrides(&GateOverrides::default()).expect("valid");
        assert_eq!(table.gated(), ClassTable::built_in().gated());
    }
}
