//! BDD step definitions for power switching (rp.md § Power Switching):
//! the power map on a `switches[]` entry, `disconnect_device` and
//! `connect_device` holding a device out of the reconnect supervisor
//! and giving it back, and `get_switch` / `set_switch` reading and
//! switching a power box's outputs.
//!
//! The power box is [`PowerBoxStub`] and the device on its output is
//! [`AlpacaDeviceStub`]'s camera, so a disconnect reaches no device
//! another scenario shares.

use std::time::Duration;

use cucumber::{given, then, when};
use serde_json::{json, Value};

use bdd_infra::rp_harness::{AlpacaDeviceStub, PowerBoxStub, StubDevice, SwitchConfig};

use super::tool_steps::ensure_mcp_client;
use crate::world::RpWorld;

/// The config id of the stub power box's `switches[]` entry.
const POWER_BOX_ID: &str = "power-box";

/// How long a wait-until-observed step waits before failing the
/// scenario: a floor for slow instrumented builds, not an estimate.
const OBSERVATION_BUDGET: Duration = Duration::from_secs(15);

// --- Given steps ---

#[given(
    expr = "a stub power box with the outputs {string}, {string}, {string} and {string}, all on"
)]
fn stub_power_box(world: &mut RpWorld, a: String, b: String, c: String, d: String) {
    world.power_box = Some(PowerBoxStub::start(&[&a, &b, &c, &d]));
}

#[given("a stub Alpaca service hosting a camera")]
fn stub_hosting_camera(world: &mut RpWorld) {
    world.alpaca_stub = Some(AlpacaDeviceStub::start(StubDevice::Camera));
}

#[given(expr = "rp's power map puts {string} on the stub power box's {string} output")]
fn map_puts_device_on_output(world: &mut RpWorld, device_id: String, output: String) {
    add_output(world, json!({ "name": output, "devices": [device_id] }));
}

#[given(expr = "rp's power map lists the stub power box's {string} output with no devices")]
fn map_lists_output(world: &mut RpWorld, output: String) {
    add_output(world, json!({ "name": output }));
}

#[given(expr = "rp's power map protects the stub power box's {string} output because {string}")]
fn map_protects_output(world: &mut RpWorld, output: String, reason: String) {
    add_output(world, json!({ "name": output, "protected": reason }));
}

// --- When steps ---

/// Any tool, any arguments: a regex step so the arguments can be any
/// JSON object.
#[when(regex = r#"^the MCP client calls "([^"]+)" with the JSON (.+)$"#)]
async fn call_tool_with_json(world: &mut RpWorld, tool: String, arguments: String) {
    let arguments: Value =
        serde_json::from_str(&arguments).expect("the tool arguments must be JSON");
    ensure_mcp_client(world).await;
    let result = world.mcp().call_tool(&tool, arguments).await;
    world.last_tool_result = Some(result);
}

/// rp's reconnect supervisor reads the power box's `Connected` once a
/// pass, after every device ahead of it in its walk, the camera among
/// them: `n` more reads mean `n` more passes over the camera.
#[when(expr = "the reconnect supervisor has passed the stub power box {int} more times")]
async fn supervisor_passed(world: &mut RpWorld, n: u32) {
    let target = power_box(world).connected_reads() + n;
    let deadline = std::time::Instant::now() + OBSERVATION_BUDGET;
    while power_box(world).connected_reads() < target {
        assert!(
            std::time::Instant::now() < deadline,
            "the reconnect supervisor did not read the power box's Connected {n} more times \
             within {OBSERVATION_BUDGET:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// --- Then steps ---

#[then(expr = "the stub power box's {string} output should be on")]
fn output_on(world: &mut RpWorld, output: String) {
    assert!(
        power_box(world).is_on(&output),
        "expected the stub power box's {output:?} output to be on"
    );
}

#[then(expr = "the stub power box's {string} output should be off")]
fn output_off(world: &mut RpWorld, output: String) {
    assert!(
        !power_box(world).is_on(&output),
        "expected the stub power box's {output:?} output to be off"
    );
}

#[then("the stub camera should read disconnected")]
fn stub_camera_disconnected(world: &mut RpWorld) {
    assert!(
        !camera_stub(world).is_connected(),
        "expected the stub camera's Connected to be false"
    );
}

#[then("the stub camera should read connected")]
fn stub_camera_connected(world: &mut RpWorld) {
    assert!(
        camera_stub(world).is_connected(),
        "expected the stub camera's Connected to be true"
    );
}

#[then("the stub camera should have been sent Connected=true once and Connected=false never")]
fn stub_camera_not_cycled(world: &mut RpWorld) {
    let stub = camera_stub(world);
    assert_eq!(
        (stub.connect_requests(), stub.disconnect_requests()),
        (1, 0),
        "expected one Connected=true and no Connected=false (true, false)"
    );
}

#[then(expr = "the equipment status should show the camera {string} held out")]
async fn camera_held_out(world: &mut RpWorld, id: String) {
    let entry = camera_status(world, &id).await;
    assert_eq!(
        (entry.get("connected"), entry.get("held_out")),
        (Some(&json!(false)), Some(&json!(true))),
        "expected {id} disconnected and held out: {entry}"
    );
}

#[then(expr = "the equipment status should show the camera {string} connected and not held out")]
async fn camera_connected_not_held_out(world: &mut RpWorld, id: String) {
    let entry = camera_status(world, &id).await;
    assert_eq!(
        (entry.get("connected"), entry.get("held_out")),
        (Some(&json!(true)), Some(&json!(false))),
        "expected {id} connected and not held out: {entry}"
    );
}

// --- helpers ---

/// Add one output to the stub power box's `switches[]` entry, creating
/// the entry on the first call.
fn add_output(world: &mut RpWorld, output: Value) {
    let url = power_box(world).url();
    if !world.switches.iter().any(|s| s.id == POWER_BOX_ID) {
        world.switches.push(SwitchConfig {
            id: POWER_BOX_ID.to_string(),
            alpaca_url: url,
            device_number: 0,
            outputs: Vec::new(),
        });
    }
    world
        .switches
        .iter_mut()
        .find(|s| s.id == POWER_BOX_ID)
        .expect("the power box entry was just added")
        .outputs
        .push(output);
}

const fn power_box(world: &RpWorld) -> &PowerBoxStub {
    world
        .power_box
        .as_ref()
        .expect("no stub power box — add a 'Given a stub power box ...' step")
}

const fn camera_stub(world: &RpWorld) -> &AlpacaDeviceStub {
    world
        .alpaca_stub
        .as_ref()
        .expect("no stub Alpaca service — add a 'Given a stub Alpaca service ...' step")
}

/// The camera `id`'s entry in one `GET /api/equipment` read.
async fn camera_status(world: &RpWorld, id: &str) -> Value {
    let url = format!("{}/api/equipment", world.rp_url());
    let body: Value = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .expect("GET /api/equipment failed")
        .json()
        .await
        .expect("GET /api/equipment did not answer JSON");
    body["cameras"]
        .as_array()
        .and_then(|cameras| cameras.iter().find(|c| c["id"] == id))
        .cloned()
        .unwrap_or_else(|| panic!("no camera {id} in GET /api/equipment: {body}"))
}
