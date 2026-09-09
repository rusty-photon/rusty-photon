//! BDD step definitions for Focuser MCP tools

use cucumber::{given, then, when};

use bdd_infra::rp_harness::{BacklashConfig, FocuserConfig, OmniSimHandle};

use crate::steps::tool_steps::{ensure_mcp_client, start_rp};
use crate::world::RpWorld;

// --- Given steps ---

#[given("rp is running with a focuser on the simulator")]
async fn rp_running_with_focuser(world: &mut RpWorld) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
    add_focuser(world, None, None, None);
    start_rp(world).await;
}

#[given(expr = "rp is running with a focuser on the simulator with bounds {int}..{int}")]
async fn rp_running_with_focuser_bounded(world: &mut RpWorld, min: i32, max: i32) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
    add_focuser(world, Some(min), Some(max), None);
    start_rp(world).await;
}

#[given(
    expr = "rp is running with a focuser on the simulator with backlash approach {string} and {int} steps"
)]
async fn rp_running_with_focuser_backlash(world: &mut RpWorld, approach: String, steps: i32) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
    add_focuser(world, None, None, Some(backlash_block(approach, steps)));
    start_rp(world).await;
}

#[given(
    expr = "rp is running with a focuser on the simulator with bounds {int}..{int} and backlash approach {string} and {int} steps"
)]
async fn rp_running_with_focuser_bounded_backlash(
    world: &mut RpWorld,
    min: i32,
    max: i32,
    approach: String,
    steps: i32,
) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
    add_focuser(
        world,
        Some(min),
        Some(max),
        Some(backlash_block(approach, steps)),
    );
    start_rp(world).await;
}

#[given(expr = "rp is running with a focuser at {string} device {int}")]
async fn rp_running_with_focuser_at(world: &mut RpWorld, url: String, device_number: i32) {
    let device_number = u32::try_from(device_number)
        .expect("device_number in focuser scenarios must be non-negative");
    world.focusers.push(FocuserConfig {
        id: "main-focuser".to_string(),
        alpaca_url: url,
        device_number,
        min_position: None,
        max_position: None,
        backlash: None,
    });
    start_rp(world).await;
}

// --- When steps ---

#[when(expr = "the MCP client calls \"move_focuser\" with focuser {string} to position {int}")]
async fn mcp_call_move_focuser(world: &mut RpWorld, focuser_id: String, position: i32) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "move_focuser",
            serde_json::json!({
                "focuser_id": focuser_id,
                "position": position,
            }),
        )
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"get_focuser_position\" with focuser {string}")]
async fn mcp_call_get_focuser_position(world: &mut RpWorld, focuser_id: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "get_focuser_position",
            serde_json::json!({"focuser_id": focuser_id}),
        )
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"get_focuser_temperature\" with focuser {string}")]
async fn mcp_call_get_focuser_temperature(world: &mut RpWorld, focuser_id: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "get_focuser_temperature",
            serde_json::json!({"focuser_id": focuser_id}),
        )
        .await;
    world.last_tool_result = Some(result);
}

#[when("the MCP client calls \"move_focuser\" with no focuser_id")]
async fn mcp_call_move_focuser_no_id(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("move_focuser", serde_json::json!({"position": 1000}))
        .await;
    world.last_tool_result = Some(result);
}

// --- Then steps ---

#[then(expr = "the move_focuser result actual_position should be {int}")]
fn move_focuser_actual_position(world: &mut RpWorld, expected: i32) {
    let result = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("tool call failed");
    let actual = result
        .get("actual_position")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_else(|| panic!("expected actual_position field, got: {result:?}"));
    assert_eq!(
        actual,
        i64::from(expected),
        "expected actual_position {expected}, got {actual}"
    );
}

#[then(expr = "the move_focuser result backlash_compensated should be {word}")]
fn move_focuser_backlash_compensated(world: &mut RpWorld, expected: String) {
    let expected: bool = expected
        .parse()
        .unwrap_or_else(|_| panic!("expected true or false, got {expected}"));
    let result = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("tool call failed");
    let actual = result
        .get("backlash_compensated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| panic!("expected backlash_compensated field, got: {result:?}"));
    assert_eq!(
        actual, expected,
        "expected backlash_compensated {expected}, got {actual}"
    );
}

#[then(expr = "the get_focuser_position result position should be {int}")]
fn get_focuser_position_value(world: &mut RpWorld, expected: i32) {
    let result = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("tool call failed");
    let actual = result
        .get("position")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_else(|| panic!("expected position field, got: {result:?}"));
    assert_eq!(
        actual,
        i64::from(expected),
        "expected position {expected}, got {actual}"
    );
}

#[then(expr = "the get_focuser_temperature result should contain a {string} field")]
fn get_focuser_temperature_field(world: &mut RpWorld, field: String) {
    let result = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("tool call failed");
    assert!(
        result.get(&field).is_some(),
        "expected '{field}' field in result, got: {result:?}"
    );
}

// --- Helpers ---

pub(super) fn add_focuser(
    world: &mut RpWorld,
    min_position: Option<i32>,
    max_position: Option<i32>,
    backlash: Option<BacklashConfig>,
) {
    if world.focusers.is_empty() {
        let url = world.omnisim_url();
        world.focusers.push(FocuserConfig {
            id: "main-focuser".to_string(),
            alpaca_url: url,
            device_number: 0,
            min_position,
            max_position,
            backlash,
        });
    }
}

/// The `focusers[].backlash` block as a scenario spells it. `steps`
/// arrives as the `{int}` capture; the scenarios only use positive
/// values, so a negative one is a feature-file mistake.
fn backlash_block(approach: String, steps: i32) -> BacklashConfig {
    let steps = u32::try_from(steps).expect("backlash steps in focuser scenarios must be positive");
    BacklashConfig { approach, steps }
}
