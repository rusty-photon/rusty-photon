//! BDD step definitions for safety enforcement (rp.md § Safety): a
//! `SafetyMonitor` unsafe transition cancels in-flight gated work and
//! closes the safety gate — gated tools are refused with `SafetyUnsafe`
//! at dispatch, ungated ones keep answering; the safe transition opens
//! it and nothing else (rp re-invokes nobody).
//!
//! The monitor is `OmniSim`'s safety-monitor simulator; its reported
//! `IsSafe` is flipped at runtime through `OmniSim`'s private
//! `issafesetting` endpoint.

use std::time::Duration;

use cucumber::gherkin::Step;
use cucumber::{given, then, when};
use serde_json::Value;

use bdd_infra::rp_harness::{MountConfig, OmniSimHandle, SafetyMonitorConfig};

use crate::steps::cover_calibrator_steps::add_cover_calibrator;
use crate::steps::focuser_steps::add_focuser;
use crate::steps::tool_steps::{
    add_camera, add_filter_wheel, ensure_mcp_client, ensure_omnisim, start_rp,
};
use crate::world::RpWorld;

#[given("a safety monitor on the simulator")]
async fn safety_monitor_on_simulator(world: &mut RpWorld) {
    crate::steps::tool_steps::ensure_omnisim(world).await;
    // Start from a known-safe reading regardless of what a previous
    // scenario (or crashed run) left in the simulator's memory.
    OmniSimHandle::set_safety_monitor_is_safe(true)
        .await
        .expect("failed to reset OmniSim's safety monitor to safe");
    world.safety_monitors.push(SafetyMonitorConfig {
        id: "weather-watcher".to_string(),
        alpaca_url: world.omnisim_url(),
        device_number: 0,
    });
    // Fast polling so transitions are detected in test time, not the
    // production default (10 s).
    world.safety_poll_interval = Some(Duration::from_millis(250));
}

/// The gated set's two shapes on one rig: mount motion and exposed
/// optics.
#[given("rp is running with a mount and a cover calibrator on the simulator")]
async fn rp_with_mount_and_cover(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_mount(world);
    add_cover_calibrator(world);
    start_rp(world).await;
}

/// Every ungated shape on one rig: camera, filter wheel, focuser, cover
/// calibrator and mount.
#[given("rp is running with a full indoor rig on the simulator")]
async fn rp_with_full_indoor_rig(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_filter_wheel(world);
    add_focuser(world, None, None, None);
    add_cover_calibrator(world);
    add_mount(world);
    start_rp(world).await;
}

/// The capture rig (a train of camera and filter wheel, flat
/// `<uuid8>.fits` names) plus a mount for the unsafe transition to park.
#[given("rp is running with a mount and a capture rig and no naming templates configured")]
async fn rp_with_mount_and_capture_rig(world: &mut RpWorld) {
    crate::steps::capture_target_linkage_steps::capture_rig_without_templates(world).await;
    add_mount(world);
    // OmniSim's mount site: rp refuses to start on a mount whose site
    // disagrees with the configured one.
    world.site = Some((51.0786, -0.2944));
    start_rp(world).await;
}

fn add_mount(world: &mut RpWorld) {
    if world.mount.is_none() {
        world.mount = Some(MountConfig {
            alpaca_url: world.omnisim_url(),
            device_number: 0,
            settle_after_slew: None,
        });
    }
}

#[given(expr = "a safety gate override gating {string} and ungating {string}")]
fn safety_gate_override(world: &mut RpWorld, gated: String, ungated: String) {
    world.safety_gate = Some((vec![gated], vec![ungated]));
}

/// A minimal config (no equipment) whose `safety.gate` names `tool`,
/// for the startup-validation scenario; consumed by `rp attempts to
/// start` in `target_naming_template_steps.rs`.
#[given(expr = "an rp config with a safety gate override gating {string}")]
fn config_with_gate_override(world: &mut RpWorld, tool: String) {
    let dir = tempfile::tempdir().expect("create temp dir for rp config");
    let config = serde_json::json!({
        "session": { "data_directory": dir.path().join("data").to_string_lossy() },
        "equipment": {},
        "safety": { "gate": { "gated": [tool] } },
        "server": { "port": 0, "bind_address": "127.0.0.1" }
    });
    let path = dir.path().join("rp.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&config).expect("serialize rp config"),
    )
    .expect("write rp config file");
    world.config_rest_path = Some(path);
    world.config_rest_dir = Some(dir);
}

