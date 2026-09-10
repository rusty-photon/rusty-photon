//! BDD step definitions for the focus tools served through rp
//! (`focus_tools.feature`).
//!
//! The scenarios spawn three processes in this order: `OmniSim` (the
//! Alpaca simulator), focus-model (the tool provider under test,
//! started first because rp dials it at startup and it must answer
//! `tools/list` on its own), and rp with the provider registered — on
//! a port picked in advance, since the provider's config names rp's
//! MCP URL before rp exists. Tools are then called through rp's proxy
//! with the harness MCP client, exactly as a session-runner document
//! would.

use std::time::Duration;

use bdd_infra::rp_harness::{
    start_rp, write_temp_config_file, CameraConfig, CannedGuiding, FilterWheelConfig,
    FocuserConfig, GuiderConfig, GuiderStub, GuiderStubBehavior, McpTestClient, MountConfig,
    OmniSimHandle, OpticalTrainConfig, WebhookReceiver,
};
use bdd_infra::ServiceHandle;
use cucumber::{given, then, when};
use focus_model::store::{FocusRecord, FocusStore, LastGood};
use serde_json::Value;

use crate::world::{build_focus_model_config, FocusModelWorld};

/// The registration name rp knows the provider by.
const PROVIDER_NAME: &str = "focus-model";

/// The tools the provider offers; the registration ungates all six
/// (docs/services/focus-model.md § Registration in rp).
const PROVIDER_TOOLS: [&str; 6] = [
    "focus_train",
    "get_sweep_plan",
    "get_focus_model",
    "get_focus_runs",
    "set_focus_offsets",
    "reset_focus_model",
];

/// The rp tools the provider calls — the registration's `requires_tools`.
const REQUIRED_RP_TOOLS: [&str; 13] = [
    "get_train_info",
    "get_refocus_plan",
    "get_focuser_position",
    "get_focuser_temperature",
    "move_focuser",
    "get_filter",
    "set_filter",
    "capture",
    "measure_stars",
    "get_guiding_stats",
    "pause_guiding",
    "resume_guiding",
    "auto_focus",
];

/// The reference train: 500 mm at f/5.
const FOCAL_LENGTH_MM: f64 = 500.0;
const APERTURE_MM: f64 = 100.0;

/// The wheel's names in position order, as the rig configures them: a
/// seeded record carries them so it reads fresh against the train.
fn wheel_filters() -> Vec<String> {
    vec!["Luminance".to_string(), "Ha".to_string()]
}

// ---------------------------------------------------------------------------
// Given steps
// ---------------------------------------------------------------------------

#[given("a running Alpaca simulator")]
async fn running_alpaca_simulator(world: &mut FocusModelWorld) {
    ensure_omnisim(world).await;
}

#[given("rp's data_directory is pinned to a fresh tempdir")]
fn pin_data_directory(world: &mut FocusModelWorld) {
    world.data_dir = Some(tempfile::tempdir().expect("create rp's data directory"));
}

#[given(expr = "the focuser is configured with microns_per_step {float}")]
const fn focuser_microns_per_step(world: &mut FocusModelWorld, microns: f64) {
    world.focuser_microns_per_step = Some(microns);
}

#[given(expr = "the filter wheel's {string} filter is configured at {int} nm")]
fn filter_wavelength(world: &mut FocusModelWorld, filter: String, nanometres: i64) {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a wavelength in nanometres is a small integer in the feature file"
    )]
    let nanometres = nanometres as f64;
    world.filter_wavelengths_nm.push((filter, nanometres));
}

/// A provider knob pinned by the scenario, inside the train's block: a
/// value that parses as a number is sent as one (`max_attempts "1"`),
/// anything else as a string (`duration "3s"`).
#[given(expr = "the focus provider is configured for train {string} with {word} {string}")]
fn provider_train_knob(world: &mut FocusModelWorld, train_id: String, key: String, value: String) {
    world
        .train_overrides
        .push((train_id, key, json_scalar(&value)));
}

/// A top-level provider knob pinned by the scenario.
#[given(expr = "the focus provider is configured with {word} {string}")]
fn provider_knob(world: &mut FocusModelWorld, key: String, value: String) {
    world.provider_overrides.insert(key, json_scalar(&value));
}

