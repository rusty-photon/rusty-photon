//! BDD step definitions for the `refocus_train` compound tool
//! (`refocus_train.feature`) plus the train-addressed `auto_focus`
//! compositions shared with `auto_focus.feature`.
//!
//! Shared steps: tool listing / error assertions in `tool_steps.rs`,
//! webhook receiver in `event_steps.rs`, guider stub lifecycle in
//! `guider_steps.rs`, FITS/sidecar assertions in
//! `auto_focus_steps.rs`, offline-roster helpers in
//! `rotator_steps.rs`. The "standard `auto_focus` block" pins the same
//! sweep the `auto_focus` scenarios use per call: duration 100ms,
//! `step_size` 100, `half_width` 200, `min_area` 5, `max_area` 65536,
//! and `max_attempts` 1 — the simulator's starless frames fail every
//! fit, so a retry would only double each scenario's sweep; the retry
//! itself is pinned by the scenarios that ask for it.

use cucumber::{given, then, when};
use serde_json::{Map, Value};

use bdd_infra::rp_harness::{
    FocuserConfig, GuiderConfig, MountConfig, OpticalTrainConfig, TrainAutoFocusConfig,
};

use crate::steps::focuser_steps::add_focuser;
use crate::steps::rotator_steps::{add_offline_camera, push_train};
use crate::steps::tool_steps::{add_camera, ensure_mcp_client, ensure_omnisim, start_rp};
use crate::world::RpWorld;

/// The sweep parameters every "standard `auto_focus` block" Given pins.
fn standard_auto_focus_block() -> TrainAutoFocusConfig {
    TrainAutoFocusConfig {
        duration: Some("100ms".to_string()),
        step_size: 100,
        half_width: 200,
        min_area: Some(5),
        max_area: Some(65_536),
        binning: None,
        frames_per_step: None,
        max_attempts: Some(1),
    }
}

fn push_imaging_train_with_block(world: &mut RpWorld, id: &str, devices: Vec<String>) {
    push_imaging_train_with_attempts(world, id, devices, 1);
}

/// The standard block with its `max_attempts` set by the scenario.
fn push_imaging_train_with_attempts(
    world: &mut RpWorld,
    id: &str,
    devices: Vec<String>,
    max_attempts: i64,
) {
    world.optical_trains.push(OpticalTrainConfig {
        aperture_mm: None,
        id: id.to_string(),
        purpose: Some("imaging".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices,
        auto_focus: Some(TrainAutoFocusConfig {
            max_attempts: Some(max_attempts),
            ..standard_auto_focus_block()
        }),
    });
}

fn push_guiding_train(world: &mut RpWorld, id: &str, devices: Vec<String>) {
    world.optical_trains.push(OpticalTrainConfig {
        aperture_mm: None,
        id: id.to_string(),
        purpose: Some("guiding".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices,
        auto_focus: None,
    });
}

fn offline_mount(world: &mut RpWorld) {
    world.mount = Some(MountConfig {
        alpaca_url: "not-a-url".to_string(),
        device_number: 0,
        settle_after_slew: None,
    });
}

// --- Given steps ----------------------------------------------------

#[given(
    expr = "rp is running with a camera and a focuser on the simulator in train {string} with the standard auto_focus block"
)]
async fn rp_with_train_and_block(world: &mut RpWorld, train_id: String) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_focuser(world, None, None, None);
    push_imaging_train_with_block(
        world,
        &train_id,
        vec!["main-focuser".to_string(), "main-cam".to_string()],
    );
    start_rp(world).await;
}

#[given(
    expr = "rp is running with a camera and a focuser on the simulator in train {string} with the standard auto_focus block at binning {string}"
)]
async fn rp_with_train_and_block_binning(world: &mut RpWorld, train_id: String, binning: String) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_focuser(world, None, None, None);
    world.optical_trains.push(OpticalTrainConfig {
        aperture_mm: None,
        id: train_id,
        purpose: Some("imaging".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices: vec!["main-focuser".to_string(), "main-cam".to_string()],
        auto_focus: Some(TrainAutoFocusConfig {
            binning: Some(binning),
            ..standard_auto_focus_block()
        }),
    });
    start_rp(world).await;
}

#[given(
    expr = "rp is running with a camera and a focuser on the simulator in train {string} with the standard auto_focus block and max_attempts {int}"
)]
async fn rp_with_train_and_block_attempts(
    world: &mut RpWorld,
    train_id: String,
    max_attempts: i64,
) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_focuser(world, None, None, None);
    push_imaging_train_with_attempts(
        world,
        &train_id,
        vec!["main-focuser".to_string(), "main-cam".to_string()],
        max_attempts,
    );
    start_rp(world).await;
}

