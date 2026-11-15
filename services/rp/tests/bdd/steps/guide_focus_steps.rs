//! BDD step definitions for guide-train focus: `guide_focus.feature`
//! (the guide-train sweep's refusals, the guide focus watch), and the
//! stub guider and rotate-while-guiding ladder assertions other
//! guiding scenarios share.
//!
//! Shared steps: tool listing / error assertions in `tool_steps.rs`,
//! the stub guider's Given in `guider_steps.rs`, webhook receiver in
//! `event_steps.rs`, offline-roster helpers in `rotator_steps.rs`.
//! The stub guider's HFD script drives the deterministic outcomes
//! here: a one-value script is perfectly flat HFD (the sweep's fit then
//! reports no minimum), a two-value script is the degradation the
//! focus watch turns into events.

use cucumber::{given, then, when};
use serde_json::{Map, Value};

use crate::steps::focuser_steps::add_focuser;
use crate::steps::guider_steps::guider_stub as stub;
use crate::steps::rotator_steps::{add_offline_camera, last_rotator_result};
use crate::steps::tool_steps::{ensure_mcp_client, ensure_omnisim, start_rp};
use crate::world::RpWorld;

// --- Given steps: the stub guider's metrics --------------------------

#[given(expr = "the stub guider serves the HFD script {string}")]
fn stub_serves_hfd_script(world: &mut RpWorld, script: String) {
    // `lost` serves star-lost frames.
    let values: Vec<Option<f64>> = script
        .split(',')
        .map(str::trim)
        .map(|s| {
            (s != "lost").then(|| {
                s.parse()
                    .expect("HFD script values must be numbers or lost")
            })
        })
        .collect();
    stub(world).set_hfd_script_with_lost(values);
}

// --- Given steps: equipment compositions ----------------------------

fn push_guiding_focuser_train(world: &mut RpWorld, train_id: String, with_block: bool) {
    let auto_focus = with_block.then_some(bdd_infra::rp_harness::TrainAutoFocusConfig {
        step_size: 50,
        half_width: 100,
        frames_per_step: Some(2),
    });
    world
        .optical_trains
        .push(bdd_infra::rp_harness::OpticalTrainConfig {
            telescope: None,
            aperture_mm: None,
            obstruction_mm: None,
            id: train_id,
            purpose: Some("guiding".to_string()),
            focal_length_mm: None,
            default_position_angle_degrees: None,
            devices: vec!["main-focuser".to_string(), "guide-cam".to_string()],
            auto_focus,
        });
}

#[given(
    expr = "rp is running with a focuser on the simulator in guiding train {string} with a metric auto_focus block"
)]
async fn rp_with_guide_focuser_and_block(world: &mut RpWorld, train_id: String) {
    ensure_omnisim(world).await;
    add_focuser(world, None, None, None);
    add_offline_camera(world, "guide-cam");
    push_guiding_focuser_train(world, train_id, true);
    start_rp(world).await;
}

/// The simulator focuser terminal in both an imaging train and the
/// guiding train — a guide path with no focuser of its own, as behind
/// an unmotorised OAG. Both cameras are offline: what these scenarios
/// assert is settled before a frame would be taken.
#[given(
    expr = "rp is running with a focuser on the simulator shared by imaging train {string} and guiding train {string}"
)]
async fn rp_with_focuser_shared_by_imaging_and_guiding(
    world: &mut RpWorld,
    imaging_id: String,
    guiding_id: String,
) {
    ensure_omnisim(world).await;
    add_focuser(world, None, None, None);
    add_offline_camera(world, "main-cam");
    add_offline_camera(world, "guide-cam");
    world
        .optical_trains
        .push(bdd_infra::rp_harness::OpticalTrainConfig {
            telescope: None,
            aperture_mm: None,
            obstruction_mm: None,
            id: imaging_id,
            purpose: Some("imaging".to_string()),
            focal_length_mm: None,
            default_position_angle_degrees: None,
            devices: vec!["main-focuser".to_string(), "main-cam".to_string()],
            auto_focus: None,
        });
    push_guiding_focuser_train(world, guiding_id, true);
    start_rp(world).await;
}

#[given(
    expr = "rp is running with a focuser on the simulator in guiding train {string} without an auto_focus block"
)]
async fn rp_with_guide_focuser_no_block(world: &mut RpWorld, train_id: String) {
    ensure_omnisim(world).await;
    add_focuser(world, None, None, None);
    add_offline_camera(world, "guide-cam");
    push_guiding_focuser_train(world, train_id, false);
    start_rp(world).await;
}

/// The watch needs only a guider with a `focus_watch` block — no
/// devices, no trains (`start_rp` auto-adds the simulator mount the
/// guiding block requires). Cooldown is left long so a degradation
/// fires exactly once per scenario.
#[given(
    expr = "rp is running with a guide focus watch of window {int}, poll interval {string}, and escalation deadline {string}"
)]
async fn rp_with_focus_watch(
    world: &mut RpWorld,
    window: i64,
    poll_interval: String,
    escalation_deadline: String,
) {
    ensure_omnisim(world).await;
    set_focus_watch(world, window, &poll_interval, &escalation_deadline);
    start_rp(world).await;
}

