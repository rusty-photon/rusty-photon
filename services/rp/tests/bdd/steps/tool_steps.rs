//! BDD step definitions for MCP tool execution feature

use cucumber::{given, then, when};

use bdd_infra::rp_harness::{
    CameraConfig, FilterWheelConfig, McpTestClient, MountConfig, OmniSimHandle,
};
use bdd_infra::ServiceHandle;

use crate::world::RpWorld;

// --- Given steps ---

#[given("rp is running with a camera on the simulator")]
async fn rp_running_with_camera(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    start_rp(world).await;
}

#[given("rp is running with a filter wheel on the simulator")]
async fn rp_running_with_filter_wheel(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_filter_wheel(world);
    start_rp(world).await;
}

#[given("rp is running with a camera and filter wheel on the simulator")]
async fn rp_running_with_camera_and_filter_wheel(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_filter_wheel(world);
    start_rp(world).await;
}

// Also a `when`: the startup-recovery scenarios reconnect a fresh MCP
// client to the restarted rp mid-scenario.
#[given("an MCP client connected to rp")]
#[when("an MCP client connected to rp")]
async fn mcp_client_connected(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
}

// --- When steps ---

#[when(expr = "the MCP client calls \"capture\" with camera {string} for {int} ms")]
async fn mcp_call_capture(world: &mut RpWorld, camera_id: String, duration_ms: i32) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "capture",
            serde_json::json!({
                "camera_id": camera_id,
                "duration": format!("{}ms", duration_ms),
            }),
        )
        .await;

    if let Ok(ref v) = result {
        world.last_image_path = v
            .get("image_path")
            .and_then(|v| v.as_str())
            .map(String::from);
        world.last_document_id = v
            .get("document_id")
            .and_then(|v| v.as_str())
            .map(String::from);
    }
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"set_filter\" with filter wheel {string} and filter {string}")]
async fn mcp_call_set_filter(world: &mut RpWorld, fw_id: String, filter_name: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "set_filter",
            serde_json::json!({
                "filter_wheel_id": fw_id,
                "filter_name": filter_name
            }),
        )
        .await;

    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"get_filter\" with filter wheel {string}")]
async fn mcp_call_get_filter(world: &mut RpWorld, fw_id: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "get_filter",
            serde_json::json!({
                "filter_wheel_id": fw_id
            }),
        )
        .await;

    if let Ok(ref v) = result {
        world.current_filter = v
            .get("filter_name")
            .and_then(|v| v.as_str())
            .map(String::from);
    }
    world.last_tool_result = Some(result);
}

#[when("the MCP client lists available tools")]
async fn mcp_list_tools(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    match world.mcp().list_tools().await {
        Ok(tools) => world.last_tool_list = Some(tools),
        Err(_) => world.last_tool_list = Some(vec![]),
    }
}

#[given(expr = "rp is running with a camera at {string} device {int}")]
async fn rp_running_with_camera_at(world: &mut RpWorld, url: String, device_number: i32) {
    world.cameras.push(CameraConfig {
        id: "main-cam".to_string(),
        alpaca_url: url,
        device_number: device_number.cast_unsigned(),
        cooler_targets_c: Vec::new(),
    });
    start_rp(world).await;
}

#[given(expr = "rp is running with a filter wheel at {string} device {int}")]
async fn rp_running_with_fw_at(world: &mut RpWorld, url: String, device_number: i32) {
    world.filter_wheels.push(FilterWheelConfig {
        wavelengths_nm: vec![],
        id: "main-fw".to_string(),
        alpaca_url: url,
        device_number: device_number.cast_unsigned(),
        filters: vec![
            "Luminance".to_string(),
            "Red".to_string(),
            "Green".to_string(),
            "Blue".to_string(),
        ],
    });
    start_rp(world).await;
}

