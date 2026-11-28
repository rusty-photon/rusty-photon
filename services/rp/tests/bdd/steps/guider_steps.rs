//! BDD step definitions for the guiding MCP tools (`start_guiding`,
//! `stop_guiding`, `dither`, `pause_guiding`, `resume_guiding`,
//! `get_guiding_stats`) and the safety enforcer's stop-guiding /
//! park-mount handling.
//!
//! Shared steps live in `tool_steps.rs` (`the MCP client lists
//! available tools`, `the tool call should return an error`, ...) and
//! `event_steps.rs` (webhook receiver givens and payload assertions).
//! The guider service is `bdd_infra::rp_harness::GuiderStub`, an
//! in-process double of its HTTP contract.

use std::time::Duration;

use cucumber::{given, then, when};
use serde_json::{Map, Value};

use bdd_infra::rp_harness::{GuiderConfig, GuiderStub, MountConfig, OpticalTrainConfig};

use crate::steps::device_identity_steps::simulator_unique_id;
use crate::steps::event_steps::json_values_match;
use crate::steps::rotator_steps::add_offline_camera;
use crate::steps::tool_steps::{add_camera, ensure_mcp_client, ensure_omnisim, start_rp};
use crate::world::RpWorld;

// --- Given steps: the stub guider ------------------------------------

#[given(expr = "a stub guider in state {string}")]
async fn stub_guider_in_state(world: &mut RpWorld, state: String) {
    start_stub_guider(world, &state).await;
}

/// The simulator's camera 0 is the one every simulator rig binds as
/// `main-cam`, so the stub then names the guiding train's camera.
#[given("the stub guider's camera is the simulator's camera")]
async fn stub_camera_is_simulators(world: &mut RpWorld) {
    let camera_id = simulator_unique_id(world, "Camera").await;
    guider_stub(world).set_camera_unique_id(Some(&camera_id));
}

#[given(
    expr = "the stub guider answers {string} requests with error code {string} and message {string}"
)]
fn stub_guider_fails(world: &mut RpWorld, path: String, code: String, message: String) {
    guider_stub(world).fail(&path, &code, &message);
}

#[given(expr = "the stub guider reports a last stop of reason {string}")]
fn stub_guider_last_stop(world: &mut RpWorld, reason: String) {
    guider_stub(world).set_last_stop(&reason);
}

#[given(regex = r#"^the stub guider reports the (camera|mount) unique_id "([^"]*)"$"#)]
fn stub_guider_reports_unique_id(world: &mut RpWorld, device: String, unique_id: String) {
    set_stub_unique_id(world, &device, Some(&unique_id));
}

/// No identity for `device`: `null` for "no", the empty string for "a
/// blank".
#[given(regex = r"^the stub guider reports (no|a blank) (camera|mount) unique_id$")]
fn stub_guider_reports_no_unique_id(world: &mut RpWorld, kind: String, device: String) {
    let unique_id = match kind.as_str() {
        "no" => None,
        _ => Some(""),
    };
    set_stub_unique_id(world, &device, unique_id);
}

#[given(expr = "the stub guider settles after {int} ms")]
fn stub_guider_settles_after(world: &mut RpWorld, delay_ms: u64) {
    guider_stub(world).set_settle_delay(Duration::from_millis(delay_ms));
}

/// A key of rp's guiding block given as raw JSON (`"2s"`, `0.8`, ...),
/// written over whatever the rig step emits for it.
#[given(regex = r#"^rp's guiding config sets "([^"]+)" to the JSON (.+)$"#)]
fn guiding_config_sets(world: &mut RpWorld, key: String, value: String) {
    let value: Value = serde_json::from_str(&value)
        .unwrap_or_else(|e| panic!("the guiding config value {value:?} is not JSON: {e}"));
    guider_config(world).overrides.push((key, value));
}

