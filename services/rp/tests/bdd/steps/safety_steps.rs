//! BDD step definitions for safety enforcement (rp.md § Safety): a
//! `SafetyMonitor` unsafe transition interrupts the active session and
//! closes the safety gate — gated tools are refused with `SafetyUnsafe`
//! at dispatch, ungated ones keep answering; the safe transition opens
//! it and re-invokes the orchestrator with recovery context.
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

/// How long a poll-until-observed step waits before failing the scenario.
const OBSERVATION_BUDGET: Duration = Duration::from_secs(5);

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
    add_focuser(world, None, None);
    add_cover_calibrator(world);
    add_mount(world);
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

#[then(expr = "the test orchestrator should have been re-invoked with recovery reason {string}")]
async fn orchestrator_reinvoked_with_recovery(world: &mut RpWorld, reason: String) {
    let deadline = std::time::Instant::now() + OBSERVATION_BUDGET;
    loop {
        {
            let invocations = world.orchestrator_invocations.read().await;
            if invocations.len() >= 2 {
                let recovery = invocations
                    .last()
                    .and_then(|inv| inv.recovery.clone())
                    .expect("the re-invocation carries no `recovery` key at all");
                assert_eq!(
                    recovery.get("reason").and_then(|v| v.as_str()),
                    Some(reason.as_str()),
                    "unexpected recovery object: {recovery}"
                );
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the orchestrator was not re-invoked within {OBSERVATION_BUDGET:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[then("the recovery invocation should carry the original workflow and session ids")]
async fn recovery_invocation_carries_original_ids(world: &mut RpWorld) {
    let invocations = world.orchestrator_invocations.read().await;
    let first = invocations.first().expect("no invocations recorded");
    let last = invocations.last().expect("no invocations recorded");
    assert_eq!(
        (&first.workflow_id, &first.session_id),
        (&last.workflow_id, &last.session_id),
        "the recovery invocation must reuse the interrupted session's ids"
    );
}

// --- The safety gate (rp.md § Safety → In-Flight Tool Calls) --------

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

/// Every row is refused before dispatch: the harness renders
/// `McpCallError::SafetyStopped` naming the JSON-RPC code and the
/// monitor rp put in `data.monitor`.
#[then(
    expr = "each of these gated tools should be refused with SafetyUnsafe code -32010 naming monitor {string}:"
)]
async fn gated_tools_are_refused(world: &mut RpWorld, monitor: String, step: &Step) {
    ensure_mcp_client(world).await;
    for (tool, arguments) in tool_rows(step) {
        let result = world.mcp().call_tool(&tool, arguments).await;
        let message = result
            .as_ref()
            .err()
            .unwrap_or_else(|| panic!("{tool} was not refused while unsafe: {result:?}"));
        assert!(
            message.contains("-32010") && message.contains("safety"),
            "{tool} failed, but not with the SafetyUnsafe refusal: {message}"
        );
        assert!(
            message.contains(&monitor),
            "{tool}'s refusal does not name monitor {monitor:?}: {message}"
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
    );
}

#[when("a second MCP client starts a park in the background")]
async fn start_park_in_background(world: &mut RpWorld) {
    crate::steps::motion_gate_steps::spawn_background_call(world, "park", serde_json::json!({}));
}

/// Drop the most recently started background client mid-call. Aborting
/// the task drops its `McpTestClient`; the rmcp client's transport
/// worker outlives the abort long enough to `DELETE` its session, and
/// rp's side of that session cancels the in-flight handler through
/// the request's own token — the "client disconnected" path the
/// in-flight registry answers with `cancelled: client disconnected`.
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

/// Read `CameraState` straight from `OmniSim`'s Alpaca API (`0` is
/// `Idle`): the aborted exposure must leave the simulator camera idle,
/// not exposing into the void for the rest of its 10 s.
#[then(expr = "the simulator camera should report idle within {int} seconds")]
async fn simulator_camera_idle(world: &mut RpWorld, seconds: u64) {
    let url = format!("{}/api/v1/camera/0/camerastate", world.omnisim_url());
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let state = match client
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
        };
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