#[when("the MCP client calls \"capture\" with no camera_id")]
async fn mcp_call_capture_no_camera_id(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("capture", serde_json::json!({"duration": "1s"}))
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"capture\" with camera {string} but no duration")]
async fn mcp_call_capture_no_duration(world: &mut RpWorld, camera_id: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("capture", serde_json::json!({"camera_id": camera_id}))
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls an unknown method {string}")]
async fn mcp_call_unknown_method(world: &mut RpWorld, _method: String) {
    // rmcp handles unknown methods at the protocol level.
    // We simulate by calling a nonexistent tool, since the client
    // API doesn't expose raw method calls.
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("__unknown_method__", serde_json::json!({}))
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls tool {string}")]
async fn mcp_call_unknown_tool(world: &mut RpWorld, tool_name: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(&tool_name, serde_json::json!({}))
        .await;
    world.last_tool_result = Some(result);
}

// --- Then steps ---

#[then("the tool result should contain an image path")]
fn result_has_image_path(world: &mut RpWorld) {
    let result = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("tool call failed");

    assert!(
        result.get("image_path").and_then(|v| v.as_str()).is_some(),
        "expected image_path in tool result, got: {result:?}"
    );
}

#[then("the tool result should contain a document id")]
fn result_has_document_id(world: &mut RpWorld) {
    let result = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("tool call failed");

    assert!(
        result.get("document_id").and_then(|v| v.as_str()).is_some(),
        "expected document_id in tool result, got: {result:?}"
    );
}

#[then(expr = "the current filter should be {string}")]
fn current_filter_is(world: &mut RpWorld, expected: String) {
    assert_eq!(
        world.current_filter.as_deref(),
        Some(expected.as_str()),
        "expected filter '{}' but got '{:?}'",
        expected,
        world.current_filter
    );
}

#[then("the tool call should return an error")]
fn tool_call_returned_error(world: &mut RpWorld) {
    let result = world.last_tool_result.as_ref().expect("no tool result");

    assert!(
        result.is_err(),
        "expected tool call to return an error, got: {result:?}"
    );
}

#[then(expr = "the error message should contain {string}")]
fn error_message_contains(world: &mut RpWorld, expected: String) {
    let result = world.last_tool_result.as_ref().expect("no tool result");
    let err_msg = result
        .as_ref()
        .expect_err("expected an error but got success");

    assert!(
        err_msg.contains(&expected),
        "expected error message to contain '{expected}', got: '{err_msg}'"
    );
}

#[then(expr = "the tool list should include {string}")]
fn tool_list_includes(world: &mut RpWorld, tool_name: String) {
    let tools = world.last_tool_list.as_ref().expect("no tool list");

    assert!(
        tools.contains(&tool_name),
        "expected tool '{tool_name}' in catalog, got: {tools:?}"
    );
}

// --- Helpers (pub for reuse in other step files) ---

pub async fn ensure_mcp_client(world: &mut RpWorld) {
    if world.mcp_client.is_none() {
        let url = world.rp_mcp_url();
        world.mcp_client = Some(
            McpTestClient::connect(&url)
                .await
                .expect("failed to connect MCP test client"),
        );
    }
}

pub async fn ensure_omnisim(world: &mut RpWorld) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
}

pub fn add_camera(world: &mut RpWorld) {
    if world.cameras.is_empty() {
        let url = world.omnisim_url();
        world.cameras.push(CameraConfig {
            id: "main-cam".to_string(),
            alpaca_url: url,
            device_number: 0,
            cooler_targets_c: Vec::new(),
        });
    }
}

pub fn add_filter_wheel(world: &mut RpWorld) {
    if world.filter_wheels.is_empty() {
        let url = world.omnisim_url();
        world.filter_wheels.push(FilterWheelConfig {
            id: "main-fw".to_string(),
            alpaca_url: url,
            device_number: 0,
            filters: vec![
                "Luminance".to_string(),
                "Red".to_string(),
                "Green".to_string(),
                "Blue".to_string(),
            ],
            wavelengths_nm: world.filter_wavelengths_nm.clone(),
        });
    }
}

pub async fn start_rp(world: &mut RpWorld) {
    if world
        .rp
        .as_ref()
        .is_some_and(bdd_infra::ServiceHandle::is_running)
    {
        return;
    }

    // Guiding is mount-scoped (`equipment.mount.guiding`), so a scenario
    // that configured a guider stub implies a mount; point it at the
    // simulator when the scenario didn't configure one itself.
    if world.guider.is_some() && world.mount.is_none() {
        world.mount = Some(MountConfig {
            alpaca_url: world.omnisim_url(),
            device_number: 0,
            settle_after_slew: None,
        });
    }

    let config = world.build_config();
    let config_path = std::env::temp_dir()
        .join(format!(
            "rp-test-config-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
        .to_string_lossy()
        .to_string();
    tokio::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap())
        .await
        .expect("failed to write config");

    world.rp = Some(ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await);

    assert!(
        world.wait_for_rp_healthy().await,
        "rp did not become healthy within timeout"
    );
}