/// Serve the guider's contract from a [`GuiderStub`] in `state`, and
/// point rp's guiding block at it.
///
/// With the simulator running, the stub's device report names the
/// simulator's mount: the mount `start_rp` gives a rig that names no
/// mount of its own, so the device check before a start passes on the
/// mount. Its camera is one of its own, which is none of rp's.
pub async fn start_stub_guider(world: &mut RpWorld, state: &str) {
    let stub = GuiderStub::start(state).await;
    if world.omnisim.is_some() {
        let mount_id = simulator_unique_id(world, "Telescope").await;
        stub.set_mount_unique_id(Some(&mount_id));
    }
    world.guider = Some(GuiderConfig::url_only(stub.url.clone()));
    world.guider_stub = Some(stub);
}

// --- Given steps: composite "rp running with ..." -------------------

#[given(
    expr = "rp is running with a camera on the simulator and guider settle pixels {float} time {string} timeout {string}"
)]
async fn rp_with_camera_and_guider_settle(
    world: &mut RpWorld,
    pixels: f64,
    time: String,
    timeout: String,
) {
    ensure_omnisim(world).await;
    add_camera(world);
    // Reuse the URL the prior `Given a stub guider ...` step placed
    // on the world, layering on the operator-set settle defaults.
    let guider = guider_config(world);
    guider.settle_pixels = Some(pixels);
    guider.settle_time = Some(parse_humantime(&time));
    guider.settle_timeout = Some(parse_humantime(&timeout));
    start_rp(world).await;
}

#[given(expr = "rp is running with a camera on the simulator and guider dither_pixels {float}")]
async fn rp_with_camera_and_guider_dither(world: &mut RpWorld, dither_pixels: f64) {
    ensure_omnisim(world).await;
    add_camera(world);
    guider_config(world).dither_pixels = Some(dither_pixels);
    start_rp(world).await;
}

#[given("rp is running with a camera on the simulator and a guider pointing at an unbound port")]
async fn rp_with_camera_and_unbound_guider(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    // Port 1 is reserved and reliably unbound on Linux dev hosts /
    // CI runners, mirroring the plate-solver unreachable scenario.
    world.guider = Some(GuiderConfig::url_only("http://127.0.0.1:1".to_string()));
    start_rp(world).await;
}

/// The simulator camera in no train, beside two imaging trains of an
/// offline camera each: `main_px` has no one imaging train to take its
/// scale from.
#[given("rp is running with a camera on the simulator and two offline imaging trains")]
async fn rp_with_camera_and_two_imaging_trains(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    for id in ["first", "second"] {
        add_offline_camera(world, &format!("{id}-cam"));
        world.optical_trains.push(OpticalTrainConfig {
            telescope: None,
            aperture_mm: None,
            obstruction_mm: None,
            id: id.to_string(),
            purpose: Some("imaging".to_string()),
            focal_length_mm: Some(500.0),
            default_position_angle_degrees: None,
            devices: vec![format!("{id}-cam")],
            auto_focus: None,
        });
    }
    start_rp(world).await;
}

/// rp's mount is the simulator's, and its guiding train "guide" holds
/// one camera rp cannot reach, so rp binds no identity for it.
#[given("rp is running with a mount on the simulator and an offline camera in a guiding train")]
async fn rp_with_mount_and_offline_guiding_camera(world: &mut RpWorld) {
    expect_guiding_config(world);
    ensure_omnisim(world).await;
    world.mount = Some(MountConfig {
        alpaca_url: world.omnisim_url(),
        device_number: 0,
        settle_after_slew: None,
    });
    add_offline_camera(world, "guide-cam");
    world.optical_trains.push(OpticalTrainConfig {
        telescope: None,
        aperture_mm: None,
        obstruction_mm: None,
        id: "guide".to_string(),
        purpose: Some("guiding".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices: vec!["guide-cam".to_string()],
        auto_focus: None,
    });
    start_rp(world).await;
}

#[given(
    expr = "rp is running with the simulator camera in an imaging train with focal length {float}"
)]
async fn rp_with_simulator_camera_in_imaging_train(world: &mut RpWorld, focal_length_mm: f64) {
    ensure_omnisim(world).await;
    add_camera(world);
    push_imaging_train(world, focal_length_mm);
    start_rp(world).await;
}

