//! Step definitions for `capture_rig_state.feature`: the rig state a
//! capture copies into its exposure document (rp.md § Core Fields,
//! Rig state).

use cucumber::gherkin::Step;
use cucumber::{given, then};
use serde_json::Value;

use bdd_infra::rp_harness::{
    CannedGuiding, CannedTotals, GuiderConfig, GuiderStub, GuiderStubBehavior, MountConfig,
    ObservingConditionsConfig, OmniSimHandle, OpticalTrainConfig, RotatorConfig,
    SafetyMonitorConfig,
};

use crate::steps::event_steps::json_values_match;
use crate::steps::focuser_steps::add_focuser;
use crate::steps::tool_steps::{add_camera, add_filter_wheel, ensure_omnisim, start_rp};
use crate::world::RpWorld;

// --- Given steps: the whole rig ---------------------------------------

#[given("rp is running with the whole rig on the simulator")]
async fn rp_with_whole_rig(world: &mut RpWorld) {
    add_whole_rig(world, None, true).await;
    start_rp(world).await;
}

#[given(
    expr = "rp is running with the whole rig on the simulator and default position angle {float}"
)]
async fn rp_with_whole_rig_and_default_angle(world: &mut RpWorld, angle: f64) {
    add_whole_rig(world, Some(angle), true).await;
    start_rp(world).await;
}

#[given("rp is running with the whole rig on the simulator and its rotator offline")]
async fn rp_with_whole_rig_rotator_offline(world: &mut RpWorld) {
    add_whole_rig(world, None, false).await;
    start_rp(world).await;
}

/// A camera behind a focuser, a rotator and a filter wheel in train
/// "main", a mount, an observing conditions device "sim-weather" and a
/// safety monitor "weather-watcher", all on the simulator, and a site
/// (the naming template's `{night_date}` needs one). An offline
/// rotator keeps its roster entry and its place in the train, but
/// points nowhere, so its session never comes up.
async fn add_whole_rig(
    world: &mut RpWorld,
    default_position_angle: Option<f64>,
    rotator_online: bool,
) {
    ensure_omnisim(world).await;
    // Start from a safe reading whatever an earlier scenario left in
    // the simulator.
    OmniSimHandle::set_safety_monitor_is_safe(true)
        .await
        .expect("failed to reset OmniSim's safety monitor to safe");
    add_camera(world);
    add_filter_wheel(world);
    add_focuser(world, None, None, None);
    let url = world.omnisim_url();
    world.rotators.push(RotatorConfig {
        id: "main-rotator".to_string(),
        alpaca_url: if rotator_online {
            url.clone()
        } else {
            "not-a-url".to_string()
        },
        device_number: 0,
    });
    world.mount = Some(MountConfig {
        alpaca_url: url.clone(),
        device_number: 0,
        settle_after_slew: None,
    });
    world.observing_conditions.push(ObservingConditionsConfig {
        id: "sim-weather".to_string(),
        alpaca_url: url.clone(),
        device_number: 0,
    });
    world.safety_monitors.push(SafetyMonitorConfig {
        id: "weather-watcher".to_string(),
        alpaca_url: url,
        device_number: 0,
    });
    world.optical_trains.push(OpticalTrainConfig {
        telescope: None,
        aperture_mm: None,
        obstruction_mm: None,
        id: "main".to_string(),
        purpose: Some("imaging".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: default_position_angle,
        devices: vec![
            "main-focuser".to_string(),
            "main-rotator".to_string(),
            "main-fw".to_string(),
            "main-cam".to_string(),
        ],
        auto_focus: None,
    });
    world.site = Some((51.0786, -0.2944));
}

// --- Given steps: the guider service ----------------------------------

#[given(
    regex = r#"^a guider service reporting "(Guiding|Stopped)" whose running totals grow on every read by:$"#
)]
async fn guider_with_totals(world: &mut RpWorld, app_state: String, step: &Step) {
    let totals = totals_from_table(step, false);
    start_guider(world, &app_state, Some(totals)).await;
}

#[given(
    regex = r#"^a guider service reporting "(Guiding|Stopped)" that starts a new epoch on every read, its running totals growing on every read by:$"#
)]
async fn guider_with_new_epochs(world: &mut RpWorld, app_state: String, step: &Step) {
    let totals = totals_from_table(step, true);
    start_guider(world, &app_state, Some(totals)).await;
}

#[given(regex = r#"^a guider service reporting "(Guiding|Stopped)" without running totals$"#)]
async fn guider_without_totals(world: &mut RpWorld, app_state: String) {
    start_guider(world, &app_state, None).await;
}

