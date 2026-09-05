//! BDD step definitions for `get_camera_info` MCP tool

use cucumber::{then, when};

use crate::steps::tool_steps::ensure_mcp_client;
use crate::world::RpWorld;

// --- When steps ---

#[when(expr = "the MCP client calls \"get_camera_info\" with camera {string}")]
async fn mcp_call_get_camera_info(world: &mut RpWorld, camera_id: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "get_camera_info",
            serde_json::json!({"camera_id": camera_id}),
        )
        .await;
    world.last_tool_result = Some(result);
}

#[when("the MCP client calls \"get_camera_info\" with no camera_id")]
async fn mcp_call_get_camera_info_no_id(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("get_camera_info", serde_json::json!({}))
        .await;
    world.last_tool_result = Some(result);
}

// --- Then steps ---

#[then(expr = "the tool result should contain {string} as a positive integer")]
fn result_contains_positive_integer(world: &mut RpWorld, field: String) {
    let result = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("tool call failed");

    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in tool result, got: {result:?}"));

    let num = value.as_u64().unwrap_or_else(|| {
        panic!("expected '{field}' to be a non-negative integer, got: {value:?}")
    });

    assert!(num > 0, "expected '{field}' to be positive, got: {num}");
}

#[then(expr = "the tool result should contain {string}")]
fn result_contains_field(world: &mut RpWorld, field: String) {
    let result = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("tool call failed");

    assert!(
        result.get(&field).is_some(),
        "expected '{field}' in tool result, got: {result:?}"
    );
}

/// Gain and offset are read live from the device and are `null` when
/// the driver does not expose them, so the contract is "present, and
/// an integer or null" — the simulator's own support decides which.
#[then(expr = "the tool result should contain {string} as an integer or null")]
fn result_contains_integer_or_null(world: &mut RpWorld, field: String) {
    let result = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("tool call failed");

    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in tool result, got: {result:?}"));

    assert!(
        value.is_null() || value.as_i64().is_some(),
        "expected '{field}' to be an integer or null, got: {value:?}"
    );
}