/// The imaging train's camera, and the mount the guiding block hangs
/// from, are both out of reach: the scenario runs no simulator.
#[given(
    expr = "rp is running with an offline camera in an imaging train with focal length {float}"
)]
async fn rp_with_offline_camera_in_imaging_train(world: &mut RpWorld, focal_length_mm: f64) {
    expect_guiding_config(world);
    add_offline_camera(world, "main-cam");
    // Port 1 is reserved and reliably unbound on Linux dev hosts and CI
    // runners.
    world.mount = Some(MountConfig {
        alpaca_url: "http://127.0.0.1:1".to_string(),
        device_number: 0,
        settle_after_slew: None,
    });
    push_imaging_train(world, focal_length_mm);
    start_rp(world).await;
}

/// Imaging train "main" of the roster's `main-cam` alone.
fn push_imaging_train(world: &mut RpWorld, focal_length_mm: f64) {
    world.optical_trains.push(OpticalTrainConfig {
        telescope: None,
        aperture_mm: None,
        obstruction_mm: None,
        id: "main".to_string(),
        purpose: Some("imaging".to_string()),
        focal_length_mm: Some(focal_length_mm),
        default_position_angle_degrees: None,
        devices: vec!["main-cam".to_string()],
        auto_focus: None,
    });
}

// --- When steps -----------------------------------------------------

#[when("the MCP client calls \"start_guiding\" with no arguments")]
async fn call_start_guiding_no_args(world: &mut RpWorld) {
    call_guider_tool(world, "start_guiding", Map::new()).await;
}

#[when("the MCP client calls \"start_guiding\" with recalibrate true")]
async fn call_start_guiding_recalibrate(world: &mut RpWorld) {
    let mut params = Map::new();
    params.insert("recalibrate".to_string(), Value::Bool(true));
    call_guider_tool(world, "start_guiding", params).await;
}

#[when(
    expr = "the MCP client calls \"start_guiding\" with settle_pixels {float} and settle_timeout {string}"
)]
async fn call_start_guiding_settle_override(world: &mut RpWorld, pixels: f64, timeout: String) {
    let mut params = Map::new();
    params.insert("settle_pixels".to_string(), serde_json::json!(pixels));
    params.insert("settle_timeout".to_string(), Value::String(timeout));
    call_guider_tool(world, "start_guiding", params).await;
}

#[when("the MCP client calls \"stop_guiding\"")]
async fn call_stop_guiding(world: &mut RpWorld) {
    call_guider_tool(world, "stop_guiding", Map::new()).await;
}

#[when(expr = "the MCP client calls \"dither\" with pixels {float} and ra_only {word}")]
async fn call_dither(world: &mut RpWorld, pixels: f64, ra_only: String) {
    let mut params = Map::new();
    params.insert("pixels".to_string(), serde_json::json!(pixels));
    params.insert("ra_only".to_string(), Value::Bool(parse_bool(&ra_only)));
    call_guider_tool(world, "dither", params).await;
}

#[when("the MCP client calls \"dither\" with no arguments")]
async fn call_dither_no_args(world: &mut RpWorld) {
    call_guider_tool(world, "dither", Map::new()).await;
}

#[when(expr = "the MCP client calls \"dither\" with pixels {float} and unit {string}")]
async fn call_dither_with_unit(world: &mut RpWorld, pixels: f64, unit: String) {
    let mut params = Map::new();
    params.insert("pixels".to_string(), serde_json::json!(pixels));
    params.insert("unit".to_string(), Value::String(unit));
    call_guider_tool(world, "dither", params).await;
}

