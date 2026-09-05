//! BDD step definitions for the flats tools served through rp
//! (`flats_tools.feature`).
//!
//! The scenarios spawn three processes in this order: `OmniSim` (Alpaca
//! simulator), calibrator-flats (the tool provider under test, started
//! first because rp dials it at startup and it must answer `tools/list`
//! on its own), and rp with the provider registered — on a port picked
//! in advance, since the provider's config names rp's MCP URL before rp
//! exists. Tools are then called through rp's proxy with the harness
//! MCP client, exactly as a session-runner document would. All three
//! processes are coordinated via `bdd_infra::rp_harness` helpers; this
//! file holds only the Gherkin step wiring and the provider-specific
//! config builder.

use std::time::Duration;

use bdd_infra::rp_harness::{
    build_calibrator_flats_config, start_rp, write_temp_config_file, CameraConfig,
    CoverCalibratorConfig, FilterWheelConfig, McpTestClient, OmniSimHandle, OpticalTrainConfig,
    SafetyMonitorConfig, WebhookReceiver,
};
use bdd_infra::ServiceHandle;
use calibrator_flats::store::{FlatRecord, FlatStore};
use cucumber::{given, then, when};
use serde_json::Value;

use crate::world::CalibratorFlatsWorld;

/// The registration name rp knows the provider by.
const PROVIDER_NAME: &str = "calibrator-flats";

/// The tools the provider offers; the registration ungates all three
/// (docs/services/calibrator-flats.md § Registration in rp).
const PROVIDER_TOOLS: [&str; 3] = ["train_flats", "take_flats", "get_flat_training"];

/// The rp tools the provider calls — the registration's `requires_tools`.
const REQUIRED_RP_TOOLS: [&str; 10] = [
    "get_train_info",
    "get_camera_info",
    "capture",
    "compute_image_stats",
    "set_filter",
    "get_cover_state",
    "close_cover",
    "open_cover",
    "calibrator_on",
    "calibrator_off",
];

/// Alpaca `CalibratorState` values (ASCOM `CalibratorStatus`).
const CALIBRATOR_OFF: i64 = 1;
const CALIBRATOR_READY: i64 = 3;
/// Alpaca `CoverState` values (ASCOM `CoverStatus`).
const COVER_CLOSED: i64 = 1;
const COVER_OPEN: i64 = 3;

// ---------------------------------------------------------------------------
// Given steps
// ---------------------------------------------------------------------------

#[given("a running Alpaca simulator")]
async fn running_alpaca_simulator(world: &mut CalibratorFlatsWorld) {
    ensure_omnisim(world).await;
}

#[given(expr = "the cover starts {word}")]
async fn cover_starts(world: &mut CalibratorFlatsWorld, state: String) {
    ensure_omnisim(world).await;
    OmniSimHandle::set_cover_closed(state == "closed")
        .await
        .unwrap_or_else(|e| panic!("failed to preset the cover {state}: {e}"));
}

#[given("a safety monitor on the simulator")]
async fn safety_monitor_on_simulator(world: &mut CalibratorFlatsWorld) {
    ensure_omnisim(world).await;
    OmniSimHandle::set_safety_monitor_is_safe(true)
        .await
        .expect("failed to preset the safety monitor safe");
    world.safety_monitors.push(SafetyMonitorConfig {
        id: "weather-watcher".to_string(),
        alpaca_url: world.omnisim_url(),
        device_number: 0,
    });
}

/// A provider config knob pinned by the scenario: a value that parses as
/// a number is sent as one (`tolerance "1.0"`, `max_iterations "1"`),
/// anything else as a string (`min_exposure "5s"`).
#[given(expr = "the flats provider is configured with {word} {string}")]
fn provider_configured_with(world: &mut CalibratorFlatsWorld, key: String, value: String) {
    let json = value
        .parse::<u64>()
        .map(Value::from)
        .or_else(|_| value.parse::<f64>().map(Value::from))
        .unwrap_or(Value::String(value));
    world.provider_overrides.insert(key, json);
}