#[given(expr = "a test webhook receiver subscribed to {string}")]
async fn webhook_receiver_subscribed_to(world: &mut FocusModelWorld, event_type: String) {
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
        "name": format!("test-event-plugin-{event_type}"),
        "type": "event",
        "webhook_url": url,
        "subscribes_to": [event_type]
    }));
}

#[given("a stub guider returning canned guiding stats")]
async fn stub_guider_canned(world: &mut FocusModelWorld) {
    let stub = GuiderStub::start(GuiderStubBehavior::Canned(CannedGuiding::default())).await;
    world.guider = Some(GuiderConfig::url_only(stub.url.clone()));
    world.guider_stub = Some(stub);
}

#[given("a stub guider reporting guiding inactive")]
async fn stub_guider_inactive(world: &mut FocusModelWorld) {
    let stub = GuiderStub::start(GuiderStubBehavior::Canned(CannedGuiding {
        guiding: false,
        ..CannedGuiding::default()
    }))
    .await;
    world.guider = Some(GuiderConfig::url_only(stub.url.clone()));
    world.guider_stub = Some(stub);
}

/// Seed the scenario's store before the provider opens it: a record
/// whose last good focus anchors the next sweep's prediction.
#[given(expr = "a stored focus model for train {string} with a last good position of {int}")]
async fn stored_model_with_last_good(world: &mut FocusModelWorld, train_id: String, position: i64) {
    let mut record = FocusRecord::new(
        &train_id,
        Some("main-focuser"),
        Some("main-cam"),
        Some(wheel_filters()),
    );
    record.set_last_good(LastGood {
        filter: Some("Luminance".to_string()),
        position: i32::try_from(position).expect("a focuser position fits in an i32"),
        temperature_c: None,
        hfr: 1.5,
        at: "2026-09-10T20:00:00Z".to_string(),
    });
    seed(world, record).await;
}

/// Seed a record whose camera no longer matches the train.
#[given(expr = "a stored focus model for train {string} trained on camera {string}")]
async fn stored_model_on_camera(world: &mut FocusModelWorld, train_id: String, camera_id: String) {
    let mut record = FocusRecord::new(
        &train_id,
        Some("main-focuser"),
        Some(&camera_id),
        Some(wheel_filters()),
    );
    record.set_last_good(LastGood {
        filter: Some("Luminance".to_string()),
        position: 24_000,
        temperature_c: None,
        hfr: 1.5,
        at: "2026-09-10T20:00:00Z".to_string(),
    });
    seed(world, record).await;
}

#[given("rp is running with a focus train on the simulator and focus-model registered as a tool provider")]
async fn rp_with_focus_train(world: &mut FocusModelWorld) {
    configure_rig(world, Some(APERTURE_MM));
    push_train(
        world,
        vec![
            "main-fw".to_string(),
            "main-focuser".to_string(),
            "main-cam".to_string(),
        ],
    );
    start_provider_then_rp(world).await;
}

#[given("rp is running with a focus train without an aperture and focus-model registered as a tool provider")]
async fn rp_with_train_without_aperture(world: &mut FocusModelWorld) {
    configure_rig(world, None);
    push_train(
        world,
        vec![
            "main-fw".to_string(),
            "main-focuser".to_string(),
            "main-cam".to_string(),
        ],
    );
    start_provider_then_rp(world).await;
}

