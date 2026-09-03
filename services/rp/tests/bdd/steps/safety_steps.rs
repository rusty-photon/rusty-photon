//! BDD step definitions for safety enforcement (rp.md § Safety): a
//! `SafetyMonitor` unsafe transition interrupts the active session and
//! gates `/mcp`; the safe transition re-invokes the orchestrator with
//! recovery context.
//!
//! The monitor is `OmniSim`'s safety-monitor simulator; its reported
//! `IsSafe` is flipped at runtime through `OmniSim`'s private
//! `issafesetting` endpoint.

use std::time::Duration;

use cucumber::{given, then, when};

use bdd_infra::rp_harness::{OmniSimHandle, SafetyMonitorConfig};

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

#[then(expr = "the MCP endpoint should reject requests with 503 within {int} seconds")]
async fn mcp_rejects_with_503(world: &mut RpWorld, seconds: u64) {
    assert!(
        poll_mcp_gate(world, true, Duration::from_secs(seconds)).await,
        "the MCP endpoint never answered 503 while conditions were unsafe"
    );
}

#[then(expr = "the MCP endpoint should accept requests again within {int} seconds")]
async fn mcp_accepts_again(world: &mut RpWorld, seconds: u64) {
    assert!(
        poll_mcp_gate(world, false, Duration::from_secs(seconds)).await,
        "the MCP endpoint kept answering 503 after conditions returned to safe"
    );
}

/// Poll `POST /mcp` until its status is (or stops being) 503. The body is
/// a JSON-RPC `initialize` so an ungated rp answers with a normal MCP
/// response; the step only discriminates on the 503 gate, not on rmcp's
/// protocol details.
async fn poll_mcp_gate(world: &mut RpWorld, expect_gated: bool, budget: Duration) -> bool {
    let client = reqwest::Client::new();
    let url = world.rp_mcp_url();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "bdd-gate-probe", "version": "0" }
        }
    });
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if let Ok(resp) = client
            .post(&url)
            .header("accept", "application/json, text/event-stream")
            .json(&body)
            .send()
            .await
        {
            let gated = resp.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE;
            if gated == expect_gated {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
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
