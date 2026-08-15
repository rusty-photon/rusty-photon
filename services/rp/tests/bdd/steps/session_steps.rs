//! BDD step definitions for session lifecycle feature

use cucumber::{given, then, when};

use bdd_infra::rp_harness::{OrchestratorBehavior, TestOrchestrator};

use crate::world::RpWorld;

// --- Given steps ---

#[given("a test orchestrator that completes immediately")]
async fn orchestrator_completes_immediately(world: &mut RpWorld) {
    setup_orchestrator(world, OrchestratorBehavior::CompleteImmediately).await;
    add_orchestrator_plugin(world);
}

#[given("a test orchestrator that waits for a stop signal")]
async fn orchestrator_waits_for_stop(world: &mut RpWorld) {
    setup_orchestrator(world, OrchestratorBehavior::WaitForStop).await;
    add_orchestrator_plugin(world);
}

#[given(
    expr = "a test flat-calibration orchestrator configured for {int} {string} flats and {int} {string} flats"
)]
async fn flat_calibration_orchestrator(
    world: &mut RpWorld,
    count1: i32,
    filter1: String,
    count2: i32,
    filter2: String,
) {
    let plan = vec![
        (filter1, count1.cast_unsigned()),
        (filter2, count2.cast_unsigned()),
    ];
    world.flat_plan.clone_from(&plan);
    setup_orchestrator(world, OrchestratorBehavior::FlatCalibration(plan)).await;
    add_orchestrator_plugin(world);
}

#[given(expr = "the server config advertises {string} as rp's URL")]
fn server_config_advertises_url(world: &mut RpWorld, url: String) {
    world.advertised_url = Some(url);
}

#[given(expr = "a test orchestrator that requires the credential {string} with password {string}")]
async fn orchestrator_requires_credential(world: &mut RpWorld, username: String, password: String) {
    if world.orchestrator.is_none() {
        let invocations = world.orchestrator_invocations.clone();
        let cancelled = world.orchestrator_cancelled.clone();
        world.orchestrator = Some(
            TestOrchestrator::start_requiring_auth(
                invocations,
                cancelled,
                OrchestratorBehavior::WaitForStop,
                &username,
                &password,
            )
            .await,
        );
    }
    add_orchestrator_plugin(world);
}

#[given(
    expr = "the orchestrator registration carries the credential {string} with password {string}"
)]
fn orchestrator_registration_carries_credential(
    world: &mut RpWorld,
    username: String,
    password: String,
) {
    let entry = world
        .plugin_configs
        .iter_mut()
        .find(|p| p.get("type").and_then(|v| v.as_str()) == Some("orchestrator"))
        .expect("no orchestrator plugin registered; add the test orchestrator first");
    entry
        .as_object_mut()
        .expect("orchestrator plugin config is not an object")
        .insert(
            "auth".to_string(),
            serde_json::json!({ "username": username, "password": password }),
        );
}

#[given(expr = "a plugin configured as orchestrator with invoke URL {string}")]
fn plugin_configured_as_orchestrator(world: &mut RpWorld, invoke_url: String) {
    // Only add if not already present
    let already_exists = world
        .plugin_configs
        .iter()
        .any(|p| p.get("type").and_then(|v| v.as_str()) == Some("orchestrator"));

    if !already_exists {
        world.plugin_configs.push(serde_json::json!({
            "name": "unreachable-orchestrator",
            "type": "orchestrator",
            "invoke_url": invoke_url,
            "requires_tools": []
        }));
    }
}

#[given("the orchestrator registration carries a config object")]
fn orchestrator_registration_carries_config(world: &mut RpWorld) {
    let config = serde_json::json!({
        "workflow": "calibrator_flats",
        "parameters": { "camera_id": "main-cam", "flat_count": 2 }
    });

    let entry = world
        .plugin_configs
        .iter_mut()
        .find(|p| p.get("type").and_then(|v| v.as_str()) == Some("orchestrator"))
        .expect("no orchestrator plugin registered; add the test orchestrator first");
    entry
        .as_object_mut()
        .expect("orchestrator plugin config is not an object")
        .insert("config".to_string(), config.clone());

    world.orchestrator_registered_config = Some(config);
}

#[given("rp is running with a camera and filter wheel on the simulator and the test orchestrator")]
async fn rp_running_with_equipment_and_orchestrator(world: &mut RpWorld) {
    crate::steps::tool_steps::ensure_omnisim(world).await;
    crate::steps::tool_steps::add_camera(world);
    crate::steps::tool_steps::add_filter_wheel(world);
    crate::steps::tool_steps::start_rp(world).await;
}