#[given(
    "rp is running with a focus train sharing its focuser with an offline guiding train and focus-model registered as a tool provider"
)]
async fn rp_with_shared_focuser(world: &mut FocusModelWorld) {
    configure_rig(world, Some(APERTURE_MM));
    push_train(
        world,
        vec![
            "main-fw".to_string(),
            "main-focuser".to_string(),
            "main-cam".to_string(),
        ],
    );
    // An offline guide camera: the guiding train exists in the model —
    // which is what makes the shared focuser guide-coupled — without a
    // second simulator camera.
    world.cameras.push(CameraConfig {
        id: "guide-cam".to_string(),
        alpaca_url: "not-a-url".to_string(),
        device_number: 0,
        cooler_targets_c: Vec::new(),
    });
    world.optical_trains.push(OpticalTrainConfig {
        aperture_mm: None,
        id: "guide".to_string(),
        purpose: Some("guiding".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices: vec!["main-focuser".to_string(), "guide-cam".to_string()],
        auto_focus: None,
    });
    // Guiding is mount-scoped, so the guider block needs a mount; it
    // is never actuated by these scenarios.
    world.mount = Some(MountConfig {
        alpaca_url: "not-a-url".to_string(),
        device_number: 0,
        settle_after_slew: None,
    });
    start_provider_then_rp(world).await;
}

#[given("an MCP client connected to rp")]
async fn mcp_client_connected(world: &mut FocusModelWorld) {
    ensure_mcp_client(world).await;
}

#[given(expr = "the focuser is at position {int}")]
async fn focuser_is_at(world: &mut FocusModelWorld, expected: i64) {
    let position = read_focuser_position(world).await;
    assert_eq!(
        position, expected,
        "the simulator's focuser starts every scenario at its default position"
    );
}

// ---------------------------------------------------------------------------
// When steps
// ---------------------------------------------------------------------------

#[when(regex = r#"^the MCP client calls "([^"]+)" with (.+)$"#)]
async fn call_tool(world: &mut FocusModelWorld, tool: String, arguments: String) {
    ensure_mcp_client(world).await;
    let args: Value = serde_json::from_str(&arguments).expect("arguments must be JSON");
    world.last_tool_result = Some(world.mcp().call_tool(&tool, args).await);
}

#[when("the MCP client lists available tools")]
async fn list_tools(world: &mut FocusModelWorld) {
    ensure_mcp_client(world).await;
    let tools = world
        .mcp()
        .list_tools()
        .await
        .unwrap_or_else(|e| panic!("tools/list failed: {e}"));
    world.last_tool_list = Some(tools);
}

#[when(regex = r#"^a second MCP client starts "([^"]+)" with (.+) in the background$"#)]
fn start_tool_in_background(world: &mut FocusModelWorld, tool: String, arguments: String) {
    let args: Value = serde_json::from_str(&arguments).expect("arguments must be JSON");
    let url = world.rp_mcp_url();
    let tool_name = tool.clone();
    let handle = tokio::spawn(async move {
        let client = McpTestClient::connect(&url).await?;
        client.call_tool(&tool_name, args).await
    });
    world.background_calls.push((tool, handle));
}

/// Wait until the sweep has moved the focuser off its starting
/// position — the background call is past its pre-flight and mid-walk.
#[when(expr = "the focuser has moved away from position {int}")]
async fn focuser_has_moved(world: &mut FocusModelWorld, from: i64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if read_focuser_position(world).await != from {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the focuser never left {from}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Drop the most recently started background client mid-call. Aborting
/// the task drops its `McpTestClient`, which drops the HTTP request
/// still waiting on its response; rp cancels the in-flight proxied
/// call when that connection goes away and forwards
/// `notifications/cancelled` to the provider.
#[when("the second MCP client disconnects")]
async fn second_client_disconnects(world: &mut FocusModelWorld) {
    let (_tool, handle) = world
        .background_calls
        .pop()
        .expect("no background call was started in this scenario");
    handle.abort();
    let _ = handle.await;
}

// ---------------------------------------------------------------------------
// Then steps
// ---------------------------------------------------------------------------

#[then("the tool call should succeed")]
fn tool_call_succeeds(world: &mut FocusModelWorld) {
    if let Err(message) = world.last_result() {
        panic!("the tool call failed: {message}");
    }
}

#[then("the tool call should return an error")]
fn tool_call_errors(world: &mut FocusModelWorld) {
    if let Ok(value) = world.last_result() {
        panic!("the tool call succeeded, expected an error: {value}");
    }
}

#[then(expr = "the error message should contain {string}")]
fn error_message_contains(world: &mut FocusModelWorld, expected: String) {
    let message = world
        .last_result()
        .as_ref()
        .expect_err("the tool call succeeded, so there is no error message");
    assert!(
        message.contains(&expected),
        "error message {message:?} does not contain {expected:?}"
    );
}

/// One value of the last successful result, compared as JSON so a
/// scenario can pin a string, a number, a null or a boolean with one
/// step (`{string}` cannot carry the inner quotes a JSON value needs).
#[then(regex = r#"^the tool result at "([^"]+)" should be the JSON (.+)$"#)]
fn tool_result_json(world: &mut FocusModelWorld, pointer: String, expected: String) {
    let expected: Value = serde_json::from_str(&expected)
        .unwrap_or_else(|e| panic!("the expected value is not JSON: {expected}: {e}"));
    let actual = result_at(world, &pointer);
    assert!(
        json_values_match(&actual, &expected),
        "{pointer}: expected {expected}, got {actual}"
    );
}

#[then(expr = "the tool result {string} should have {int} entries")]
fn tool_result_len(world: &mut FocusModelWorld, pointer: String, expected: usize) {
    let value = result_at(world, &pointer);
    let list = value
        .as_array()
        .unwrap_or_else(|| panic!("{pointer} is not a list: {value}"));
    assert_eq!(list.len(), expected, "{pointer}: {value}");
}

#[then(expr = "the tool list should include {string}")]
fn tool_list_includes(world: &mut FocusModelWorld, tool: String) {
    let tools = world
        .last_tool_list
        .as_ref()
        .expect("no tool list was fetched in this scenario");
    assert!(tools.contains(&tool), "tool list lacks {tool}: {tools:?}");
}

#[then(expr = "the safety status should not list {string} as gated")]
async fn safety_status_does_not_list_gated(world: &mut FocusModelWorld, tool: String) {
    ensure_mcp_client(world).await;
    let status = world
        .mcp()
        .call_tool("get_safety_status", serde_json::json!({}))
        .await
        .expect("get_safety_status must answer whatever the conditions");
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
    world: &mut FocusModelWorld,
    count: usize,
    event_type: String,
) {
    assert!(
        world.wait_for_events(&event_type, count).await,
        "expected at least {count} '{event_type}' event(s) within timeout"
    );
}

#[then(expr = "the focuser should be back at position {int} within {int} seconds")]
async fn focuser_back_at(world: &mut FocusModelWorld, expected: i64, seconds: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let position = read_focuser_position(world).await;
        if position == expected {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the focuser never returned to {expected} (last {position})"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[then(expr = "the simulator's filter wheel should be at position {int}")]
async fn filter_wheel_at(world: &mut FocusModelWorld, expected: i64) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "get_filter",
            serde_json::json!({ "filter_wheel_id": "main-fw" }),
        )
        .await
        .expect("get_filter must answer");
    assert_eq!(
        result["position"].as_i64(),
        Some(expected),
        "filter wheel position: {result}"
    );
}

#[then("the stub guider should have received a pause request with full false")]
async fn stub_pause_request(world: &mut FocusModelWorld) {
    let mut pauses = world.guider().requests_to("/guiding/pause").await;
    let request = pauses
        .pop()
        .expect("the stub guider received no pause request");
    assert_eq!(
        request.get("full").and_then(Value::as_bool),
        Some(false),
        "full mismatch in {request}"
    );
}

#[then("the stub guider should have received a resume request")]
async fn stub_resume_request(world: &mut FocusModelWorld) {
    let resumes = world.guider().requests_to("/guiding/resume").await;
    assert!(!resumes.is_empty(), "the stub received no resume request");
}

#[then("the stub guider should not have received a pause request")]
async fn stub_no_pause_request(world: &mut FocusModelWorld) {
    let pauses = world.guider().requests_to("/guiding/pause").await;
    assert!(pauses.is_empty(), "the stub received {pauses:?}");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A scenario's scalar knob: a number when it parses as one, a string
/// otherwise.
fn json_scalar(value: &str) -> Value {
    value
        .parse::<u64>()
        .map(Value::from)
        .or_else(|_| value.parse::<f64>().map(Value::from))
        .unwrap_or_else(|_| Value::String(value.to_string()))
}

/// Compare two JSON values, numbers by their `f64` value so `20` and
/// `20.0` match.
fn json_values_match(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Number(a), Value::Number(b)) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => (a - b).abs() < 1e-9,
            _ => a == b,
        },
        _ => actual == expected,
    }
}

async fn ensure_omnisim(world: &mut FocusModelWorld) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
}