#[given(expr = "a test webhook receiver subscribed to {string}")]
async fn webhook_receiver_subscribed_to(world: &mut CalibratorFlatsWorld, event_type: String) {
    if world.webhook_receiver.is_none() {
        let events = world.received_events.clone();
        world.webhook_receiver = Some(
            WebhookReceiver::start(events, Duration::from_secs(5), Duration::from_secs(10)).await,
        );
    }
    let url = world
        .webhook_receiver
        .as_ref()
        .expect("webhook receiver not started")
        .url
        .clone();
    world.plugin_configs.push(serde_json::json!({
        "name": "test-event-plugin",
        "type": "event",
        "webhook_url": url,
        "subscribes_to": [event_type]
    }));
}

/// Seed the scenario's store before the provider opens it: a record
/// whose camera fact no longer matches what rp will report.
#[given(
    expr = "a stored flat training record for train {string} filter {string} trained on camera {string}"
)]
async fn stored_training_record(
    world: &mut CalibratorFlatsWorld,
    train_id: String,
    filter: String,
    camera_id: String,
) {
    let store = FlatStore::open(world.store_path())
        .await
        .expect("open the scenario's store for seeding");
    store
        .put(FlatRecord {
            train_id,
            filter: Some(filter),
            duration: Duration::from_millis(100),
            brightness: 1,
            median_adu: 32_000,
            max_adu: 65_535,
            bin_x: 1,
            bin_y: 1,
            gain: None,
            offset: None,
            camera_id,
            trained_at: "2026-01-01T00:00:00Z".to_string(),
        })
        .await
        .expect("seed the training record");
    // `store` drops here, releasing redb's file lock before the
    // provider process opens the same file.
}

#[given(
    "rp is running with an imaging train on the simulator and calibrator-flats registered as a tool provider"
)]
async fn rp_running_with_imaging_train_and_provider(world: &mut CalibratorFlatsWorld) {
    configure_rig(world, true).await;
    start_provider_then_rp(world).await;
}

#[given(
    "rp is running with a filterless imaging train on the simulator and calibrator-flats registered as a tool provider"
)]
async fn rp_running_with_filterless_train_and_provider(world: &mut CalibratorFlatsWorld) {
    configure_rig(world, false).await;
    start_provider_then_rp(world).await;
}

#[given("an MCP client connected to rp")]
async fn mcp_client_connected(world: &mut CalibratorFlatsWorld) {
    ensure_mcp_client(world).await;
}

// ---------------------------------------------------------------------------
// When steps
// ---------------------------------------------------------------------------

#[when(regex = r#"^the MCP client calls "([^"]+)" with (.+)$"#)]
async fn call_tool(world: &mut CalibratorFlatsWorld, tool: String, arguments: String) {
    ensure_mcp_client(world).await;
    let args: Value = serde_json::from_str(&arguments).expect("arguments must be JSON");
    world.last_tool_result = Some(world.mcp().call_tool(&tool, args).await);
}

#[when("the MCP client lists available tools")]
async fn list_tools(world: &mut CalibratorFlatsWorld) {
    ensure_mcp_client(world).await;
    let tools = world
        .mcp()
        .list_tools()
        .await
        .unwrap_or_else(|e| panic!("tools/list failed: {e}"));
    world.last_tool_list = Some(tools);
}

#[when(regex = r#"^a second MCP client starts "([^"]+)" with (.+) in the background$"#)]
fn start_tool_in_background(world: &mut CalibratorFlatsWorld, tool: String, arguments: String) {
    let args: Value = serde_json::from_str(&arguments).expect("arguments must be JSON");
    let url = world.rp_mcp_url();
    let tool_name = tool.clone();
    let handle = tokio::spawn(async move {
        let client = McpTestClient::connect(&url).await?;
        client.call_tool(&tool_name, args).await
    });
    world.background_calls.push((tool, handle));
}

