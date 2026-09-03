//! Tool classes for safety enforcement (rp.md § Safety → In-Flight
//! Tool Calls).
//!
//! Every tool in the catalog is either **gated** — it moves the mount
//! towards the sky or exposes the optics to it — or **ungated**. The
//! class drives which in-flight calls the unsafe transition cancels
//! ([`super::inflight::InFlight::cancel_for_safety`], see
//! [`cancelled_on_unsafe`]); a tool that stops or secures (`park`,
//! `abort_slew`, `close_cover`, ...) is never gated, because it is what
//! the transition itself does, and every indoor actuator stays
//! available so an unsafe hour can still go to darks, bias, panel flats
//! and cooling (tenet 1).
//!
//! There is no default class: [`class_of`] returns `None` for a name
//! that is in neither list, and the unit test below walks the merged
//! tool router so a new tool that forgets to name its class fails the
//! build rather than silently landing in one class or the other.

/// Whether a tool call is cancelled by the unsafe transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolClass {
    /// Moves the mount towards the sky or exposes the optics: cancelled
    /// by the unsafe transition.
    Gated,
    /// Reads, stop/secure commands, and indoor actuators: runs to
    /// completion through an unsafe transition.
    Ungated,
}

/// Tools the unsafe transition cancels.
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

/// The class of a catalog tool, or `None` for a name in neither list
/// (an unknown tool, or a new one that has not yet been classified).
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
}
