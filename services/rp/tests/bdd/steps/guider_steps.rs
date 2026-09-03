//! BDD step definitions for the guiding MCP tools (`start_guiding`,
//! `stop_guiding`, `dither`, `pause_guiding`, `resume_guiding`,
//! `get_guiding_stats`) and the safety enforcer's stop-guiding /
//! park-mount handling.
//!
//! Shared steps live in `tool_steps.rs` (`the MCP client lists
//! available tools`, `the tool call should return an error`, ...) and
//! `event_steps.rs` (webhook receiver givens and payload assertions).
//! The guider service is stubbed in-process by
//! `bdd_infra::rp_harness::GuiderStub`.

use std::time::Duration;

use cucumber::{given, then, when};
use serde_json::{Map, Value};

use bdd_infra::rp_harness::{
    CannedGuiding, GuiderConfig, GuiderStub, GuiderStubBehavior, MountConfig, OpticalTrainConfig,
};

use crate::steps::tool_steps::{add_camera, ensure_mcp_client, ensure_omnisim, start_rp};
use crate::world::RpWorld;

// --- Given steps: stub server lifecycle ------------------------------

#[given("a stub guider returning canned guiding stats")]
async fn stub_guider_canned(world: &mut RpWorld) {
    let stub = GuiderStub::start(GuiderStubBehavior::Canned(CannedGuiding::default())).await;
    world.guider = Some(GuiderConfig::url_only(stub.url.clone()));
    world.guider_stub = Some(stub);
}

#[given(expr = "a stub guider returning error code {string} with message {string}")]
async fn stub_guider_error(world: &mut RpWorld, code: String, message: String) {
    let stub = GuiderStub::start(GuiderStubBehavior::Error { code, message }).await;
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
    let mut guider = world
        .guider
        .clone()
        .expect("this step expects a prior 'Given a stub guider ...' step to set the URL");
    guider.settle_pixels = Some(pixels);
    guider.settle_time = Some(parse_humantime(&time));
    guider.settle_timeout = Some(parse_humantime(&timeout));
    world.guider = Some(guider);
    start_rp(world).await;
}

#[given(expr = "rp is running with a camera on the simulator and guider dither_pixels {float}")]
async fn rp_with_camera_and_guider_dither(world: &mut RpWorld, dither_pixels: f64) {
    ensure_omnisim(world).await;
    add_camera(world);
    let mut guider = world
        .guider
        .clone()
        .expect("this step expects a prior 'Given a stub guider ...' step to set the URL");
    guider.dither_pixels = Some(dither_pixels);
    world.guider = Some(guider);
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

#[given("a stub guider reporting guiding inactive")]
async fn stub_guider_not_guiding(world: &mut RpWorld) {
    let stub = GuiderStub::start(GuiderStubBehavior::Canned(CannedGuiding {
        guiding: false,
        ..CannedGuiding::default()
    }))
    .await;
    world.guider = Some(GuiderConfig::url_only(stub.url.clone()));
    world.guider_stub = Some(stub);
}

// --- Given steps: dither-unit train compositions ---------------------

#[given(
    expr = "rp is running with the simulator camera in a guiding train with focal length {float}"
)]
async fn rp_with_camera_in_guiding_train(world: &mut RpWorld, focal_length_mm: f64) {
    ensure_omnisim(world).await;
    add_camera(world);
    push_guiding_train_for_camera(world, Some(focal_length_mm));
    start_rp(world).await;
}

#[given("rp is running with the simulator camera in a guiding train without a focal length")]
async fn rp_with_camera_in_guiding_train_no_focal(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    push_guiding_train_for_camera(world, None);
    start_rp(world).await;
}

/// A disconnected guide camera (invalid URL fails client construction
/// before the connect retry loop) in a guiding train, with an offline
/// mount so no simulator is needed: the pixel-size conversion input is
/// the piece that's missing.
#[given("rp is running with an offline camera in a guiding train")]
async fn rp_with_offline_camera_in_guiding_train(world: &mut RpWorld) {
    crate::steps::rotator_steps::add_offline_camera(world, "main-cam");
    push_guiding_train_for_camera(world, Some(200.0));
    world.mount = Some(MountConfig {
        alpaca_url: "not-a-url".to_string(),
        device_number: 0,
        settle_after_slew: None,
    });
    start_rp(world).await;
}

