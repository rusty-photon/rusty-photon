//! BDD step definitions for rp's train lease (`train_lease.feature`).
//!
//! The imaging-train rigs run on the simulator: its focuser, filter
//! wheel and camera, in one train "main". The guiding-train rigs pair
//! the simulator's focuser with the simulator's camera, behind the stub
//! guider a prior Given started, since a guiding train needs the
//! guiding config; its guide camera guard is in
//! `guide_camera_guard.feature`.
//!
//! Shared steps live elsewhere: `the tool call should return an error`
//! and `the error message should contain {string}` in `tool_steps.rs`,
//! the stub guider's Given in `guider_steps.rs`, the background calls
//! and their joins in `motion_gate_steps.rs`, the event waits in
//! `event_steps.rs`, and `the MCP client calls auto_focus with train
//! {string}, step_size {int} and half_width {int}` in
//! `guide_focus_steps.rs`.

use std::time::Duration;

use cucumber::{given, then, when};
use serde_json::{json, Value};

use bdd_infra::rp_harness::{FocuserConfig, OpticalTrainConfig};

use crate::steps::focuser_steps::add_focuser;
use crate::steps::motion_gate_steps::add_stub_guider;
use crate::steps::rotator_steps::{add_offline_camera, push_train};
use crate::steps::tool_steps::{
    add_camera, add_filter_wheel, ensure_mcp_client, ensure_omnisim, start_rp,
};
use crate::world::RpWorld;

// --- Given steps: rigs ------------------------------------------------

/// The simulator's focuser, filter wheel and camera, in that optical
/// order, as imaging train `train_id`.
async fn add_imaging_rig(world: &mut RpWorld, train_id: &str) {
    ensure_omnisim(world).await;
    add_focuser(world, None, None, None);
    add_filter_wheel(world);
    add_camera(world);
    push_train(
        world,
        train_id,
        vec![
            "main-focuser".to_string(),
            "main-fw".to_string(),
            "main-cam".to_string(),
        ],
    );
}

#[given(
    expr = "rp is running with imaging train {string} of a focuser, a filter wheel and a camera on the simulator"
)]
async fn rp_with_imaging_rig(world: &mut RpWorld, train_id: String) {
    add_imaging_rig(world, &train_id).await;
    start_rp(world).await;
}

#[given(
    expr = "rp is running with imaging train {string} of a focuser, a filter wheel and a camera on the simulator and a stub guider"
)]
async fn rp_with_imaging_rig_and_guider(world: &mut RpWorld, train_id: String) {
    add_imaging_rig(world, &train_id).await;
    add_stub_guider(world, Duration::ZERO).await;
    start_rp(world).await;
}

#[given(
    expr = "rp is running with imaging train {string} of a focuser, a filter wheel and a camera on the simulator and an offline camera {string} in no train"
)]
async fn rp_with_imaging_rig_and_side_camera(
    world: &mut RpWorld,
    train_id: String,
    camera_id: String,
) {
    add_imaging_rig(world, &train_id).await;
    add_offline_camera(world, &camera_id);
    start_rp(world).await;
}