#[when(expr = "the MCP client calls \"dither\" with unit {string} and no pixels")]
async fn call_dither_unit_only(world: &mut RpWorld, unit: String) {
    let mut params = Map::new();
    params.insert("unit".to_string(), Value::String(unit));
    call_guider_tool(world, "dither", params).await;
}

#[when("the MCP client calls \"pause_guiding\" with full true")]
async fn call_pause_guiding_full(world: &mut RpWorld) {
    let mut params = Map::new();
    params.insert("full".to_string(), Value::Bool(true));
    call_guider_tool(world, "pause_guiding", params).await;
}

#[when("the MCP client calls \"resume_guiding\"")]
async fn call_resume_guiding(world: &mut RpWorld) {
    call_guider_tool(world, "resume_guiding", Map::new()).await;
}

#[when("the MCP client calls \"get_guiding_stats\"")]
async fn call_get_guiding_stats(world: &mut RpWorld) {
    call_guider_tool(world, "get_guiding_stats", Map::new()).await;
}

#[when(expr = "the MCP client calls the guider tool {string} with empty arguments")]
async fn call_named_guider_tool(world: &mut RpWorld, tool: String) {
    call_guider_tool(world, &tool, Map::new()).await;
}

// --- When steps: what happens to the stub guider ---------------------

/// A guider that restarts: `idle`, no latest stop, a new epoch.
#[when("the stub guider restarts")]
fn stub_guider_restarts(world: &mut RpWorld) {
    guider_stub(world).restart();
}

/// From now on the stub records every request and answers none.
#[when("the stub guider stops answering")]
fn stub_guider_stops_answering(world: &mut RpWorld) {
    guider_stub(world).stop_answering();
}

// --- Then steps: tool result ----------------------------------------

#[then(expr = "the guider result should contain {string} with value {string}")]
fn guider_result_string_field(world: &mut RpWorld, field: String, expected: String) {
    let result = last_guider_result(world);
    assert_eq!(
        result.get(&field).and_then(|v| v.as_str()),
        Some(expected.as_str()),
        "field '{field}' mismatch in {result}"
    );
}

#[then(expr = "the guider result should contain {string} with number {float}")]
fn guider_result_number_field(world: &mut RpWorld, field: String, expected: f64) {
    let result = last_guider_result(world);
    let actual = result
        .get(&field)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("expected numeric field '{field}' in {result}"));
    assert!(
        (actual - expected).abs() < 1e-9,
        "field '{field}': expected {expected}, got {actual}"
    );
}

/// A regex sibling of the string and number steps, for `true`,
/// `false`, `null` and other JSON values. Numbers compare by value.
#[then(regex = r#"^the guider result should contain "([^"]+)" with the JSON (.+)$"#)]
fn guider_result_json_field(world: &mut RpWorld, field: String, expected: String) {
    let expected: Value = serde_json::from_str(&expected)
        .unwrap_or_else(|e| panic!("the expected value {expected:?} is not JSON: {e}"));
    let result = last_guider_result(world);
    let actual = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected field '{field}' in {result}"));
    assert!(
        json_values_match(actual, &expected),
        "field '{field}': expected {expected}, got {actual} in {result}"
    );
}

#[then(expr = "the guider result should not contain {string}")]
fn guider_result_lacks_field(world: &mut RpWorld, field: String) {
    let result = last_guider_result(world);
    assert!(
        result.get(&field).is_none(),
        "expected no field '{field}' in {result}"
    );
}

#[then(expr = "the guider result's {string} should carry the reason {string}")]
fn guider_result_carries_reason(world: &mut RpWorld, field: String, reason: String) {
    let result = last_guider_result(world);
    let object = result
        .get(&field)
        .filter(|value| value.is_object())
        .unwrap_or_else(|| panic!("expected an object '{field}' in {result}"));
    assert_eq!(
        object.get("reason").and_then(Value::as_str),
        Some(reason.as_str()),
        "'{field}' reason mismatch in {result}"
    );
}