#[when("the safety monitor reports unsafe")]
async fn safety_monitor_reports_unsafe(_world: &mut RpWorld) {
    OmniSimHandle::set_safety_monitor_is_safe(false)
        .await
        .expect("failed to flip OmniSim's safety monitor to unsafe");
}

#[when("the safety monitor reports safe again")]
async fn safety_monitor_reports_safe(_world: &mut RpWorld) {
    OmniSimHandle::set_safety_monitor_is_safe(true)
        .await
        .expect("failed to flip OmniSim's safety monitor to safe");
}

// --- The safety gate (rp.md § Safety → In-Flight Tool Calls) --------

/// The safe transition's whole effect: a gated tool dispatches again.
/// Called with no arguments, so the tool must be one that takes none
/// (`unpark` on the parked-by-the-transition mount).
#[then(expr = "the gated tool {string} should answer again")]
async fn gated_tool_answers_again(world: &mut RpWorld, tool: String) {
    ensure_mcp_client(world).await;
    let result = world.mcp().call_tool(&tool, serde_json::json!({})).await;
    assert!(
        result.is_ok(),
        "{tool} must dispatch once the gate is open, got: {result:?}"
    );
}

/// `get_safety_status` through the scenario's MCP client. The tool is
/// ungated, so it answers whatever the conditions — that is the point.
async fn read_safety_status(world: &mut RpWorld) -> Value {
    ensure_mcp_client(world).await;
    world
        .mcp()
        .call_tool("get_safety_status", serde_json::json!({}))
        .await
        .expect("get_safety_status must answer whatever the conditions")
}

/// Poll `get_safety_status` until `overall` reads `expected` — the
/// observable that the enforcer's poll has landed and the gate moved.
#[when(expr = "the safety status reports overall {string} within {int} seconds")]
async fn safety_status_reports_overall(world: &mut RpWorld, expected: String, seconds: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let status = read_safety_status(world).await;
        if status["overall"] == expected {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "safety status never reported overall {expected:?} within {seconds}s; last: {status}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[then(expr = "the safety status should list monitor {string} as {string}")]
async fn safety_status_lists_monitor(world: &mut RpWorld, id: String, state: String) {
    let status = read_safety_status(world).await;
    let monitors = status["monitors"]
        .as_array()
        .unwrap_or_else(|| panic!("no monitors array in {status}"));
    let monitor = monitors
        .iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("monitor {id:?} not listed in {status}"));
    assert_eq!(monitor["state"], state, "monitor {id}: {status}");
    assert!(
        monitor["since"].is_string(),
        "monitor {id} carries no since stamp: {status}"
    );
}

#[then(expr = "the safety status should list {string} as gated")]
async fn safety_status_lists_gated(world: &mut RpWorld, tool: String) {
    let status = read_safety_status(world).await;
    assert!(
        gated_list(&status).contains(&tool.as_str()),
        "{tool} is not in the gated list: {status}"
    );
}

#[then(expr = "the safety status should not list {string} as gated")]
async fn safety_status_does_not_list_gated(world: &mut RpWorld, tool: String) {
    let status = read_safety_status(world).await;
    assert!(
        !gated_list(&status).contains(&tool.as_str()),
        "{tool} is in the gated list: {status}"
    );
}

fn gated_list(status: &Value) -> Vec<&str> {
    status["gated"]
        .as_array()
        .unwrap_or_else(|| panic!("no gated array in {status}"))
        .iter()
        .filter_map(Value::as_str)
        .collect()
}

/// The `| tool | arguments |` rows of a step table, arguments parsed
/// as the JSON object the tool is called with.
fn tool_rows(step: &Step) -> Vec<(String, Value)> {
    let table = step
        .table
        .as_ref()
        .expect("this step takes a | tool | arguments | table");
    let mut rows = table.rows.iter();
    let header = rows.next().expect("table header row");
    assert_eq!(header, &["tool".to_string(), "arguments".to_string()]);
    rows.map(|row| {
        let arguments: Value = serde_json::from_str(&row[1])
            .unwrap_or_else(|e| panic!("row {row:?}: arguments are not JSON: {e}"));
        (row[0].clone(), arguments)
    })
    .collect()
}

/// The `| arguments |` rows of a step table, each the JSON object a
/// `capture` is called with.
fn capture_rows(step: &Step) -> Vec<Value> {
    let table = step
        .table
        .as_ref()
        .expect("this step takes an | arguments | table");
    let mut rows = table.rows.iter();
    let header = rows.next().expect("table header row");
    assert_eq!(header, &["arguments".to_string()]);
    rows.map(|row| {
        serde_json::from_str(&row[0])
            .unwrap_or_else(|e| panic!("row {row:?}: arguments are not JSON: {e}"))
    })
    .collect()
}

/// The harness renders `McpCallError::SafetyStopped` naming the
/// JSON-RPC code and the monitor rp put in `data.monitor`.
fn assert_safety_refusal(call: &str, result: &Result<Value, String>, monitor: &str) {
    let message = result
        .as_ref()
        .err()
        .unwrap_or_else(|| panic!("{call} was not refused while unsafe: {result:?}"));
    assert!(
        message.contains("-32010") && message.contains("safety"),
        "{call} failed, but not with the SafetyUnsafe refusal: {message}"
    );
    assert!(
        message.contains(monitor),
        "{call}'s refusal does not name monitor {monitor:?}: {message}"
    );
}

/// Every row is refused before dispatch.
#[then(
    expr = "each of these gated tools should be refused with SafetyUnsafe code -32010 naming monitor {string}:"
)]
async fn gated_tools_are_refused(world: &mut RpWorld, monitor: String, step: &Step) {
    ensure_mcp_client(world).await;
    for (tool, arguments) in tool_rows(step) {
        let result = world.mcp().call_tool(&tool, arguments).await;
        assert_safety_refusal(&tool, &result, &monitor);
    }
}