async fn start_guider(world: &mut RpWorld, app_state: &str, totals: Option<CannedTotals>) {
    let stub = GuiderStub::start(GuiderStubBehavior::Canned(CannedGuiding {
        guiding: app_state == "Guiding",
        totals,
        ..CannedGuiding::default()
    }))
    .await;
    world.guider = Some(GuiderConfig::url_only(stub.url.clone()));
    world.guider_stub = Some(stub);
}

/// One read's growth, from the single data row under the header
/// `guide_steps | ra_sum_sq_px2 | dec_sum_sq_px2 | star_lost |
/// interruptions`.
fn totals_from_table(step: &Step, new_epoch_per_read: bool) -> CannedTotals {
    let table = step.table.as_ref().expect("the step needs a data table");
    let header = &table.rows[0];
    let row = &table.rows[1];
    let cell = |name: &str| -> &str {
        let column = header
            .iter()
            .position(|h| h == name)
            .unwrap_or_else(|| panic!("the table has no '{name}' column"));
        &row[column]
    };
    CannedTotals {
        guide_steps_per_read: cell("guide_steps").parse().expect("guide_steps"),
        ra_sum_sq_px2_per_read: cell("ra_sum_sq_px2").parse().expect("ra_sum_sq_px2"),
        dec_sum_sq_px2_per_read: cell("dec_sum_sq_px2").parse().expect("dec_sum_sq_px2"),
        star_lost_per_read: cell("star_lost").parse().expect("star_lost"),
        interruptions_per_read: cell("interruptions").parse().expect("interruptions"),
        new_epoch_per_read,
    }
}

// --- Then steps: the fetched document ---------------------------------

const fn document_body(world: &RpWorld) -> &Value {
    world
        .last_document_response_body
        .as_ref()
        .expect("no document response body recorded")
}

fn document_value<'a>(body: &'a Value, pointer: &str) -> &'a Value {
    body.pointer(pointer)
        .unwrap_or_else(|| panic!("no value at {pointer} in the document: {body}"))
}

/// A reading the simulator lets drift, such as its focuser's probe: the
/// field is there and holds a number, whatever its value.
#[then(expr = "the document at {string} should be a number")]
fn document_at_is_a_number(world: &mut RpWorld, pointer: String) {
    let body = document_body(world);
    let value = document_value(body, &pointer);
    assert!(
        value.is_number(),
        "expected a number at {pointer}, got {value} (document: {body})"
    );
}

#[then(regex = r#"^the document at "([^"]+)" should hold (\d+) entr(?:y|ies)$"#)]
fn document_at_holds_entries(world: &mut RpWorld, pointer: String, count: usize) {
    let body = document_body(world);
    let value = document_value(body, &pointer);
    let entries = value
        .as_array()
        .unwrap_or_else(|| panic!("expected a list at {pointer}, got {value}"));
    assert_eq!(
        entries.len(),
        count,
        "unexpected number of entries at {pointer}: {value}"
    );
}

/// Each row's `field` of the object at the pointer is the row's `json`,
/// or any number where the cell says `a number` (a reading the
/// simulator lets drift), and the object has no field the table does
/// not name.
#[then(expr = "the document at {string} should carry these fields:")]
fn document_at_carries_fields(world: &mut RpWorld, pointer: String, step: &Step) {
    let body = document_body(world);
    let object = document_value(body, &pointer)
        .as_object()
        .unwrap_or_else(|| panic!("expected an object at {pointer} in the document: {body}"));
    let table = step.table.as_ref().expect("the step needs a data table");
    let mut named = Vec::new();
    for row in table.rows.iter().skip(1) {
        let (field, json) = (&row[0], &row[1]);
        let actual = object
            .get(field)
            .unwrap_or_else(|| panic!("{pointer} has no '{field}': {object:?}"));
        if json == "a number" {
            assert!(
                actual.is_number(),
                "expected {pointer}/{field} to be a number, got {actual}"
            );
        } else {
            let expected: Value = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("the json cell for {field} must be JSON: {e}"));
            assert!(
                json_values_match(actual, &expected),
                "expected {pointer}/{field} to be {expected}, got {actual}"
            );
        }
        named.push(field.as_str());
    }
    let unnamed: Vec<&String> = object
        .keys()
        .filter(|key| !named.contains(&key.as_str()))
        .collect();
    assert!(
        unnamed.is_empty(),
        "{pointer} carries fields the table does not name: {unnamed:?}"
    );
}

/// Absent, not `null`, at every listed pointer.
#[then("the document should have no value at any of these paths:")]
fn document_has_no_value_at_any(world: &mut RpWorld, step: &Step) {
    let body = document_body(world);
    let table = step.table.as_ref().expect("the step needs a data table");
    let present: Vec<&str> = table
        .rows
        .iter()
        .skip(1)
        .map(|row| row[0].as_str())
        .filter(|pointer| body.pointer(pointer).is_some())
        .collect();
    assert!(
        present.is_empty(),
        "expected nothing at {present:?} (document: {body})"
    );
}