/// The id the scenario's rp binds for the simulator's camera or mount
/// appears in the tool call's error.
#[then(regex = r"^the error message should name the simulator (camera|mount)'s unique id$")]
async fn error_names_simulator_unique_id(world: &mut RpWorld, device: String) {
    let device_type = match device.as_str() {
        "camera" => "Camera",
        _ => "Telescope",
    };
    let unique_id = simulator_unique_id(world, device_type).await;
    let result = world.last_tool_result.as_ref().expect("no tool result");
    let message = result
        .as_ref()
        .expect_err("expected an error but got success");
    assert!(
        message.contains(&unique_id),
        "expected the error to name the simulator {device}'s unique id '{unique_id}', got: '{message}'"
    );
}

// --- Then steps: stub request assertions -----------------------------

#[then(expr = "the stub guider should have received exactly {int} {string} request(s)")]
fn stub_received_exactly(world: &mut RpWorld, count: usize, path: String) {
    let requests = guider_stub(world).requests_to(&path);
    assert_eq!(
        requests.len(),
        count,
        "expected exactly {count} requests to '{path}', got {requests:?}"
    );
}

/// The requests to the comma-separated paths, in order of arrival, are
/// exactly those paths once each, in the order given; requests to other
/// paths may fall between them.
#[then(expr = "the stub guider should have received the requests {string} in that order")]
fn stub_received_requests_in_order(world: &mut RpWorld, paths: String) {
    let expected: Vec<&str> = paths.split(',').map(str::trim).collect();
    let received: Vec<String> = guider_stub(world)
        .requests()
        .into_iter()
        .map(|request| request.path)
        .filter(|path| expected.contains(&path.as_str()))
        .collect();
    assert_eq!(
        received,
        expected,
        "the requests to {expected:?}, in order of arrival; all: {:?}",
        guider_stub(world).requests()
    );
}

#[then(expr = "the stub guider should have received a {string} request within {int} seconds")]
async fn stub_received_request_within(world: &mut RpWorld, path: String, seconds: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    while guider_stub(world).requests_to(&path).is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the stub guider received no '{path}' request within {seconds}s; all: {:?}",
            guider_stub(world).requests()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Watches the whole window for a request to `path` the stub had not
/// received when the step began (testing.md § 6.9).
#[then(expr = "the stub guider should receive no {string} request within {int} ms")]
async fn stub_receives_no_request_within(world: &mut RpWorld, path: String, window_ms: u64) {
    let before = guider_stub(world).requests_to(&path).len();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(window_ms);
    while tokio::time::Instant::now() < deadline {
        let now = guider_stub(world).requests_to(&path).len();
        assert_eq!(
            now, before,
            "the stub guider received a '{path}' request inside the {window_ms} ms window"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[then("the stub guider should have received a start request without a settle override")]
fn stub_start_without_settle(world: &mut RpWorld) {
    let request = last_stub_request_to(world, "/guiding/start");
    assert!(
        request.get("settle").is_none(),
        "expected no settle override, got: {request}"
    );
}

#[then(
    expr = "the stub guider should have received a start request with settle pixels {float} time {string} timeout {string}"
)]
fn stub_start_with_settle(world: &mut RpWorld, pixels: f64, time: String, timeout: String) {
    let request = last_stub_request_to(world, "/guiding/start");
    let settle = request
        .get("settle")
        .unwrap_or_else(|| panic!("expected a settle override, got: {request}"));
    assert_eq!(
        settle.get("pixels").and_then(serde_json::Value::as_f64),
        Some(pixels),
        "settle.pixels mismatch in {request}"
    );
    assert_eq!(
        settle.get("time").and_then(|v| v.as_str()),
        Some(time.as_str()),
        "settle.time mismatch in {request}"
    );
    assert_eq!(
        settle.get("timeout").and_then(|v| v.as_str()),
        Some(timeout.as_str()),
        "settle.timeout mismatch in {request}"
    );
}

#[then("the stub guider should have received a start request with recalibrate true")]
fn stub_start_with_recalibrate(world: &mut RpWorld) {
    let request = last_stub_request_to(world, "/guiding/start");
    assert_eq!(
        request
            .get("recalibrate")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "recalibrate mismatch in {request}"
    );
}

#[then(
    expr = "the stub guider should have received a dither request with amount_px {float} and ra_only {word}"
)]
fn stub_dither_request(world: &mut RpWorld, amount_px: f64, ra_only: String) {
    let request = last_stub_request_to(world, "/dither");
    assert_eq!(
        request.get("amount_px").and_then(serde_json::Value::as_f64),
        Some(amount_px),
        "amount_px mismatch in {request}"
    );
    assert_eq!(
        request.get("ra_only").and_then(serde_json::Value::as_bool),
        Some(parse_bool(&ra_only)),
        "ra_only mismatch in {request}"
    );
}