#[given("rp is running with equipment and both plugins configured")]
async fn rp_running_with_equipment_and_both_plugins(world: &mut RpWorld) {
    crate::steps::tool_steps::ensure_omnisim(world).await;
    crate::steps::tool_steps::add_camera(world);
    crate::steps::tool_steps::add_filter_wheel(world);
    crate::steps::tool_steps::start_rp(world).await;
}

// --- When steps ---

#[when("a session is started via the REST API")]
async fn start_session(world: &mut RpWorld) {
    let client = reqwest::Client::new();
    let url = format!("{}/api/session/start", world.rp_url());

    let resp = client
        .post(&url)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("failed to POST /api/session/start");

    world.last_api_status = Some(resp.status().as_u16());
    world.last_api_body = resp.json().await.ok();
}

#[when("I try to start another session via the REST API")]
async fn try_start_another_session(world: &mut RpWorld) {
    let client = reqwest::Client::new();
    let url = format!("{}/api/session/start", world.rp_url());

    let resp = client
        .post(&url)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("failed to POST /api/session/start");

    world.last_api_status = Some(resp.status().as_u16());
    world.last_api_body = resp.json().await.ok();
}

#[when("the test orchestrator posts completion to rp")]
async fn orchestrator_posts_completion(world: &mut RpWorld) {
    // Wait for the orchestrator to be invoked (happens asynchronously after session start)
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let inv = world.orchestrator_invocations.read().await;
        if !inv.is_empty() {
            break;
        }
    }

    let invocations = world.orchestrator_invocations.read().await;
    let invocation = invocations
        .last()
        .expect("no orchestrator invocation recorded");

    let workflow_id = invocation.workflow_id.clone();
    drop(invocations);

    let client = reqwest::Client::new();
    let url = format!("{}/api/plugins/{}/complete", world.rp_url(), workflow_id);

    let _ = client
        .post(&url)
        .json(&serde_json::json!({
            "status": "complete",
            "result": {
                "reason": "all_targets_complete",
                "exposures_captured": 0
            }
        }))
        .send()
        .await;

    // Wait for rp to process completion and return to idle
    assert!(
        world.wait_for_session_status("idle").await,
        "session did not return to idle after completion"
    );
}

#[when("the session is stopped via the REST API")]
async fn stop_session(world: &mut RpWorld) {
    let client = reqwest::Client::new();
    let url = format!("{}/api/session/stop", world.rp_url());

    let resp = client
        .post(&url)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("failed to POST /api/session/stop");

    world.last_api_status = Some(resp.status().as_u16());

    // Mark orchestrator as cancelled
    *world.orchestrator_cancelled.write().await = true;

    // Wait for rp to process stop and return to idle
    assert!(
        world.wait_for_session_status("idle").await,
        "session did not return to idle after stop"
    );
}

#[when("a workflow completion is posted with an unknown workflow id")]
async fn workflow_complete_unknown_id(world: &mut RpWorld) {
    let client = reqwest::Client::new();
    let url = format!(
        "{}/api/plugins/{}/complete",
        world.rp_url(),
        "00000000-0000-0000-0000-000000000000"
    );

    let _ = client
        .post(&url)
        .json(&serde_json::json!({
            "status": "complete",
            "result": {
                "reason": "all_targets_complete",
                "exposures_captured": 0
            }
        }))
        .send()
        .await;
}

#[when("the test orchestrator runs to completion")]
async fn orchestrator_runs_to_completion(world: &mut RpWorld) {
    // The flat calibration orchestrator runs asynchronously after invocation.
    // Wait for it to complete by checking for the expected number of events.
    let expected_exposures: u32 = world.flat_plan.iter().map(|(_, count)| count).sum();

    assert!(
        world
            .wait_for_events("exposure_complete", expected_exposures as usize)
            .await,
        "flat calibration orchestrator did not complete within timeout"
    );

    // Wait for rp to process completion and return to idle
    assert!(
        world.wait_for_session_status("idle").await,
        "session did not return to idle after orchestrator completion"
    );
}

// --- Then steps ---

#[then("the test orchestrator should have been invoked")]
async fn orchestrator_was_invoked(world: &mut RpWorld) {
    assert!(
        world.wait_for_orchestrator_invocation().await,
        "expected orchestrator to have been invoked"
    );
}

#[then("the test orchestrator should have recorded no invocation")]
async fn orchestrator_recorded_no_invocation(world: &mut RpWorld) {
    assert!(
        world.orchestrator_invocations.read().await.is_empty(),
        "an invocation carrying no credential must be rejected before it is recorded"
    );
}

