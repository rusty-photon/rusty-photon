//! BDD step definitions for the rotator MCP tools (`move_rotator`,
//! `get_rotator_position`) — `rotator.feature`.
//!
//! Shared steps live in `tool_steps.rs` (tool listing, error
//! assertions, MCP client) and `event_steps.rs` (webhook receiver).
//! The offline compositions use the invalid `not-a-url` Alpaca URL so
//! rp starts instantly with a disconnected roster entry instead of
//! paying the connect-retry backoff (see `optical_trains_steps.rs`).

use cucumber::{given, then, when};
use serde_json::{Map, Value};

use bdd_infra::rp_harness::{CameraConfig, OpticalTrainConfig, RotatorConfig};

use crate::steps::tool_steps::{ensure_mcp_client, ensure_omnisim, start_rp};
use crate::world::RpWorld;

// --- Given steps: equipment compositions ----------------------------

#[given("rp is running with a rotator on the simulator")]
async fn rp_with_rotator(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_sim_rotator(world);
    start_rp(world).await;
}

#[given(expr = "rp is running with a rotator on the simulator inside train {string}")]
async fn rp_with_rotator_in_train(world: &mut RpWorld, train_id: String) {
    ensure_omnisim(world).await;
    add_sim_rotator(world);
    add_offline_camera(world, "main-cam");
    push_train(
        world,
        &train_id,
        vec!["main-rotator".to_string(), "main-cam".to_string()],
    );
    start_rp(world).await;
}

#[given(expr = "rp is running with an offline rotator inside train {string}")]
async fn rp_with_offline_rotator_in_train(world: &mut RpWorld, train_id: String) {
    add_offline_rotator(world, "main-rotator");
    add_offline_camera(world, "main-cam");
    push_train(
        world,
        &train_id,
        vec!["main-rotator".to_string(), "main-cam".to_string()],
    );
    start_rp(world).await;
}

#[given(expr = "rp is running with two offline rotators inside train {string}")]
async fn rp_with_two_offline_rotators_in_train(world: &mut RpWorld, train_id: String) {
    add_offline_rotator(world, "main-rotator");
    add_offline_rotator(world, "second-rotator");
    add_offline_camera(world, "main-cam");
    push_train(
        world,
        &train_id,
        vec![
            "main-rotator".to_string(),
            "second-rotator".to_string(),
            "main-cam".to_string(),
        ],
    );
    start_rp(world).await;
}

/// A roster with one offline camera in a camera-only train "main" —
/// the train-without-a-rotator / train-without-a-focuser composition
/// shared with `auto_focus.feature` and `refocus_train.feature`.
#[given("rp is running with an offline camera-only train")]
async fn rp_with_camera_only_train(world: &mut RpWorld) {
    add_offline_camera(world, "main-cam");
    push_train(world, "main", vec!["main-cam".to_string()]);
    start_rp(world).await;
}

// --- When steps -----------------------------------------------------

#[when(expr = "the MCP client calls \"move_rotator\" with rotator {string} to angle {float}")]
async fn call_move_rotator_by_id(world: &mut RpWorld, rotator_id: String, angle: f64) {
    let mut args = Map::new();
    args.insert("rotator_id".into(), Value::String(rotator_id));
    args.insert("angle".into(), Value::from(angle));
    call_rotator_tool(world, "move_rotator", args).await;
}

#[when(expr = "the MCP client calls \"move_rotator\" with train {string} to angle {float}")]
async fn call_move_rotator_by_train(world: &mut RpWorld, train_id: String, angle: f64) {
    let mut args = Map::new();
    args.insert("train_id".into(), Value::String(train_id));
    args.insert("angle".into(), Value::from(angle));
    call_rotator_tool(world, "move_rotator", args).await;
}

#[when(
    expr = "the MCP client calls \"move_rotator\" with rotator {string} and train {string} to angle {float}"
)]
async fn call_move_rotator_both(
    world: &mut RpWorld,
    rotator_id: String,
    train_id: String,
    angle: f64,
) {
    let mut args = Map::new();
    args.insert("rotator_id".into(), Value::String(rotator_id));
    args.insert("train_id".into(), Value::String(train_id));
    args.insert("angle".into(), Value::from(angle));
    call_rotator_tool(world, "move_rotator", args).await;
}

#[when(expr = "the MCP client calls \"move_rotator\" with no addressing to angle {float}")]
async fn call_move_rotator_unaddressed(world: &mut RpWorld, angle: f64) {
    let mut args = Map::new();
    args.insert("angle".into(), Value::from(angle));
    call_rotator_tool(world, "move_rotator", args).await;
}