#[then(
    expr = "the stub guider should have received a dither request with amount_arcsec {float} and ra_only {word}"
)]
fn stub_dither_request_arcsec(world: &mut RpWorld, amount_arcsec: f64, ra_only: String) {
    let request = last_stub_request_to(world, "/dither");
    assert_arcsec_dither(&request, amount_arcsec);
    assert_eq!(
        request.get("ra_only").and_then(Value::as_bool),
        Some(parse_bool(&ra_only)),
        "ra_only mismatch in {request}"
    );
}

/// `pixels` main-camera pixels at the imaging train's scale,
/// `206.265 × PixelSizeX / focal_length_mm`, with the pixel size read
/// from the simulator's camera.
#[then(
    expr = "the stub guider should have received a dither request with amount_arcsec equal to {float} pixels at the imaging train's {float} mm pixel scale"
)]
async fn stub_dither_request_main_px(world: &mut RpWorld, pixels: f64, focal_length_mm: f64) {
    let url = format!("{}/api/v1/camera/0/pixelsizex", world.omnisim_url());
    let body: Value = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect("failed to read the simulator camera's pixel size")
        .json()
        .await
        .expect("the pixel-size answer is not JSON");
    let pixel_size_x_um = body
        .get("Value")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("no numeric Value in the pixel-size answer: {body}"));
    let expected = pixels * 206.265 * pixel_size_x_um / focal_length_mm;
    let request = last_stub_request_to(world, "/dither");
    assert_arcsec_dither(&request, expected);
}

/// A dither body that names its amount in arcseconds alone.
fn assert_arcsec_dither(request: &Value, expected_arcsec: f64) {
    let actual = request
        .get("amount_arcsec")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("no numeric amount_arcsec in {request}"));
    assert!(
        (actual - expected_arcsec).abs() < 1e-9,
        "amount_arcsec: expected {expected_arcsec}, got {actual} in {request}"
    );
    assert!(
        !request.get("amount_px").is_some_and(Value::is_number),
        "a dither in arcseconds names no amount_px: {request}"
    );
}

#[then("the stub guider should have received a stop request")]
fn stub_stop_request(world: &mut RpWorld) {
    let stops = guider_stub(world).requests_to("/guiding/stop");
    assert!(!stops.is_empty(), "the stub received no stop request");
}

#[then("the stub guider should have received a pause request with full true")]
fn stub_pause_request_full(world: &mut RpWorld) {
    let request = last_stub_request_to(world, "/guiding/pause");
    assert_eq!(
        request.get("full").and_then(serde_json::Value::as_bool),
        Some(true),
        "full mismatch in {request}"
    );
}

#[then("the stub guider should have received a resume request")]
fn stub_resume_request(world: &mut RpWorld) {
    let resumes = guider_stub(world).requests_to("/guiding/resume");
    assert!(!resumes.is_empty(), "the stub received no resume request");
}

// --- Then steps: safety integration ----------------------------------