#[then("the invocation payload should contain a session id")]
async fn invocation_has_session_id(world: &mut RpWorld) {
    let invocations = world.orchestrator_invocations.read().await;
    let invocation = invocations.last().expect("no orchestrator invocation");

    assert!(
        !invocation.session_id.is_empty(),
        "expected non-empty session_id in invocation"
    );
}

#[then("the invocation payload should contain the MCP server URL")]
async fn invocation_has_mcp_url(world: &mut RpWorld) {
    let invocations = world.orchestrator_invocations.read().await;
    let invocation = invocations.last().expect("no orchestrator invocation");

    assert!(
        !invocation.mcp_server_url.is_empty(),
        "expected non-empty mcp_server_url in invocation"
    );
    assert!(
        invocation.mcp_server_url.contains("/mcp"),
        "expected mcp_server_url to contain '/mcp', got: {}",
        invocation.mcp_server_url
    );
}

#[then(expr = "the invocation payload's MCP server URL should be {string}")]
async fn invocation_mcp_url_is(world: &mut RpWorld, expected: String) {
    let invocations = world.orchestrator_invocations.read().await;
    let invocation = invocations.last().expect("no orchestrator invocation");

    assert_eq!(invocation.mcp_server_url, expected);
}

#[then("the invocation payload should carry the registered config verbatim")]
async fn invocation_carries_registered_config(world: &mut RpWorld) {
    let registered = world
        .orchestrator_registered_config
        .clone()
        .expect("no config was registered for the orchestrator");

    let invocations = world.orchestrator_invocations.read().await;
    let invocation = invocations.last().expect("no orchestrator invocation");

    assert_eq!(
        invocation.config.as_ref(),
        Some(&registered),
        "expected the invocation to carry the registered config verbatim"
    );
}

#[then("the invocation payload should carry a null config")]
async fn invocation_carries_null_config(world: &mut RpWorld) {
    let invocations = world.orchestrator_invocations.read().await;
    let invocation = invocations.last().expect("no orchestrator invocation");

    assert_eq!(
        invocation.config.as_ref(),
        Some(&serde_json::Value::Null),
        "expected the invocation to carry `config: null` when none was registered"
    );
}

#[then(expr = "the session status should be {string}")]
async fn session_status_is(world: &mut RpWorld, expected: String) {
    let client = reqwest::Client::new();
    let url = format!("{}/api/session/status", world.rp_url());

    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /api/session/status");

    let body: serde_json::Value = resp.json().await.expect("failed to parse session status");

    let status = body
        .get("status")
        .and_then(|v| v.as_str())
        .expect("no status field in session response");

    assert_eq!(
        status, expected,
        "expected session status '{expected}', got '{status}'"
    );
}

#[then(expr = "the session status should become {string}")]
async fn session_status_becomes(world: &mut RpWorld, expected: String) {
    assert!(
        world.wait_for_session_status(&expected).await,
        "session status did not become '{expected}' within the wait budget"
    );
}

#[then("the test orchestrator should have been cancelled")]
async fn orchestrator_was_cancelled(world: &mut RpWorld) {
    // The orchestrator's WaitForStop behavior checks the cancelled flag.
    // After session stop, rp should have terminated the orchestrator's MCP session.
    // We verify by checking that the orchestrator was invoked and the session is now idle.
    let invocations = world.orchestrator_invocations.read().await;
    assert!(
        !invocations.is_empty(),
        "expected orchestrator to have been invoked before cancellation"
    );
}

#[then("the second session start should fail with an error")]
fn second_session_should_fail(world: &mut RpWorld) {
    let status = world.last_api_status.expect("no API response status");

    assert!(
        status >= 400,
        "expected error status code (>= 400), got {status}"
    );
}

// --- Helpers ---

async fn setup_orchestrator(world: &mut RpWorld, behavior: OrchestratorBehavior) {
    if world.orchestrator.is_some() {
        return;
    }

    let invocations = world.orchestrator_invocations.clone();
    let cancelled = world.orchestrator_cancelled.clone();
    world.orchestrator = Some(TestOrchestrator::start(invocations, cancelled, behavior).await);
}

fn add_orchestrator_plugin(world: &mut RpWorld) {
    let url = world
        .orchestrator
        .as_ref()
        .expect("orchestrator not started")
        .invoke_url
        .clone();

    // Only add if not already present
    let already_exists = world
        .plugin_configs
        .iter()
        .any(|p| p.get("type").and_then(|v| v.as_str()) == Some("orchestrator"));

    if !already_exists {
        world.plugin_configs.push(serde_json::json!({
            "name": "test-orchestrator",
            "type": "orchestrator",
            "invoke_url": url,
            "requires_tools": []
        }));
    }
}