pub async fn ensure_mcp_client(world: &mut FocusModelWorld) {
    if world.mcp_client.is_none() {
        let url = world.rp_mcp_url();
        let client = McpTestClient::connect(&url)
            .await
            .unwrap_or_else(|e| panic!("failed to connect the MCP client to rp: {e}"));
        world.mcp_client = Some(client);
    }
}

/// Write `record` into the scenario's store, then release redb's file
/// lock before the provider process opens the same file.
async fn seed(world: &mut FocusModelWorld, record: FocusRecord) {
    let store = FocusStore::open(world.store_path())
        .await
        .expect("open the scenario's store for seeding");
    store.put(record).await.expect("seed the focus record");
}

/// The reference rig: camera `main-cam`, focuser `main-focuser` and
/// filter wheel `main-fw` (Luminance, Ha), all on the simulator.
fn configure_rig(world: &mut FocusModelWorld, aperture_mm: Option<f64>) {
    let alpaca_url = world.omnisim_url();
    world.cameras.push(CameraConfig {
        id: "main-cam".to_string(),
        alpaca_url: alpaca_url.clone(),
        device_number: 0,
        cooler_targets_c: Vec::new(),
    });
    world.filter_wheels.push(FilterWheelConfig {
        id: "main-fw".to_string(),
        alpaca_url: alpaca_url.clone(),
        device_number: 0,
        filters: vec!["Luminance".to_string(), "Ha".to_string()],
        wavelengths_nm: world.filter_wavelengths_nm.clone(),
    });
    world.focusers.push(FocuserConfig {
        id: "main-focuser".to_string(),
        alpaca_url,
        device_number: 0,
        min_position: None,
        max_position: None,
        backlash: None,
        microns_per_step: world.focuser_microns_per_step,
    });
    world.aperture_mm_for_train = aperture_mm;
}