#[then(expr = "the stub guider should have received a stop request within {int} seconds")]
async fn stub_stop_request_within(world: &mut RpWorld, seconds: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        if !guider_stub(world).requests_to("/guiding/stop").is_empty() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the stub guider received no stop request within {seconds}s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[then(expr = "the mount should report parked on the simulator within {int} seconds")]
async fn mount_parked_on_simulator(world: &mut RpWorld, seconds: u64) {
    // Read AtPark straight from OmniSim's Alpaca API rather than
    // through rp: the check must not depend on rp's own mount reads
    // (ungated, but the transition's park is what is under test).
    let url = format!("{}/api/v1/telescope/0/atpark", world.omnisim_url());
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let at_park = client
            .get(&url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .ok();
        if let Some(resp) = at_park {
            if let Ok(body) = resp.json::<Value>().await {
                if body.get("Value").and_then(serde_json::Value::as_bool) == Some(true) {
                    return;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the simulator mount never reported AtPark within {seconds}s"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[then("the mount should report unparked on the simulator")]
async fn mount_unparked_on_simulator(world: &mut RpWorld) {
    // Read straight from OmniSim, as the parked check above does.
    let url = format!("{}/api/v1/telescope/0/atpark", world.omnisim_url());
    let body: Value = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .expect("the simulator answers AtPark")
        .json()
        .await
        .expect("AtPark answers JSON");
    assert_eq!(
        body.get("Value").and_then(serde_json::Value::as_bool),
        Some(false),
        "the simulator mount reports parked: {body}"
    );
}

// --- Helpers --------------------------------------------------------

pub async fn call_guider_tool(world: &mut RpWorld, tool: &str, params: Map<String, Value>) {
    ensure_mcp_client(world).await;

    let result = world.mcp().call_tool(tool, Value::Object(params)).await;

    match &result {
        Ok(v) => world.last_guider_result = Some(v.clone()),
        Err(_) => world.last_guider_result = None,
    }
    world.last_tool_result = Some(result);
}

pub const fn last_guider_result(world: &RpWorld) -> &Value {
    world
        .last_guider_result
        .as_ref()
        .expect("no successful guider tool result recorded — When step missing or tool errored?")
}

/// The scenario's stub guider.
pub const fn guider_stub(world: &RpWorld) -> &GuiderStub {
    world
        .guider_stub
        .as_ref()
        .expect("no guider stub registered for this scenario")
}

/// Set the `unique_id` the stub reports for `device` (`camera` or
/// `mount`).
fn set_stub_unique_id(world: &RpWorld, device: &str, unique_id: Option<&str>) {
    let stub = guider_stub(world);
    match device {
        "camera" => stub.set_camera_unique_id(unique_id),
        _ => stub.set_mount_unique_id(unique_id),
    }
}

/// A rig step that leaves the guiding block to a prior `Given a stub
/// guider ...` step.
fn expect_guiding_config(world: &RpWorld) {
    assert!(
        world.guider.is_some(),
        "this step expects a prior 'Given a stub guider ...' step to set the URL"
    );
}

/// The guiding block a prior `Given a stub guider ...` step set, for a
/// rig step to add its config keys to.
const fn guider_config(world: &mut RpWorld) -> &mut GuiderConfig {
    world
        .guider
        .as_mut()
        .expect("this step expects a prior 'Given a stub guider ...' step to set the URL")
}

/// The most recent request body the stub received on `path`.
fn last_stub_request_to(world: &RpWorld, path: &str) -> Value {
    let mut requests = guider_stub(world).requests_to(path);
    requests
        .pop()
        .unwrap_or_else(|| panic!("stub guider received no request to {path}"))
}

fn parse_humantime(s: &str) -> Duration {
    humantime::parse_duration(s)
        .unwrap_or_else(|e| panic!("scenario used an invalid humantime string {s:?}: {e}"))
}

fn parse_bool(s: &str) -> bool {
    match s {
        "true" => true,
        "false" => false,
        other => panic!("expected true|false, got {other}"),
    }
}
