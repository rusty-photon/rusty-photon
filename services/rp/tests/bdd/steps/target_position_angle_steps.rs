//! BDD step definitions for the effective position angle
//! (`target_position_angle.feature`, plan Decision 5 / P2).
//!
//! These scenarios boot rp the ordinary `OmniSim` way (`tool_steps`)
//! with a camera in an imaging train, since the second fallback layer
//! lives in `equipment.optical_trains[].default_position_angle_degrees`;
//! the target seeds reuse `target_store_crud_steps::add_target_fixture`
//! with the always-visible floor from `target_store_planner_steps.rs`.

use cucumber::{given, then, when};
use serde_json::Value;

use bdd_infra::rp_harness::OpticalTrainConfig;

use crate::steps::target_store_crud_steps::add_target_fixture;
use crate::steps::tool_steps::{add_camera, ensure_mcp_client, ensure_omnisim, start_rp};
use crate::world::RpWorld;

// ---------------------------------------------------------------------------
// Given
// ---------------------------------------------------------------------------

#[given("rp is running with a camera on the simulator in an imaging train")]
async fn rp_with_camera_in_plain_train(world: &mut RpWorld) {
    push_imaging_train(world, None);
    ensure_omnisim(world).await;
    add_camera(world);
    start_rp(world).await;
}

#[given(
    expr = "rp is running with a camera on the simulator in an imaging train with default position angle {float}"
)]
async fn rp_with_camera_in_train_with_default_angle(world: &mut RpWorld, angle: f64) {
    push_imaging_train(world, Some(angle));
    ensure_omnisim(world).await;
    add_camera(world);
    start_rp(world).await;
}

#[given(
    expr = "the MCP client has added the always-visible target {string} with position angle {float}"
)]
async fn added_always_visible_target_with_angle(
    world: &mut RpWorld,
    display_name: String,
    angle: f64,
) {
    add_target_fixture(
        world,
        serde_json::json!({
            "display_name": display_name,
            "ra_hours": 0.0,
            "dec_degrees": 0.0,
            "scheduling": { "min_altitude_degrees": -90.0 },
            "position_angle_degrees": angle
        }),
    )
    .await;
}

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

#[when(expr = "the MCP client calls \"get_next_target\" with train {string}")]
async fn call_next_target_with_train(world: &mut RpWorld, train_id: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "get_next_target",
            serde_json::json!({ "train_id": train_id }),
        )
        .await;
    world.last_tool_result = Some(result);
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

#[then(expr = "the result position_angle_degrees should be {float}")]
fn result_position_angle(world: &mut RpWorld, expected: f64) {
    let value = success_payload(world);
    let angle = value
        .get("position_angle_degrees")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("missing `position_angle_degrees` in: {value}"));
    assert!(
        (angle - expected).abs() < f64::EPSILON,
        "expected position_angle_degrees={expected}, got {angle}"
    );
}

#[then("the result position_angle_degrees should be null")]
fn result_position_angle_null(world: &mut RpWorld) {
    let value = success_payload(world);
    assert!(
        value
            .get("position_angle_degrees")
            .is_some_and(serde_json::Value::is_null),
        "expected position_angle_degrees=null, got: {value}"
    );
}

// --- Helpers ---

fn push_imaging_train(world: &mut RpWorld, default_position_angle_degrees: Option<f64>) {
    world.optical_trains.push(OpticalTrainConfig {
        aperture_mm: None,
        id: "main".to_string(),
        purpose: Some("imaging".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees,
        devices: vec!["main-cam".to_string()],
        auto_focus: None,
    });
}

fn success_payload(world: &RpWorld) -> &Value {
    world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("expected tool call to succeed")
}