/// The watch with a guiding train configured, so the emitted events
/// carry the train's id for orchestrator wiring.
#[given(
    expr = "rp is running with guiding train {string} and a guide focus watch of window {int}, poll interval {string}, and escalation deadline {string}"
)]
async fn rp_with_train_and_focus_watch(
    world: &mut RpWorld,
    train_id: String,
    window: i64,
    poll_interval: String,
    escalation_deadline: String,
) {
    ensure_omnisim(world).await;
    add_focuser(world, None, None, None);
    add_offline_camera(world, "guide-cam");
    push_guiding_focuser_train(world, train_id, true);
    set_focus_watch(world, window, &poll_interval, &escalation_deadline);
    start_rp(world).await;
}

fn set_focus_watch(world: &mut RpWorld, window: i64, poll_interval: &str, escalation: &str) {
    let guider = world
        .guider
        .as_mut()
        .expect("this step expects a prior 'Given a stub guider ...' step");
    guider.focus_watch = Some(serde_json::json!({
        "window": window,
        "degrade_ratio": 1.25,
        "cooldown": "10m",
        "escalation_deadline": escalation,
        "poll_interval": poll_interval,
    }));
}

// --- When steps ------------------------------------------------------

#[when(expr = "the MCP client calls auto_focus with train {string}")]
async fn call_auto_focus_train(world: &mut RpWorld, train_id: String) {
    let mut args = Map::new();
    args.insert("train_id".into(), Value::String(train_id));
    call_auto_focus(world, args).await;
}

/// The sweep geometry per call, for a guiding train with no block.
#[when(
    expr = "the MCP client calls auto_focus with train {string}, step_size {int} and half_width {int}"
)]
async fn call_auto_focus_train_geometry(
    world: &mut RpWorld,
    train_id: String,
    step_size: i64,
    half_width: i64,
) {
    let mut args = Map::new();
    args.insert("train_id".into(), Value::String(train_id));
    args.insert("step_size".into(), Value::from(step_size));
    args.insert("half_width".into(), Value::from(half_width));
    call_auto_focus(world, args).await;
}

async fn call_auto_focus(world: &mut RpWorld, args: Map<String, Value>) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("auto_focus", Value::Object(args))
        .await;
    world.last_tool_result = Some(result);
}

// --- Then steps: stub guider request assertions ----------------------

#[then("the stub guider should have received a pause request with full false")]
fn stub_pause_request_full_false(world: &mut RpWorld) {
    let mut pauses = stub(world).requests_to("/guiding/pause");
    let request = pauses.pop().expect("stub guider received no pause request");
    assert_eq!(
        request.get("full").and_then(Value::as_bool),
        Some(false),
        "full mismatch in {request}"
    );
}

#[then("the stub guider should not have received a pause request")]
fn stub_no_pause_request(world: &mut RpWorld) {
    let pauses = stub(world).requests_to("/guiding/pause");
    assert!(
        pauses.is_empty(),
        "expected no pause request, got {pauses:?}"
    );
}

#[then(expr = "the stub guider should have received a {string} request")]
fn stub_received_request(world: &mut RpWorld, path_suffix: String) {
    let requests = stub(world).requests_to(&path_suffix);
    assert!(
        !requests.is_empty(),
        "expected a request to '{path_suffix}', got none; all: {:?}",
        stub(world).requests()
    );
}

#[then(expr = "the stub guider should not have received a {string} request")]
fn stub_not_received_request(world: &mut RpWorld, path_suffix: String) {
    let requests = stub(world).requests_to(&path_suffix);
    assert!(
        requests.is_empty(),
        "expected no request to '{path_suffix}', got {requests:?}"
    );
}

#[then(expr = "the stub guider should have received at least {int} {string} requests")]
fn stub_received_at_least(world: &mut RpWorld, count: usize, path_suffix: String) {
    let requests = stub(world).requests_to(&path_suffix);
    assert!(
        requests.len() >= count,
        "expected at least {count} requests to '{path_suffix}', got {}",
        requests.len()
    );
}

// --- Then steps: ladder result assertions ----------------------------

#[then(expr = "the rotator result ladder field {string} should be true")]
fn ladder_field_true(world: &mut RpWorld, field: String) {
    let ladder = &last_rotator_result(world)["guiding_ladder"];
    assert_eq!(
        ladder[&field].as_bool(),
        Some(true),
        "ladder field '{field}' in {ladder}"
    );
}

#[then(expr = "the rotator result ladder field {string} should be false")]
fn ladder_field_false(world: &mut RpWorld, field: String) {
    let ladder = &last_rotator_result(world)["guiding_ladder"];
    assert_eq!(
        ladder[&field].as_bool(),
        Some(false),
        "ladder field '{field}' in {ladder}"
    );
}

#[then(expr = "the rotator result ladder field {string} should be {float} within {float}")]
fn ladder_field_number(world: &mut RpWorld, field: String, expected: f64, tolerance: f64) {
    let ladder = &last_rotator_result(world)["guiding_ladder"];
    let actual = ladder[&field]
        .as_f64()
        .unwrap_or_else(|| panic!("expected numeric ladder field '{field}' in {ladder}"));
    assert!(
        (actual - expected).abs() <= tolerance,
        "ladder field '{field}': expected {expected} ± {tolerance}, got {actual} in {ladder}"
    );
}

#[then("the rotator result should have no guiding ladder")]
fn ladder_absent(world: &mut RpWorld) {
    let result = last_rotator_result(world);
    assert!(
        result["guiding_ladder"].is_null(),
        "expected guiding_ladder to be null in {result}"
    );
}