#[given(
    expr = "rp is running with a camera and a focuser on the simulator in train {string} with the standard auto_focus block and an offline guiding train sharing the focuser"
)]
async fn rp_with_train_block_and_guiding_train(world: &mut RpWorld, train_id: String) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_focuser(world, None, None, None);
    add_offline_camera(world, "guide-cam");
    push_imaging_train_with_block(
        world,
        &train_id,
        vec!["main-focuser".to_string(), "main-cam".to_string()],
    );
    push_guiding_train(
        world,
        "guide",
        vec!["main-focuser".to_string(), "guide-cam".to_string()],
    );
    start_rp(world).await;
}

/// A guider config pointing at a reliably unbound port, with no stub
/// behind it — the stats-unreachable handshake path.
#[given("an unreachable guider configuration")]
fn unreachable_guider_config(world: &mut RpWorld) {
    world.guider = Some(GuiderConfig::url_only("http://127.0.0.1:1".to_string()));
}

/// Fully offline main + guiding trains sharing `main-focuser`, with an
/// offline mount carrying the guiding block from the scenario's stub
/// guider Given. The first AF step fails at device resolution, which
/// is exactly what the failing-step scenario needs — no simulator
/// involved.
#[given("rp is running with offline main and guiding trains sharing the focuser")]
async fn rp_with_offline_shared_trains(world: &mut RpWorld) {
    world.focusers.push(FocuserConfig {
        microns_per_step: None,
        id: "main-focuser".to_string(),
        alpaca_url: "not-a-url".to_string(),
        device_number: 0,
        min_position: None,
        max_position: None,
        backlash: None,
    });
    add_offline_camera(world, "main-cam");
    add_offline_camera(world, "guide-cam");
    offline_mount(world);
    push_imaging_train_with_block(
        world,
        "main",
        vec!["main-focuser".to_string(), "main-cam".to_string()],
    );
    push_guiding_train(
        world,
        "guide",
        vec!["main-focuser".to_string(), "guide-cam".to_string()],
    );
    start_rp(world).await;
}

/// One offline focuser train with no `auto_focus` block — the
/// missing-block and train-addressing error scenarios.
#[given("rp is running with an offline focuser train without an auto_focus block")]
async fn rp_with_blockless_offline_train(world: &mut RpWorld) {
    world.focusers.push(FocuserConfig {
        microns_per_step: None,
        id: "main-focuser".to_string(),
        alpaca_url: "not-a-url".to_string(),
        device_number: 0,
        min_position: None,
        max_position: None,
        backlash: None,
    });
    add_offline_camera(world, "main-cam");
    push_train(
        world,
        "main",
        vec!["main-focuser".to_string(), "main-cam".to_string()],
    );
    start_rp(world).await;
}

// --- When steps -----------------------------------------------------

#[when(expr = "the MCP client calls \"refocus_train\" with train {string}")]
async fn call_refocus_train(world: &mut RpWorld, train_id: String) {
    let mut args = Map::new();
    args.insert("train_id".into(), Value::String(train_id));
    dispatch_refocus(world, args).await;
}

#[when(expr = "the MCP client calls \"refocus_train\" with train {string} and reason {string}")]
async fn call_refocus_train_with_reason(world: &mut RpWorld, train_id: String, reason: String) {
    let mut args = Map::new();
    args.insert("train_id".into(), Value::String(train_id));
    args.insert("reason".into(), Value::String(reason));
    dispatch_refocus(world, args).await;
}

// --- Then steps -----------------------------------------------------
//
// There are no success-payload Then steps here: the simulator's flat
// HFR makes a real sweep's fit outcome non-deterministic (see the
// feature description), so the result shape is pinned by the unit
// tests over the V-curve fixture registry instead.

#[then("the stub guider should have received a pause request with full false")]
async fn stub_pause_request_full_false(world: &mut RpWorld) {
    let stub = world
        .guider_stub
        .as_ref()
        .expect("no guider stub registered for this scenario");
    let mut pauses = stub.requests_to("/guiding/pause").await;
    let request = pauses.pop().expect("stub guider received no pause request");
    assert_eq!(
        request.get("full").and_then(Value::as_bool),
        Some(false),
        "full mismatch in {request}"
    );
}

#[then("the stub guider should not have received a pause request")]
async fn stub_no_pause_request(world: &mut RpWorld) {
    let stub = world
        .guider_stub
        .as_ref()
        .expect("no guider stub registered for this scenario");
    let pauses = stub.requests_to("/guiding/pause").await;
    assert!(
        pauses.is_empty(),
        "expected no pause request, got {pauses:?}"
    );
}

// --- Helpers --------------------------------------------------------

async fn dispatch_refocus(world: &mut RpWorld, args: Map<String, Value>) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("refocus_train", Value::Object(args))
        .await;
    world.last_tool_result = Some(result);
}
