//! BDD step definitions for the T4 guiding integration:
//! `guide_focus.feature` (the guide-train PHD2-metric sweep, the
//! guide focus watch, the `start_guiding` rotator warning) plus the
//! rotate-while-guiding ladder scenarios in `rotator.feature`.
//!
//! Shared steps: tool listing / error assertions in `tool_steps.rs`,
//! guider stub lifecycle in `guider_steps.rs`, webhook receiver in
//! `event_steps.rs`, offline-roster helpers in `rotator_steps.rs`.
//! The stub guider's `metrics_hfd_script` (`guider_stub.rs`) drives
//! the deterministic outcomes here: a one-value script is perfectly
//! flat HFD (the sweep's fit then reports no minimum), a two-value
//! script is the degradation the focus watch turns into events.

use cucumber::{given, then, when};
use serde_json::{Map, Value};

use bdd_infra::rp_harness::{CannedGuiding, GuiderConfig, GuiderStub, GuiderStubBehavior};

use crate::steps::focuser_steps::add_focuser;
use crate::steps::rotator_steps::add_offline_camera;
use crate::steps::tool_steps::{ensure_mcp_client, ensure_omnisim, start_rp};
use crate::world::RpWorld;

// --- Given steps: stub guider variants -------------------------------

async fn start_stub(world: &mut RpWorld, canned: CannedGuiding) {
    let stub = GuiderStub::start(GuiderStubBehavior::Canned(canned)).await;
    world.guider = Some(GuiderConfig::url_only(stub.url.clone()));
    world.guider_stub = Some(stub);
}

#[given("a stub guider with a connected PHD2 rotator")]
async fn stub_with_phd2_rotator(world: &mut RpWorld) {
    start_stub(
        world,
        CannedGuiding {
            phd2_rotator_connected: true,
            ..CannedGuiding::default()
        },
    )
    .await;
}

#[given(expr = "a stub guider with the HFD script {string}")]
async fn stub_with_hfd_script(world: &mut RpWorld, script: String) {
    let values: Vec<f64> = script
        .split(',')
        .map(|s| s.trim().parse().expect("HFD script values must be numbers"))
        .collect();
    start_stub(
        world,
        CannedGuiding {
            metrics_hfd_script: values,
            ..CannedGuiding::default()
        },
    )
    .await;
}

// --- Given steps: equipment compositions ----------------------------

/// A simulator rotator inside a guiding train (the ladder and
/// rotator-warning scenarios). Requires a prior stub-guider Given —
/// a guiding train is rejected at load without `mount.guiding`.
#[given(expr = "rp is running with a rotator on the simulator inside guiding train {string}")]
async fn rp_with_sim_rotator_in_guiding_train(world: &mut RpWorld, train_id: String) {
    ensure_omnisim(world).await;
    let url = world.omnisim_url();
    world.rotators.push(bdd_infra::rp_harness::RotatorConfig {
        id: "main-rotator".to_string(),
        alpaca_url: url,
        device_number: 0,
    });
    add_offline_camera(world, "guide-cam");
    world
        .optical_trains
        .push(bdd_infra::rp_harness::OpticalTrainConfig {
            aperture_mm: None,
            id: train_id,
            purpose: Some("guiding".to_string()),
            focal_length_mm: None,
            default_position_angle_degrees: None,
            devices: vec!["main-rotator".to_string(), "guide-cam".to_string()],
            auto_focus: None,
        });
    start_rp(world).await;
}

fn push_guiding_focuser_train(world: &mut RpWorld, train_id: String, with_block: bool) {
    let auto_focus = with_block.then_some(bdd_infra::rp_harness::TrainAutoFocusConfig {
        duration: None,
        step_size: 50,
        half_width: 100,
        min_area: None,
        max_area: None,
        frames_per_step: Some(2),
        max_attempts: None,
    });
    world
        .optical_trains
        .push(bdd_infra::rp_harness::OpticalTrainConfig {
            aperture_mm: None,
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

#[when(expr = "the MCP client calls auto_focus with train {string} and duration {string}")]
async fn call_auto_focus_train_duration(world: &mut RpWorld, train_id: String, duration: String) {
    ensure_mcp_client(world).await;
    let mut args = Map::new();
    args.insert("train_id".into(), Value::String(train_id));
    args.insert("duration".into(), Value::String(duration));
    let result = world
        .mcp()
        .call_tool("auto_focus", Value::Object(args))
        .await;
    world.last_tool_result = Some(result);
}

// --- Then steps: stub guider request assertions ----------------------

const fn stub(world: &RpWorld) -> &bdd_infra::rp_harness::GuiderStub {
    world
        .guider_stub
        .as_ref()
        .expect("no guider stub registered for this scenario")
}

#[then(expr = "the stub guider should have received a {string} request")]
async fn stub_received_request(world: &mut RpWorld, path_suffix: String) {
    let requests = stub(world).requests_to(&path_suffix).await;
    assert!(
        !requests.is_empty(),
        "expected a request to '{path_suffix}', got none; all: {:?}",
        stub(world).requests().await
    );
}

#[then(expr = "the stub guider should not have received a {string} request")]
async fn stub_not_received_request(world: &mut RpWorld, path_suffix: String) {
    let requests = stub(world).requests_to(&path_suffix).await;
    assert!(
        requests.is_empty(),
        "expected no request to '{path_suffix}', got {requests:?}"
    );
}

#[then(expr = "the stub guider should have received at least {int} {string} requests")]
async fn stub_received_at_least(world: &mut RpWorld, count: usize, path_suffix: String) {
    let requests = stub(world).requests_to(&path_suffix).await;
    assert!(
        requests.len() >= count,
        "expected at least {count} requests to '{path_suffix}', got {}",
        requests.len()
    );
}

// --- Then steps: ladder result assertions ----------------------------

const fn last_rotator_result(world: &RpWorld) -> &Value {
    world
        .last_rotator_result
        .as_ref()
        .expect("no successful rotator tool result recorded — When step missing or tool errored?")
}

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

#[then("the rotator result should have no guiding ladder")]
fn ladder_absent(world: &mut RpWorld) {
    let result = last_rotator_result(world);
    assert!(
        result["guiding_ladder"].is_null(),
        "expected guiding_ladder to be null in {result}"
    );
}