#[when(expr = "the MCP client calls \"get_rotator_position\" with rotator {string}")]
async fn call_get_rotator_position_by_id(world: &mut RpWorld, rotator_id: String) {
    let mut args = Map::new();
    args.insert("rotator_id".into(), Value::String(rotator_id));
    call_rotator_tool(world, "get_rotator_position", args).await;
}

#[when(expr = "the MCP client calls \"get_rotator_position\" with train {string}")]
async fn call_get_rotator_position_by_train(world: &mut RpWorld, train_id: String) {
    let mut args = Map::new();
    args.insert("train_id".into(), Value::String(train_id));
    call_rotator_tool(world, "get_rotator_position", args).await;
}

// --- Then steps -----------------------------------------------------

#[then(expr = "the rotator result field {string} should be {string}")]
fn rotator_result_string_field(world: &mut RpWorld, field: String, expected: String) {
    let result = last_rotator_result(world);
    assert_eq!(
        result.get(&field).and_then(Value::as_str),
        Some(expected.as_str()),
        "field '{field}' mismatch in {result}"
    );
}

#[then(expr = "the rotator result should carry a numeric {string}")]
fn rotator_result_numeric_field(world: &mut RpWorld, field: String) {
    let result = last_rotator_result(world);
    assert!(
        result.get(&field).and_then(Value::as_f64).is_some(),
        "expected numeric field '{field}' in {result}"
    );
}

#[then(expr = "the rotator result field {string} should be false")]
fn rotator_result_false_field(world: &mut RpWorld, field: String) {
    let result = last_rotator_result(world);
    assert_eq!(
        result.get(&field).and_then(Value::as_bool),
        Some(false),
        "field '{field}' mismatch in {result}"
    );
}

#[then(expr = "the rotator result field {string} should be {float} within {float}")]
fn rotator_result_float_field(world: &mut RpWorld, field: String, expected: f64, tolerance: f64) {
    let result = last_rotator_result(world);
    let actual = result
        .get(&field)
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("expected numeric field '{field}' in {result}"));
    assert!(
        (actual - expected).abs() <= tolerance,
        "field '{field}': expected {expected} ± {tolerance}, got {actual}"
    );
}

#[then("the rotator result should list no moved trains")]
fn rotator_result_no_moved_trains(world: &mut RpWorld) {
    let trains = moved_trains(world);
    assert!(
        trains.is_empty(),
        "expected no moved trains, got {trains:?}"
    );
}

#[then(expr = "the rotator result should list moved train {string}")]
fn rotator_result_moved_train(world: &mut RpWorld, train_id: String) {
    let trains = moved_trains(world);
    assert!(
        trains.iter().any(|t| t == &train_id),
        "expected moved train {train_id:?} in {trains:?}"
    );
}

// --- Helpers --------------------------------------------------------

fn add_sim_rotator(world: &mut RpWorld) {
    let url = world.omnisim_url();
    world.rotators.push(RotatorConfig {
        id: "main-rotator".to_string(),
        alpaca_url: url,
        device_number: 0,
    });
}

fn add_offline_rotator(world: &mut RpWorld, id: &str) {
    world.rotators.push(RotatorConfig {
        id: id.to_string(),
        alpaca_url: "not-a-url".to_string(),
        device_number: 0,
    });
}

pub fn add_offline_camera(world: &mut RpWorld, id: &str) {
    world.cameras.push(CameraConfig {
        id: id.to_string(),
        alpaca_url: "not-a-url".to_string(),
        device_number: 0,
        cooler_targets_c: Vec::new(),
    });
}

pub fn push_train(world: &mut RpWorld, id: &str, devices: Vec<String>) {
    world.optical_trains.push(OpticalTrainConfig {
        aperture_mm: None,
        id: id.to_string(),
        purpose: None,
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices,
        auto_focus: None,
    });
}

async fn call_rotator_tool(world: &mut RpWorld, tool: &str, args: Map<String, Value>) {
    ensure_mcp_client(world).await;
    let result = world.mcp().call_tool(tool, Value::Object(args)).await;
    match &result {
        Ok(v) => world.last_rotator_result = Some(v.clone()),
        Err(_) => world.last_rotator_result = None,
    }
    world.last_tool_result = Some(result);
}

const fn last_rotator_result(world: &RpWorld) -> &Value {
    world
        .last_rotator_result
        .as_ref()
        .expect("no successful rotator tool result recorded — When step missing or tool errored?")
}

fn moved_trains(world: &RpWorld) -> Vec<String> {
    last_rotator_result(world)
        .get("moved_trains")
        .and_then(Value::as_array)
        .unwrap_or_else(|| {
            panic!(
                "expected a moved_trains array in {}",
                last_rotator_result(world)
            )
        })
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}