/// Every row is a `capture` refused with the gated tools' refusal,
/// although `capture` is ungated: its arguments ask for a frame that
/// needs the sky.
#[then(
    expr = "each of these captures should be refused with SafetyUnsafe code -32010 naming monitor {string}:"
)]
async fn sky_captures_are_refused(world: &mut RpWorld, monitor: String, step: &Step) {
    ensure_mcp_client(world).await;
    for arguments in capture_rows(step) {
        let call = format!("capture {arguments}");
        let result = world.mcp().call_tool("capture", arguments).await;
        assert_safety_refusal(&call, &result, &monitor);
    }
}

/// Every row is a `capture` that succeeds and saves a frame.
#[then("each of these captures should be taken:")]
async fn captures_are_taken(world: &mut RpWorld, step: &Step) {
    ensure_mcp_client(world).await;
    for arguments in capture_rows(step) {
        let call = format!("capture {arguments}");
        let result = world
            .mcp()
            .call_tool("capture", arguments)
            .await
            .unwrap_or_else(|message| panic!("{call} was not taken: {message}"));
        assert!(
            result.get("document_id").and_then(Value::as_str).is_some(),
            "{call} answered without a document_id: {result}"
        );
    }
}

/// Every row is dispatched and succeeds, whatever the conditions.
#[then("each of these ungated tools should answer:")]
async fn ungated_tools_answer(world: &mut RpWorld, step: &Step) {
    ensure_mcp_client(world).await;
    for (tool, arguments) in tool_rows(step) {
        if let Err(message) = world.mcp().call_tool(&tool, arguments).await {
            panic!("{tool} did not answer while unsafe: {message}");
        }
    }
}

// --- In-flight tool calls (rp.md § Safety → In-Flight Tool Calls) ----

#[when(expr = "a second MCP client starts a slew to ra {string} dec {string} in the background")]
async fn start_slew_in_background(world: &mut RpWorld, ra: String, dec: String) {
    let ra: f64 = ra.parse().expect("ra must parse as f64");
    let dec: f64 = dec.parse().expect("dec must parse as f64");
    crate::steps::motion_gate_steps::spawn_background_call(
        world,
        "slew",
        serde_json::json!({ "ra": ra, "dec": dec }),
    )
    .await;
}