/// Wait for the panel to reach `Ready` — the background call is past
/// its pre-flight and mid-run.
#[when("the calibrator panel is lit")]
async fn calibrator_panel_is_lit(_world: &mut CalibratorFlatsWorld) {
    wait_for_calibrator_state(CALIBRATOR_READY, Duration::from_secs(60)).await;
}

/// Drop the most recently started background client mid-call. Aborting
/// the task drops its `McpTestClient`, which drops the HTTP request
/// still waiting on its response; rp's transport cancels the in-flight
/// proxied call through the request's own token when that connection
/// goes away, and forwards `notifications/cancelled` to the provider.
#[when("the second MCP client disconnects")]
async fn second_client_disconnects(world: &mut CalibratorFlatsWorld) {
    let (_tool, handle) = world
        .background_calls
        .pop()
        .expect("no background call was started in this scenario");
    handle.abort();
    let _ = handle.await;
}

#[when("the safety monitor reports unsafe")]
async fn safety_monitor_unsafe(_world: &mut CalibratorFlatsWorld) {
    OmniSimHandle::set_safety_monitor_is_safe(false)
        .await
        .expect("failed to flip the safety monitor unsafe");
}

/// Poll `get_safety_status` until `overall` reads `expected` — the
/// observable that rp's enforcer has seen the flip and the gate moved.
#[when(expr = "the safety status reports overall {string} within {int} seconds")]
async fn safety_status_reports_overall(
    world: &mut CalibratorFlatsWorld,
    expected: String,
    seconds: u64,
) {
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

// ---------------------------------------------------------------------------
// Then steps
// ---------------------------------------------------------------------------

#[then("the tool call should succeed")]
fn tool_call_succeeds(world: &mut CalibratorFlatsWorld) {
    if let Err(message) = world.last_result() {
        panic!("the tool call failed: {message}");
    }
}

#[then("the tool call should return an error")]
fn tool_call_errors(world: &mut CalibratorFlatsWorld) {
    if let Ok(value) = world.last_result() {
        panic!("the tool call succeeded, expected an error: {value}");
    }
}

#[then(expr = "the error message should contain {string}")]
fn error_message_contains(world: &mut CalibratorFlatsWorld, expected: String) {
    let message = world
        .last_result()
        .as_ref()
        .expect_err("the tool call succeeded, so there is no error message");
    assert!(
        message.contains(&expected),
        "error message {message:?} does not contain {expected:?}"
    );
}

#[then(expr = "the tool result {string} should be {string}")]
fn tool_result_string(world: &mut CalibratorFlatsWorld, pointer: String, expected: String) {
    let value = result_at(world, &pointer);
    assert_eq!(
        value.as_str(),
        Some(expected.as_str()),
        "{pointer}: {value}"
    );
}

#[then(expr = "the tool result {string} should be {int}")]
fn tool_result_int(world: &mut CalibratorFlatsWorld, pointer: String, expected: i64) {
    let value = result_at(world, &pointer);
    assert_eq!(value.as_i64(), Some(expected), "{pointer}: {value}");
}

#[then(expr = "the tool result {string} should be null")]
fn tool_result_null(world: &mut CalibratorFlatsWorld, pointer: String) {
    let value = result_at(world, &pointer);
    assert!(value.is_null(), "{pointer}: {value}");
}

#[then(expr = "the tool result {string} should be false")]
fn tool_result_false(world: &mut CalibratorFlatsWorld, pointer: String) {
    let value = result_at(world, &pointer);
    assert_eq!(value.as_bool(), Some(false), "{pointer}: {value}");
}

#[then(expr = "the tool result {string} should be true")]
fn tool_result_true(world: &mut CalibratorFlatsWorld, pointer: String) {
    let value = result_at(world, &pointer);
    assert_eq!(value.as_bool(), Some(true), "{pointer}: {value}");
}

#[then(expr = "the tool result {string} should have {int} entries")]
fn tool_result_len(world: &mut CalibratorFlatsWorld, pointer: String, expected: usize) {
    let value = result_at(world, &pointer);
    let list = value
        .as_array()
        .unwrap_or_else(|| panic!("{pointer} is not a list: {value}"));
    assert_eq!(list.len(), expected, "{pointer}: {value}");
}

#[then(expr = "the tool result {string} should contain an entry mentioning {string}")]
fn tool_result_list_mentions(world: &mut CalibratorFlatsWorld, pointer: String, needle: String) {
    let value = result_at(world, &pointer);
    let list = value
        .as_array()
        .unwrap_or_else(|| panic!("{pointer} is not a list: {value}"));
    assert!(
        list.iter()
            .any(|entry| entry.as_str().is_some_and(|s| s.contains(&needle))),
        "no entry of {pointer} mentions {needle:?}: {value}"
    );
}

#[then(expr = "the tool list should include {string}")]
fn tool_list_includes(world: &mut CalibratorFlatsWorld, tool: String) {
    let tools = world
        .last_tool_list
        .as_ref()
        .expect("no tool list was fetched in this scenario");
    assert!(tools.contains(&tool), "tool list lacks {tool}: {tools:?}");
}

#[then(expr = "the safety status should not list {string} as gated")]
async fn safety_status_does_not_list_gated(world: &mut CalibratorFlatsWorld, tool: String) {
    let status = read_safety_status(world).await;
    let gated = status["gated"]
        .as_array()
        .unwrap_or_else(|| panic!("no gated array in {status}"));
    assert!(
        !gated.iter().any(|t| t == &tool),
        "{tool} is in the gated list: {status}"
    );
}

#[then(expr = "the test webhook receiver should have received at least {int} {string} event(s)")]
async fn should_receive_at_least_n_events(
    world: &mut CalibratorFlatsWorld,
    count: usize,
    event_type: String,
) {
    assert!(
        world.wait_for_events(&event_type, count).await,
        "expected at least {count} '{event_type}' event(s) within timeout"
    );
}

#[then(expr = "the cover should be {word}")]
async fn cover_should_be(_world: &mut CalibratorFlatsWorld, expected: String) {
    let target = if expected == "closed" {
        COVER_CLOSED
    } else {
        COVER_OPEN
    };
    let state = OmniSimHandle::cover_state()
        .await
        .expect("failed to read the simulator's cover state");
    assert_eq!(
        state, target,
        "expected the cover to be {expected} (CoverState {target}), got CoverState {state}"
    );
}

#[then(expr = "the cover should be open within {int} seconds")]
async fn cover_open_within(_world: &mut CalibratorFlatsWorld, seconds: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let state = OmniSimHandle::cover_state()
            .await
            .expect("failed to read the simulator's cover state");
        if state == COVER_OPEN {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the cover did not open within {seconds}s (last CoverState {state})"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[then("the calibrator panel should be off")]
async fn calibrator_panel_off(_world: &mut CalibratorFlatsWorld) {
    let state = OmniSimHandle::calibrator_state()
        .await
        .expect("failed to read the simulator's calibrator state");
    assert_eq!(
        state, CALIBRATOR_OFF,
        "expected the panel off (CalibratorState {CALIBRATOR_OFF}), got CalibratorState {state}"
    );
}

#[then(expr = "the calibrator panel should be off within {int} seconds")]
async fn calibrator_panel_off_within(_world: &mut CalibratorFlatsWorld, seconds: u64) {
    wait_for_calibrator_state(CALIBRATOR_OFF, Duration::from_secs(seconds)).await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn ensure_omnisim(world: &mut CalibratorFlatsWorld) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
}

pub async fn ensure_mcp_client(world: &mut CalibratorFlatsWorld) {
    if world.mcp_client.is_none() {
        let url = world.rp_mcp_url();
        let client = McpTestClient::connect(&url)
            .await
            .unwrap_or_else(|e| panic!("failed to connect the MCP client to rp: {e}"));
        world.mcp_client = Some(client);
    }
}

/// The reference rig: camera `main-cam`, cover calibrator `flat-panel`,
/// and — with `wheel` — filter wheel `main-fw` (Luminance, Red, Green,
/// Blue); one imaging train `main` with the calibrator first.
async fn configure_rig(world: &mut CalibratorFlatsWorld, wheel: bool) {
    ensure_omnisim(world).await;
    let alpaca_url = world.omnisim_url();

    world.cameras.push(CameraConfig {
        id: "main-cam".to_string(),
        alpaca_url: alpaca_url.clone(),
        device_number: 0,
        cooler_targets_c: Vec::new(),
    });
    let mut devices = vec!["flat-panel".to_string()];
    if wheel {
        world.filter_wheels.push(FilterWheelConfig {
            id: "main-fw".to_string(),
            alpaca_url: alpaca_url.clone(),
            device_number: 0,
            filters: vec![
                "Luminance".to_string(),
                "Red".to_string(),
                "Green".to_string(),
                "Blue".to_string(),
            ],
        });
        devices.push("main-fw".to_string());
    }
    devices.push("main-cam".to_string());
    world.cover_calibrators.push(CoverCalibratorConfig {
        id: "flat-panel".to_string(),
        alpaca_url,
        device_number: 0,
        // OmniSim's cover sweep and lamp warm-up are seconds long; rp's
        // 3 s default poll would bound every state read by it.
        poll_interval: Some(Duration::from_millis(100)),
    });
    world.optical_trains.push(OpticalTrainConfig {
        id: "main".to_string(),
        purpose: Some("imaging".to_string()),
        focal_length_mm: Some(500.0),
        default_position_angle_degrees: None,
        devices,
        auto_focus: None,
    });
}

/// Start calibrator-flats, then rp with it registered. rp's port is
/// reserved first (testing.md §5.1: a band port probed by connect, never
/// a bind-and-drop) so the provider's config can name rp's MCP URL
/// before rp exists.
async fn start_provider_then_rp(world: &mut CalibratorFlatsWorld) {
    let rp_port = bdd_infra::reserved_test_port();
    world.rp_port = Some(rp_port);

    let mut config = build_calibrator_flats_config(
        &format!("http://127.0.0.1:{rp_port}/mcp"),
        &world.store_path_string(),
    );
    for (key, value) in &world.provider_overrides {
        config[key] = value.clone();
    }
    let config_path = write_temp_config_file("calibrator-flats-config", &config).await;
    world.calibrator_flats = Some(ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await);

    let gate: serde_json::Map<String, Value> = PROVIDER_TOOLS
        .iter()
        .map(|tool| ((*tool).to_string(), Value::String("none".to_string())))
        .collect();
    world.plugin_configs.push(serde_json::json!({
        "name": PROVIDER_NAME,
        "type": "tool_provider",
        "mcp_server_url": format!("{}/mcp", world.calibrator_flats_url()),
        "gate": gate,
        "requires_tools": REQUIRED_RP_TOOLS,
    }));

    let rp_config = world.build_rp_config();
    world.rp = Some(start_rp(&rp_config).await);
    assert!(
        world.wait_for_rp_healthy().await,
        "rp did not become healthy within timeout"
    );
}

async fn read_safety_status(world: &mut CalibratorFlatsWorld) -> Value {
    ensure_mcp_client(world).await;
    world
        .mcp()
        .call_tool("get_safety_status", serde_json::json!({}))
        .await
        .expect("get_safety_status must answer whatever the conditions")
}

async fn wait_for_calibrator_state(target: i64, budget: Duration) {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let state = OmniSimHandle::calibrator_state()
            .await
            .expect("failed to read the simulator's calibrator state");
        if state == target {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the panel did not reach CalibratorState {target} within {budget:?} (last {state})"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The last successful tool result at a JSON pointer (`/trained/0/filter`).
fn result_at(world: &CalibratorFlatsWorld, pointer: &str) -> Value {
    let value = world
        .last_result()
        .as_ref()
        .unwrap_or_else(|e| panic!("the tool call failed: {e}"));
    value
        .pointer(pointer)
        .cloned()
        .unwrap_or_else(|| panic!("no {pointer} in the tool result: {value}"))
}