fn push_train(world: &mut FocusModelWorld, devices: Vec<String>) {
    world.optical_trains.push(OpticalTrainConfig {
        aperture_mm: world.aperture_mm_for_train,
        id: "main".to_string(),
        purpose: Some("imaging".to_string()),
        focal_length_mm: Some(FOCAL_LENGTH_MM),
        default_position_angle_degrees: None,
        devices,
        auto_focus: None,
    });
}

/// Start focus-model, then rp with it registered. rp's port is
/// reserved first (testing.md §5.1: a band port probed by connect,
/// never a bind-and-drop) so the provider's config can name rp's MCP
/// URL before rp exists.
async fn start_provider_then_rp(world: &mut FocusModelWorld) {
    let rp_port = bdd_infra::reserved_test_port();
    world.rp_port = Some(rp_port);

    let mut config = build_focus_model_config(
        &format!("http://127.0.0.1:{rp_port}/mcp"),
        &world.store_path_string(),
    );
    for (key, value) in &world.provider_overrides {
        config[key] = value.clone();
    }
    for (train_id, key, value) in &world.train_overrides {
        config["trains"][train_id][key] = value.clone();
    }
    let config_path = write_temp_config_file("focus-model-config", &config).await;
    world.focus_model = Some(ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await);

    let gate: serde_json::Map<String, Value> = PROVIDER_TOOLS
        .iter()
        .map(|tool| ((*tool).to_string(), Value::String("none".to_string())))
        .collect();
    world.plugin_configs.push(serde_json::json!({
        "name": PROVIDER_NAME,
        "type": "tool_provider",
        "mcp_server_url": format!("{}/mcp", world.focus_model_url()),
        "gate": gate,
        "focus_tools": { "focus_train": "train_id" },
        "requires_tools": REQUIRED_RP_TOOLS,
    }));

    let rp_config = world.build_rp_config();
    world.rp = Some(start_rp(&rp_config).await);
    assert!(
        world.wait_for_rp_healthy().await,
        "rp did not become healthy within timeout"
    );
}

/// The focuser's position, read through rp like any other client.
async fn read_focuser_position(world: &mut FocusModelWorld) -> i64 {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "get_focuser_position",
            serde_json::json!({ "focuser_id": "main-focuser" }),
        )
        .await
        .expect("get_focuser_position must answer");
    result["position"]
        .as_i64()
        .unwrap_or_else(|| panic!("no position in {result}"))
}

/// The last successful tool result at a JSON pointer.
fn result_at(world: &FocusModelWorld, pointer: &str) -> Value {
    let value = world
        .last_result()
        .as_ref()
        .unwrap_or_else(|e| panic!("the tool call failed: {e}"));
    value
        .pointer(pointer)
        .cloned()
        .unwrap_or_else(|| panic!("no {pointer} in the tool result: {value}"))
}
