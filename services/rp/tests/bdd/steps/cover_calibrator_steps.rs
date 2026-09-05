//! BDD step definitions for `CoverCalibrator` MCP tools

use cucumber::{given, then, when};

use bdd_infra::rp_harness::{
    CameraConfig, CoverCalibratorConfig, OmniSimHandle, OpticalTrainConfig,
};

use crate::steps::tool_steps::{add_camera, ensure_mcp_client, ensure_omnisim, start_rp};
use crate::world::RpWorld;

// --- Given steps ---

#[given("rp is running with a cover calibrator on the simulator")]
async fn rp_running_with_cover_calibrator(world: &mut RpWorld) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
    add_cover_calibrator(world);
    start_rp(world).await;
}

#[given(expr = "rp is running with a cover calibrator at {string} device {int}")]
async fn rp_running_with_cover_calibrator_at(world: &mut RpWorld, url: String, device_number: i32) {
    world.cover_calibrators.push(CoverCalibratorConfig {
        id: "flat-panel".to_string(),
        alpaca_url: url,
        device_number: device_number.cast_unsigned(),
        poll_interval: Some(std::time::Duration::from_millis(100)),
    });
    start_rp(world).await;
}

// --- Train addressing (calibrator-flats-provider plan, D4) ---

/// `main` = [flat-panel, main-cam]: the flip-flat first in the train.
#[given("rp is running with a camera and a cover calibrator on the simulator in an imaging train")]
async fn rp_with_camera_and_calibrator_in_train(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_cover_calibrator(world);
    world.optical_trains.push(imaging_train(
        "main",
        vec!["flat-panel".to_string(), "main-cam".to_string()],
    ));
    start_rp(world).await;
}

/// One flip-flat over the OTA, first in both `main` = [flat-panel,
/// main-cam] and `guide` = [flat-panel, guide-cam]. The guide camera's
/// URL is deliberately invalid so rp starts without a connect-retry
/// wait — membership is config, not device state.
#[given(
    expr = "rp is running with a cover calibrator on the simulator shared by the trains {string} and {string}"
)]
async fn rp_with_shared_calibrator(world: &mut RpWorld, first: String, second: String) {
    ensure_omnisim(world).await;
    add_camera(world);
    world.cameras.push(CameraConfig {
        id: "guide-cam".to_string(),
        alpaca_url: "not-a-url".to_string(),
        device_number: 0,
        cooler_targets_c: Vec::new(),
    });
    add_cover_calibrator(world);
    world.optical_trains.push(imaging_train(
        &first,
        vec!["flat-panel".to_string(), "main-cam".to_string()],
    ));
    world.optical_trains.push(imaging_train(
        &second,
        vec!["flat-panel".to_string(), "guide-cam".to_string()],
    ));
    start_rp(world).await;
}

/// `main` = [main-cam] with `flat-panel` in the roster but in no train.
#[given(
    "rp is running with a camera in an imaging train and a cover calibrator outside every train on the simulator"
)]
async fn rp_with_calibrator_outside_train(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_cover_calibrator(world);
    world
        .optical_trains
        .push(imaging_train("main", vec!["main-cam".to_string()]));
    start_rp(world).await;
}

// --- When steps ---

#[when(expr = "the MCP client calls \"close_cover\" with calibrator {string}")]
async fn mcp_call_close_cover(world: &mut RpWorld, calibrator_id: String) {
    call_calibrator_tool(world, "close_cover", &calibrator_id, None).await;
}

#[when(expr = "the MCP client calls \"open_cover\" with calibrator {string}")]
async fn mcp_call_open_cover(world: &mut RpWorld, calibrator_id: String) {
    call_calibrator_tool(world, "open_cover", &calibrator_id, None).await;
}

#[when(expr = "the MCP client calls \"calibrator_on\" with calibrator {string}")]
async fn mcp_call_calibrator_on(world: &mut RpWorld, calibrator_id: String) {
    call_calibrator_tool(world, "calibrator_on", &calibrator_id, None).await;
}

#[when(
    expr = "the MCP client calls \"calibrator_on\" with calibrator {string} and brightness {int}"
)]
async fn mcp_call_calibrator_on_brightness(
    world: &mut RpWorld,
    calibrator_id: String,
    brightness: i32,
) {
    call_calibrator_tool(
        world,
        "calibrator_on",
        &calibrator_id,
        Some(brightness.cast_unsigned()),
    )
    .await;
}

