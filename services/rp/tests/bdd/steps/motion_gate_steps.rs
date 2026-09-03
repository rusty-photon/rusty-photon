//! BDD step definitions for the mount motion gate
//! (`motion_gate.feature`).
//!
//! The gate serializes concurrent work, so these steps drive rp from
//! two MCP sessions at once: the scenario's persistent client makes
//! the foreground calls, and the "a second MCP client starts ... in
//! the background" steps spawn a task that connects its own session
//! and calls the tool there. The handles land in
//! `world.background_calls`; every scenario joins them via "the
//! background {tool} call should succeed" so a stray capture cannot
//! hold the shared simulator into the next scenario. Event waits and
//! `event_seq`-based ordering assertions live in `event_steps.rs`;
//! the stub guider's `settle_delay` (`guider_stub.rs`) is what holds a
//! dither in flight long enough for a concurrent capture to contend.

use std::time::Duration;

use cucumber::{given, then, when};
use serde_json::{json, Value};

use bdd_infra::rp_harness::{
    CannedGuiding, GuiderConfig, GuiderStub, GuiderStubBehavior, McpTestClient, MountConfig,
    OpticalTrainConfig,
};

use crate::steps::rotator_steps::push_train;
use crate::steps::tool_steps::{add_camera, ensure_omnisim, start_rp};
use crate::world::RpWorld;

// --- Given steps: equipment compositions ----------------------------

async fn add_stub_guider(world: &mut RpWorld, settle_delay: Duration) {
    let stub = GuiderStub::start(GuiderStubBehavior::Canned(CannedGuiding {
        settle_delay,
        ..CannedGuiding::default()
    }))
    .await;
    world.guider = Some(GuiderConfig::url_only(stub.url.clone()));
    world.guider_stub = Some(stub);
}

#[given(
    expr = "rp is running with a camera on the simulator in imaging train {string} and a stub guider"
)]
async fn rp_with_train_and_guider(world: &mut RpWorld, train_id: String) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_stub_guider(world, Duration::ZERO).await;
    push_train(world, &train_id, vec!["main-cam".to_string()]);
    start_rp(world).await;
}

#[given(
    expr = "rp is running with a camera on the simulator in imaging train {string} and a stub guider settling after {int} ms"
)]
async fn rp_with_train_and_slow_guider(world: &mut RpWorld, train_id: String, delay_ms: u64) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_stub_guider(world, Duration::from_millis(delay_ms)).await;
    push_train(world, &train_id, vec!["main-cam".to_string()]);
    start_rp(world).await;
}

#[given(
    expr = "rp is running with a camera on the simulator and a stub guider settling after {int} ms"
)]
async fn rp_without_trains_and_slow_guider(world: &mut RpWorld, delay_ms: u64) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_stub_guider(world, Duration::from_millis(delay_ms)).await;
    start_rp(world).await;
}

#[given(
    expr = "rp is running with a camera on the simulator in guiding train {string} and a stub guider settling after {int} ms"
)]
async fn rp_with_guiding_train_and_slow_guider(
    world: &mut RpWorld,
    train_id: String,
    delay_ms: u64,
) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_stub_guider(world, Duration::from_millis(delay_ms)).await;
    world.optical_trains.push(OpticalTrainConfig {
        id: train_id,
        purpose: Some("guiding".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices: vec!["main-cam".to_string()],
        auto_focus: None,
    });
    start_rp(world).await;
}

#[given(
    expr = "rp is running with a camera on the simulator in imaging train {string} and a mount"
)]
async fn rp_with_train_and_mount(world: &mut RpWorld, train_id: String) {
    ensure_omnisim(world).await;
    add_camera(world);
    world.mount = Some(MountConfig {
        alpaca_url: world.omnisim_url(),
        device_number: 0,
        settle_after_slew: None,
    });
    push_train(world, &train_id, vec!["main-cam".to_string()]);
    start_rp(world).await;
}

// --- When steps: background calls on a second MCP session ------------

pub fn spawn_background_call(world: &mut RpWorld, tool: &str, args: Value) {
    let url = world.rp_mcp_url();
    let tool_name = tool.to_string();
    let handle = tokio::spawn(async move {
        let client = McpTestClient::connect(&url).await?;
        client.call_tool(&tool_name, args).await
    });
    world.background_calls.push((tool.to_string(), handle));
}

#[when(expr = "a second MCP client starts a {string} capture of camera {string} in the background")]
async fn start_capture_in_background(world: &mut RpWorld, duration: String, camera_id: String) {
    spawn_background_call(
        world,
        "capture",
        json!({ "camera_id": camera_id, "duration": duration }),
    );
}

#[when(expr = "a second MCP client starts a dither of {float} pixels in the background")]
async fn start_dither_in_background(world: &mut RpWorld, pixels: f64) {
    spawn_background_call(world, "dither", json!({ "pixels": pixels }));
}

// --- Then steps ------------------------------------------------------

#[then(expr = "the background {string} call should succeed")]
async fn background_call_succeeds(world: &mut RpWorld, tool: String) {
    let index = world
        .background_calls
        .iter()
        .position(|(name, _)| *name == tool)
        .unwrap_or_else(|| panic!("no background '{tool}' call was started in this scenario"));
    let (_, mut handle) = world.background_calls.remove(index);
    // Poll the handle by reference so a timeout leaves it in hand for
    // an explicit abort — timing out on the owned handle would drop
    // (detach) it, and the task's MCP streaming connection would then
    // outlive the scenario and block rp's graceful shutdown. The
    // handle is already out of `world.background_calls`, so the
    // after-hook cannot abort it for us.
    let result =
        if let Ok(join_result) = tokio::time::timeout(Duration::from_mins(1), &mut handle).await {
            join_result.unwrap_or_else(|e| panic!("background '{tool}' task panicked: {e}"))
        } else {
            handle.abort();
            panic!("background '{tool}' call did not finish within 60s");
        };
    result.unwrap_or_else(|e| panic!("background '{tool}' call failed: {e}"));
}