/// A frame that needs no sky, started on a second session; follow it
/// with the `exposure_started` barrier (`spawn_background_call`).
#[when(
    regex = r#"^a second MCP client starts a "([^"]+)" (Dark|panel-lit Flat) capture of camera "([^"]+)" in the background$"#
)]
async fn start_indoor_capture_in_background(
    world: &mut RpWorld,
    duration: String,
    frame: String,
    camera_id: String,
) {
    let mut arguments = serde_json::json!({ "camera_id": camera_id, "duration": duration });
    if frame == "Dark" {
        arguments["frame_type"] = "Dark".into();
    } else {
        arguments["frame_type"] = "Flat".into();
        arguments["light_source"] = "panel".into();
    }
    crate::steps::motion_gate_steps::spawn_background_call(world, "capture", arguments).await;
}

#[when("a second MCP client starts a park in the background")]
async fn start_park_in_background(world: &mut RpWorld) {
    crate::steps::motion_gate_steps::spawn_background_call(world, "park", serde_json::json!({}))
        .await;
}

/// Drop the most recently started background client mid-call. Aborting
/// the task drops its `McpTestClient`, which drops the HTTP request
/// still waiting on its response; rp's transport cancels the in-flight
/// handler through the request's own token when that connection goes
/// away — the "client disconnected" path the in-flight registry
/// answers with `cancelled: client disconnected`. There is no session
/// to `DELETE` (the transport is session-less): the connection is the
/// signal.
#[when("the second MCP client disconnects")]
async fn second_client_disconnects(world: &mut RpWorld) {
    let (_tool, handle) = world
        .background_calls
        .pop()
        .expect("no background call was started in this scenario");
    handle.abort();
    // The join result is a `JoinError::Cancelled`; nothing to assert
    // on it, but awaiting it pins the drop before the next step reads.
    let _ = handle.await;
}

#[then(expr = "the background {string} call should fail with {string} within {int} seconds")]
async fn background_call_fails_with(
    world: &mut RpWorld,
    tool: String,
    expected: String,
    seconds: u64,
) {
    let index = world
        .background_calls
        .iter()
        .position(|(name, _)| *name == tool)
        .unwrap_or_else(|| panic!("no background '{tool}' call was started in this scenario"));
    let (_, mut handle) = world.background_calls.remove(index);
    // Poll by reference so a timeout leaves the handle in hand for an
    // explicit abort (see `background_call_succeeds`).
    let result = if let Ok(join_result) =
        tokio::time::timeout(Duration::from_secs(seconds), &mut handle).await
    {
        join_result.unwrap_or_else(|e| panic!("background '{tool}' task panicked: {e}"))
    } else {
        handle.abort();
        panic!("background '{tool}' call did not return within {seconds}s");
    };
    let message = result.expect_err("the background call returned success, not the expected error");
    assert!(
        message.contains(&expected),
        "expected the background '{tool}' call to fail with '{expected}', got '{message}'"
    );
}

/// `CameraState` read straight from `OmniSim`'s Alpaca API (`0` is
/// `Idle`, `2` is `Exposing`); `None` when the read fails.
async fn simulator_camera_state(world: &RpWorld) -> Option<i64> {
    let url = format!("{}/api/v1/camera/0/camerastate", world.omnisim_url());
    match reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
    {
        Ok(resp) => resp
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|body| body.get("Value").and_then(serde_json::Value::as_i64)),
        Err(_) => None,
    }
}

/// The aborted exposure must leave the simulator camera idle, not
/// exposing into the void for the rest of its 10 s.
#[then(expr = "the simulator camera should report idle within {int} seconds")]
async fn simulator_camera_idle(world: &mut RpWorld, seconds: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let state = simulator_camera_state(world).await;
        if state == Some(0) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the simulator camera never reported idle within {seconds}s (last state: {state:?})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The frame is still exposing: nothing has aborted it.
#[then("the simulator camera should still be exposing")]
async fn simulator_camera_still_exposing(world: &mut RpWorld) {
    let state = simulator_camera_state(world).await;
    assert_eq!(
        state,
        Some(2),
        "the simulator camera is not exposing (CameraState {state:?})"
    );
}

#[then("the background capture should have saved a frame")]
fn background_capture_saved_a_frame(world: &mut RpWorld) {
    let result = world.last_background_result.as_ref().expect(
        "no background call joined yet — add 'the background \"capture\" call should succeed' first",
    );
    assert!(
        result.get("document_id").and_then(Value::as_str).is_some(),
        "the background capture answered without a document_id: {result}"
    );
}