#[given(
    "rp is running with the simulator camera in a guiding train and two offline imaging trains"
)]
async fn rp_with_guiding_train_and_two_imaging_trains(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    push_guiding_train_for_camera(world, Some(200.0));
    for id in ["first", "second"] {
        crate::steps::rotator_steps::add_offline_camera(world, &format!("{id}-cam"));
        world.optical_trains.push(OpticalTrainConfig {
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

// --- Then steps: tool result ----------------------------------------

#[then(expr = "the guider result should contain {string} with value {string}")]
async fn guider_result_string_field(world: &mut RpWorld, field: String, expected: String) {
    let result = last_guider_result(world);
    assert_eq!(
        result.get(&field).and_then(|v| v.as_str()),
        Some(expected.as_str()),
        "field '{field}' mismatch in {result}"
    );
}

#[then(expr = "the guider result should contain {string} with number {float}")]
async fn guider_result_number_field(world: &mut RpWorld, field: String, expected: f64) {
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

// --- Then steps: stub request assertions -----------------------------

#[then("the stub guider should have received a start request without a settle override")]
async fn stub_start_without_settle(world: &mut RpWorld) {
    let request = last_stub_request_to(world, "/guiding/start").await;
    assert!(
        request.get("settle").is_none(),
        "expected no settle override, got: {request}"
    );
}

#[then(
    expr = "the stub guider should have received a start request with settle pixels {float} time {string} timeout {string}"
)]
async fn stub_start_with_settle(world: &mut RpWorld, pixels: f64, time: String, timeout: String) {
    let request = last_stub_request_to(world, "/guiding/start").await;
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
async fn stub_start_with_recalibrate(world: &mut RpWorld) {
    let request = last_stub_request_to(world, "/guiding/start").await;
    assert_eq!(
        request
            .get("recalibrate")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "recalibrate mismatch in {request}"
    );
}

/// Self-consistency of the documented conversion: the amount the stub
/// received equals `arcsec / (206.265 × pixel_size_x_um /
/// focal_length_mm)`, with the pixel size read straight from the
/// simulator camera's Alpaca API (the same connect-time property rp
/// caches).
#[then(
    expr = "the forwarded dither amount should equal {float} arcseconds at the guiding train's {float} mm pixel scale"
)]
async fn stub_dither_amount_from_arcsec(world: &mut RpWorld, arcsec: f64, focal_length_mm: f64) {
    let url = format!("{}/api/v1/camera/0/pixelsizex", world.omnisim_url());
    let body: Value = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect("failed to query the simulator camera's pixel size")
        .json()
        .await
        .expect("pixel-size response was not JSON");
    let pixel_size_x_um = body
        .get("Value")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("no numeric Value in pixel-size response: {body}"));

    let scale_arcsec_per_px = 206.265 * pixel_size_x_um / focal_length_mm;
    let expected_px = arcsec / scale_arcsec_per_px;
    let request = last_stub_request_to(world, "/dither").await;
    let actual = request
        .get("amount_px")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("no numeric amount_px in {request}"));
    assert!(
        (actual - expected_px).abs() < 1e-9,
        "amount_px: expected {expected_px} ({arcsec}\" at {scale_arcsec_per_px}\"/px), got {actual}"
    );
}

#[then(
    expr = "the stub guider should have received a dither request with amount_px {float} and ra_only {word}"
)]
async fn stub_dither_request(world: &mut RpWorld, amount_px: f64, ra_only: String) {
    let request = last_stub_request_to(world, "/dither").await;
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

#[then("the stub guider should have received a stop request")]
async fn stub_stop_request(world: &mut RpWorld) {
    let stops = guider_stub(world).requests_to("/guiding/stop").await;
    assert!(!stops.is_empty(), "the stub received no stop request");
}

#[then("the stub guider should have received a pause request with full true")]
async fn stub_pause_request_full(world: &mut RpWorld) {
    let request = last_stub_request_to(world, "/guiding/pause").await;
    assert_eq!(
        request.get("full").and_then(serde_json::Value::as_bool),
        Some(true),
        "full mismatch in {request}"
    );
}

#[then("the stub guider should have received a resume request")]
async fn stub_resume_request(world: &mut RpWorld) {
    let resumes = guider_stub(world).requests_to("/guiding/resume").await;
    assert!(!resumes.is_empty(), "the stub received no resume request");
}

// --- Then steps: safety integration ----------------------------------

#[then(expr = "the stub guider should have received a stop request within {int} seconds")]
async fn stub_stop_request_within(world: &mut RpWorld, seconds: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        if !guider_stub(world)
            .requests_to("/guiding/stop")
            .await
            .is_empty()
        {
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

// --- Helpers --------------------------------------------------------

async fn call_guider_tool(world: &mut RpWorld, tool: &str, params: Map<String, Value>) {
    ensure_mcp_client(world).await;

    let result = world.mcp().call_tool(tool, Value::Object(params)).await;

    match &result {
        Ok(v) => world.last_guider_result = Some(v.clone()),
        Err(_) => world.last_guider_result = None,
    }
    world.last_tool_result = Some(result);
}

const fn last_guider_result(world: &RpWorld) -> &Value {
    world
        .last_guider_result
        .as_ref()
        .expect("no successful guider tool result recorded — When step missing or tool errored?")
}

const fn guider_stub(world: &RpWorld) -> &GuiderStub {
    world
        .guider_stub
        .as_ref()
        .expect("no guider stub registered for this scenario")
}

/// The most recent request body the stub received on the endpoint
/// whose path ends with `path_suffix`.
async fn last_stub_request_to(world: &RpWorld, path_suffix: &str) -> Value {
    let mut requests = guider_stub(world).requests_to(path_suffix).await;
    requests
        .pop()
        .unwrap_or_else(|| panic!("stub guider received no request to ...{path_suffix}"))
}

/// A guiding train terminating in the scenario's `main-cam`.
fn push_guiding_train_for_camera(world: &mut RpWorld, focal_length_mm: Option<f64>) {
    world.optical_trains.push(OpticalTrainConfig {
        id: "guide".to_string(),
        purpose: Some("guiding".to_string()),
        focal_length_mm,
        default_position_angle_degrees: None,
        devices: vec!["main-cam".to_string()],
        auto_focus: None,
    });
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