#[given(
    expr = "rp is running with guiding train {string} behind an offline upstream focuser {string} and a camera on the simulator"
)]
async fn rp_with_upstream_focuser(world: &mut RpWorld, train_id: String, upstream: String) {
    expect_stub_guider(world);
    ensure_omnisim(world).await;
    add_focuser(world, None, None, None);
    world.focusers.push(FocuserConfig {
        microns_per_step: None,
        id: upstream.clone(),
        alpaca_url: "not-a-url".to_string(),
        device_number: 0,
        min_position: None,
        max_position: None,
        backlash: None,
    });
    add_camera(world);
    world.optical_trains.push(OpticalTrainConfig {
        telescope: None,
        aperture_mm: None,
        obstruction_mm: None,
        id: train_id,
        purpose: Some("guiding".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices: vec![upstream, "main-focuser".to_string(), "main-cam".to_string()],
        auto_focus: None,
    });
    start_rp(world).await;
}

#[given(expr = "rp is running with a camera-only imaging train {string} on the simulator")]
async fn rp_with_camera_only_train(world: &mut RpWorld, train_id: String) {
    ensure_omnisim(world).await;
    add_camera(world);
    push_train(world, &train_id, vec!["main-cam".to_string()]);
    start_rp(world).await;
}

/// Guiding train `train_id`: the simulator's focuser, then `camera_id`.
fn push_guiding_train(world: &mut RpWorld, train_id: &str, camera_id: &str) {
    world.optical_trains.push(OpticalTrainConfig {
        telescope: None,
        aperture_mm: None,
        obstruction_mm: None,
        id: train_id.to_string(),
        purpose: Some("guiding".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices: vec!["main-focuser".to_string(), camera_id.to_string()],
        auto_focus: None,
    });
}

#[given(
    expr = "rp is running with guiding train {string} of a focuser and a camera on the simulator"
)]
async fn rp_with_simulator_guiding_train(world: &mut RpWorld, train_id: String) {
    expect_stub_guider(world);
    ensure_omnisim(world).await;
    add_focuser(world, None, None, None);
    add_camera(world);
    push_guiding_train(world, &train_id, "main-cam");
    start_rp(world).await;
}

/// A guiding train needs the guiding config, which the stub guider's
/// Given sets: the guiding-train rigs take that stub rather than one of
/// their own, so the scenario scripts the guider it runs against.
fn expect_stub_guider(world: &RpWorld) {
    assert!(
        world.guider_stub.is_some(),
        "a guiding-train rig needs a prior 'Given a stub guider in state ...' step"
    );
}

#[given(expr = "the MCP client holds train lease {string} on train {string}")]
async fn holds_train_lease(world: &mut RpWorld, lease_id: String, train_id: String) {
    acquire(world, &train_id, &lease_id, None).await;
    let result = world.last_tool_result.as_ref().expect("no tool result");
    assert!(
        result.is_ok(),
        "expected train lease '{lease_id}' on train '{train_id}' to be granted, got: {result:?}"
    );
}

// --- When steps --------------------------------------------------------

#[when(
    expr = "the MCP client calls \"acquire_train_lease\" for train {string} with lease id {string}"
)]
async fn call_acquire(world: &mut RpWorld, train_id: String, lease_id: String) {
    acquire(world, &train_id, &lease_id, None).await;
}

#[when(
    expr = "the MCP client calls \"acquire_train_lease\" for train {string} with lease id {string} for {string}"
)]
async fn call_acquire_for(
    world: &mut RpWorld,
    train_id: String,
    lease_id: String,
    max_duration: String,
) {
    acquire(world, &train_id, &lease_id, Some(&max_duration)).await;
}

#[when(expr = "the MCP client calls \"release_train_lease\" with lease id {string}")]
async fn call_release(world: &mut RpWorld, lease_id: String) {
    call(
        world,
        "release_train_lease",
        json!({ "lease_id": lease_id }),
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"move_focuser\" with focuser {string} to position {int} under lease {string}"
)]
async fn call_move_under_lease(
    world: &mut RpWorld,
    focuser_id: String,
    position: i32,
    lease_id: String,
) {
    call(
        world,
        "move_focuser",
        json!({ "focuser_id": focuser_id, "position": position, "lease_id": lease_id }),
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"set_filter\" with filter wheel {string} and filter {string} under lease {string}"
)]
async fn call_set_filter_under_lease(
    world: &mut RpWorld,
    filter_wheel_id: String,
    filter_name: String,
    lease_id: String,
) {
    call(
        world,
        "set_filter",
        json!({
            "filter_wheel_id": filter_wheel_id,
            "filter_name": filter_name,
            "lease_id": lease_id,
        }),
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"capture\" with camera {string} for {int} ms under lease {string}"
)]
async fn call_capture_under_lease(
    world: &mut RpWorld,
    camera_id: String,
    duration_ms: i32,
    lease_id: String,
) {
    call(
        world,
        "capture",
        json!({
            "camera_id": camera_id,
            "duration": format!("{duration_ms}ms"),
            "lease_id": lease_id,
        }),
    )
    .await;
}