#[when(expr = "the MCP client calls \"calibrator_off\" with calibrator {string}")]
async fn mcp_call_calibrator_off(world: &mut RpWorld, calibrator_id: String) {
    call_calibrator_tool(world, "calibrator_off", &calibrator_id, None).await;
}

#[when("the MCP client calls \"close_cover\" with no calibrator_id")]
async fn mcp_call_close_cover_no_id(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("close_cover", serde_json::json!({}))
        .await;
    world.last_tool_result = Some(result);
}

/// Train-addressed call of a calibrator tool or `get_train_info` —
/// the tools that take `train_id` alone. The tool set is closed in the
/// regex so this never shadows the literal-tool steps other features
/// define (`get_next_target`, `refocus_train`).
#[when(
    regex = r#"^the MCP client calls "(get_cover_state|close_cover|open_cover|calibrator_on|calibrator_off|get_train_info)" with train "([^"]*)"$"#
)]
async fn mcp_call_tool_with_train(world: &mut RpWorld, tool_name: String, train_id: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(&tool_name, serde_json::json!({"train_id": train_id}))
        .await;
    world.last_tool_result = Some(result);
}

#[when(
    expr = "the MCP client calls \"close_cover\" with both calibrator {string} and train {string}"
)]
async fn mcp_call_close_cover_both(world: &mut RpWorld, calibrator_id: String, train_id: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "close_cover",
            serde_json::json!({"calibrator_id": calibrator_id, "train_id": train_id}),
        )
        .await;
    world.last_tool_result = Some(result);
}

// --- Then steps ---

#[then("the tool call should succeed")]
fn tool_call_succeeded(world: &mut RpWorld) {
    let result = world.last_tool_result.as_ref().expect("no tool result");

    assert!(
        result.is_ok(),
        "expected tool call to succeed, got error: {result:?}"
    );
}

#[then(expr = "the tool result {string} should be {string}")]
fn tool_result_field_is(world: &mut RpWorld, field: String, expected: String) {
    let result = tool_result_value(world);
    assert_eq!(
        result.get(&field).and_then(serde_json::Value::as_str),
        Some(expected.as_str()),
        "unexpected '{field}' in tool result: {result:?}"
    );
}

#[then(expr = "the tool result {string} should be null")]
fn tool_result_field_is_null(world: &mut RpWorld, field: String) {
    let result = tool_result_value(world);
    assert!(
        result.get(&field).is_some_and(serde_json::Value::is_null),
        "expected '{field}' present and null in tool result: {result:?}"
    );
}

/// `expected` is comma-separated; the empty string is the empty list.
#[then(expr = "the tool result list {string} should be exactly {string}")]
fn tool_result_list_is(world: &mut RpWorld, field: String, expected: String) {
    let result = tool_result_value(world);
    let expected: Vec<&str> = if expected.is_empty() {
        Vec::new()
    } else {
        expected.split(',').collect()
    };
    assert_eq!(
        result.get(&field),
        Some(&serde_json::json!(expected)),
        "unexpected '{field}' in tool result: {result:?}"
    );
}

// --- Helpers ---

pub fn add_cover_calibrator(world: &mut RpWorld) {
    if world.cover_calibrators.is_empty() {
        let url = world.omnisim_url();
        world.cover_calibrators.push(CoverCalibratorConfig {
            id: "flat-panel".to_string(),
            alpaca_url: url,
            device_number: 0,
            poll_interval: Some(std::time::Duration::from_millis(100)),
        });
    }
}

async fn call_calibrator_tool(
    world: &mut RpWorld,
    tool_name: &str,
    calibrator_id: &str,
    brightness: Option<u32>,
) {
    ensure_mcp_client(world).await;
    let mut args = serde_json::json!({"calibrator_id": calibrator_id});
    if let Some(b) = brightness {
        args["brightness"] = serde_json::json!(b);
    }
    let result = world.mcp().call_tool(tool_name, args).await;
    world.last_tool_result = Some(result);
}

fn tool_result_value(world: &RpWorld) -> &serde_json::Value {
    world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("tool call failed")
}

fn imaging_train(id: &str, devices: Vec<String>) -> OpticalTrainConfig {
    OpticalTrainConfig {
        id: id.to_string(),
        purpose: Some("imaging".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices,
        auto_focus: None,
    }
}