// --- Then steps ---------------------------------------------------------

#[then("the train lease call should succeed")]
fn call_succeeded(world: &mut RpWorld) {
    let result = world.last_tool_result.as_ref().expect("no tool result");
    assert!(
        result.is_ok(),
        "expected the call to succeed, got: {result:?}"
    );
}

#[then(expr = "the train lease result field {string} should be {string}")]
fn result_field_is(world: &mut RpWorld, field: String, expected: String) {
    let value = result_field(world, &field);
    assert_eq!(value, json!(expected), "field '{field}'");
}

#[then(expr = "the train lease result field {string} should be true")]
fn result_field_is_true(world: &mut RpWorld, field: String) {
    assert_eq!(result_field(world, &field), json!(true), "field '{field}'");
}

#[then(expr = "the train lease result field {string} should be false")]
fn result_field_is_false(world: &mut RpWorld, field: String) {
    assert_eq!(result_field(world, &field), json!(false), "field '{field}'");
}

#[then(expr = "the train lease result list {string} should be {string}")]
fn result_list_is(world: &mut RpWorld, field: String, expected: String) {
    let expected: Vec<Value> = expected.split(", ").map(|id| json!(id)).collect();
    assert_eq!(
        result_field(world, &field),
        Value::Array(expected),
        "list '{field}'"
    );
}

#[then(expr = "the train lease result list {string} should be empty")]
fn result_list_is_empty(world: &mut RpWorld, field: String) {
    assert_eq!(result_field(world, &field), json!([]), "list '{field}'");
}

#[then(expr = "the train lease result should carry an RFC 3339 {string} time")]
fn result_carries_time(world: &mut RpWorld, field: String) {
    let value = result_field(world, &field);
    let text = value
        .as_str()
        .unwrap_or_else(|| panic!("expected '{field}' to be a string, got: {value}"));
    chrono::DateTime::parse_from_rfc3339(text)
        .unwrap_or_else(|e| panic!("'{field}' {text:?} is not RFC 3339: {e}"));
}

#[then(expr = "no {string} event arrives within {int} ms")]
async fn no_event_within(world: &mut RpWorld, event_type: String, window_ms: u64) {
    // Watch the whole window, failing at the first sighting, rather
    // than sampling once after a nap (testing.md §6.9).
    let deadline = tokio::time::Instant::now() + Duration::from_millis(window_ms);
    while tokio::time::Instant::now() < deadline {
        let seen = world
            .received_events
            .read()
            .await
            .iter()
            .any(|e| e.event_type == event_type);
        assert!(
            !seen,
            "a '{event_type}' event arrived inside the {window_ms} ms window"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// --- Helpers -------------------------------------------------------------

async fn call(world: &mut RpWorld, tool: &str, args: Value) {
    ensure_mcp_client(world).await;
    let result = world.mcp().call_tool(tool, args).await;
    world.last_tool_result = Some(result);
}

async fn acquire(world: &mut RpWorld, train_id: &str, lease_id: &str, max_duration: Option<&str>) {
    let mut args = json!({ "train_id": train_id, "lease_id": lease_id });
    if let Some(max_duration) = max_duration {
        args["max_duration"] = json!(max_duration);
    }
    call(world, "acquire_train_lease", args).await;
}

fn result_field(world: &RpWorld, field: &str) -> Value {
    let result = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .unwrap_or_else(|e| panic!("expected a result, the call failed: {e}"));
    result
        .get(field)
        .cloned()
        .unwrap_or_else(|| panic!("expected '{field}' in the result, got: {result}"))
}
