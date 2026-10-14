//! Step definitions for `tests/features/http_api.feature`.

use cucumber::{given, then, when};
use std::time::Duration;

use crate::world::GuiderWorld;

// ---------------------------------------------------------------------------
// Givens
// ---------------------------------------------------------------------------

#[given("a mock PHD2 that settles successfully")]
async fn mock_settles(world: &mut GuiderWorld) {
    world.start_mock("settle_ok", "stops").await;
}

#[given("a mock PHD2 that fails to settle")]
async fn mock_settle_fails(world: &mut GuiderWorld) {
    world.start_mock("settle_fail", "stops").await;
}

#[given("a mock PHD2 that never settles")]
async fn mock_never_settles(world: &mut GuiderWorld) {
    world.start_mock("never_settle", "stops").await;
}

#[given("a mock PHD2 that ignores stop requests")]
async fn mock_never_stops(world: &mut GuiderWorld) {
    world.start_mock("settle_ok", "never_stops").await;
}

#[given("a mock PHD2 with a connected rotator")]
async fn mock_with_rotator(world: &mut GuiderWorld) {
    world
        .start_mock_env("settle_ok", "stops", &[("MOCK_PHD2_ROTATOR", "connected")])
        .await;
}

#[given("the guider service is running")]
async fn service_running(world: &mut GuiderWorld) {
    let port = world.mock.as_ref().expect("mock PHD2 not started").port;
    world.start_service(port, "10s", true).await;
}

#[given(expr = "the guider service is running with a stop timeout of {string}")]
async fn service_running_with_stop_timeout(world: &mut GuiderWorld, stop_timeout: String) {
    let port = world.mock.as_ref().expect("mock PHD2 not started").port;
    world.start_service(port, &stop_timeout, true).await;
}

#[given("the guider service is running against an unreachable PHD2")]
async fn service_running_unreachable(world: &mut GuiderWorld) {
    // Port 1 is reserved; connecting to it is refused immediately.
    world.start_service(1, "10s", false).await;
}

// ---------------------------------------------------------------------------
// Whens
// ---------------------------------------------------------------------------

async fn post(world: &mut GuiderWorld, path: &str, body: serde_json::Value) {
    let url = format!("{}{}", world.service_url(), path);
    let response = GuiderWorld::http_client()
        .post(&url)
        .json(&body)
        .send()
        .await
        .expect("HTTP request failed");
    world.record_response(response).await;
}

async fn get(world: &mut GuiderWorld, path: &str) {
    let url = format!("{}{}", world.service_url(), path);
    let response = GuiderWorld::http_client()
        .get(&url)
        .send()
        .await
        .expect("HTTP request failed");
    world.record_response(response).await;
}

#[when("the client starts guiding")]
async fn start_guiding(world: &mut GuiderWorld) {
    post(world, "/api/v1/guiding/start", serde_json::json!({})).await;
}

#[when(
    expr = "the client starts guiding with settle pixels {float}, time {string}, and timeout {string}"
)]
async fn start_guiding_with_settle(
    world: &mut GuiderWorld,
    pixels: f64,
    time: String,
    timeout: String,
) {
    post(
        world,
        "/api/v1/guiding/start",
        serde_json::json!({ "settle": { "pixels": pixels, "time": time, "timeout": timeout } }),
    )
    .await;
}

#[when("the client stops guiding")]
async fn stop_guiding(world: &mut GuiderWorld) {
    post(world, "/api/v1/guiding/stop", serde_json::json!({})).await;
}

#[when(expr = "the client dithers by {float} pixels")]
async fn dither(world: &mut GuiderWorld, amount_px: f64) {
    post(
        world,
        "/api/v1/dither",
        serde_json::json!({ "amount_px": amount_px }),
    )
    .await;
}

#[when(
    expr = "the client dithers by {float} pixels RA-only with settle pixels {float}, time {string}, and timeout {string}"
)]
async fn dither_with_settle(
    world: &mut GuiderWorld,
    amount_px: f64,
    pixels: f64,
    time: String,
    timeout: String,
) {
    post(
        world,
        "/api/v1/dither",
        serde_json::json!({
            "amount_px": amount_px,
            "ra_only": true,
            "settle": { "pixels": pixels, "time": time, "timeout": timeout }
        }),
    )
    .await;
}

#[when("the client pauses guiding fully")]
async fn pause_fully(world: &mut GuiderWorld) {
    post(
        world,
        "/api/v1/guiding/pause",
        serde_json::json!({ "full": true }),
    )
    .await;
}

#[when("the client resumes guiding")]
async fn resume(world: &mut GuiderWorld) {
    post(world, "/api/v1/guiding/resume", serde_json::json!({})).await;
}

#[when("the client requests the guiding stats")]
async fn request_stats(world: &mut GuiderWorld) {
    get(world, "/api/v1/guiding/stats").await;
}

#[when("the client requests the guiding metrics")]
async fn request_metrics(world: &mut GuiderWorld) {
    get(world, "/api/v1/guiding/metrics").await;
}

#[when("the client requests the guider equipment")]
async fn request_equipment(world: &mut GuiderWorld) {
    get(world, "/api/v1/equipment").await;
}

#[when("the client clears the guider calibration")]
async fn clear_calibration(world: &mut GuiderWorld) {
    post(world, "/api/v1/calibration/clear", serde_json::json!({})).await;
}

#[when("the client re-selects the guide star")]
async fn reselect_star(world: &mut GuiderWorld) {
    post(world, "/api/v1/star/reselect", serde_json::json!({})).await;
}

#[when("the client probes the service health")]
async fn probe_health(world: &mut GuiderWorld) {
    get(world, "/health").await;
}

// ---------------------------------------------------------------------------
// Thens — HTTP response assertions
// ---------------------------------------------------------------------------

fn approx(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

#[then(expr = "the response status should be {int}")]
async fn response_status(world: &mut GuiderWorld, status: u16) {
    let response = world.last_response();
    assert_eq!(
        response.status, status,
        "unexpected status; body: {}",
        response.body
    );
}

#[then(expr = "the response field {string} should be {string}")]
async fn response_field_string(world: &mut GuiderWorld, field: String, expected: String) {
    let body = &world.last_response().body;
    assert_eq!(
        body[&field].as_str(),
        Some(expected.as_str()),
        "field {field} in {body}"
    );
}

#[then(expr = "the response field {string} should contain {string}")]
async fn response_field_contains(world: &mut GuiderWorld, field: String, needle: String) {
    let body = &world.last_response().body;
    let value = body[&field].as_str().unwrap_or_default();
    assert!(
        value.contains(&needle),
        "field {field} value {value:?} does not contain {needle:?} in {body}"
    );
}

#[then(expr = "the response field {string} should be {float}")]
async fn response_field_number(world: &mut GuiderWorld, field: String, expected: f64) {
    let body = &world.last_response().body;
    let actual = body[&field]
        .as_f64()
        .unwrap_or_else(|| panic!("field {field} is not a number in {body}"));
    approx(actual, expected);
}

#[then(expr = "the response field {string} should be true")]
async fn response_field_true(world: &mut GuiderWorld, field: String) {
    let body = &world.last_response().body;
    assert_eq!(
        body[&field].as_bool(),
        Some(true),
        "field {field} in {body}"
    );
}

#[then(expr = "the response error should be {string}")]
async fn response_error(world: &mut GuiderWorld, code: String) {
    let body = &world.last_response().body;
    assert_eq!(body["error"].as_str(), Some(code.as_str()), "in {body}");
}

#[then(expr = "the metrics window should hold {int} frames")]
async fn metrics_window_len(world: &mut GuiderWorld, count: usize) {
    let body = &world.last_response().body;
    let frames = body["frames"]
        .as_array()
        .unwrap_or_else(|| panic!("no frames array in {body}"));
    assert_eq!(frames.len(), count, "frames in {body}");
}

#[then(expr = "metrics entry {int} should report frame {int}, hfd {float}, and star_lost false")]
async fn metrics_entry_guide_step(world: &mut GuiderWorld, index: usize, frame: u64, hfd: f64) {
    let body = &world.last_response().body;
    let entry = &body["frames"][index - 1];
    assert_eq!(entry["frame"].as_u64(), Some(frame), "entry {entry}");
    approx(entry["hfd"].as_f64().expect("hfd"), hfd);
    assert_eq!(entry["star_lost"].as_bool(), Some(false), "entry {entry}");
}

#[then(expr = "metrics entry {int} should be a star-lost frame with frame number {int}")]
async fn metrics_entry_star_lost(world: &mut GuiderWorld, index: usize, frame: u64) {
    let body = &world.last_response().body;
    let entry = &body["frames"][index - 1];
    assert_eq!(entry["frame"].as_u64(), Some(frame), "entry {entry}");
    assert_eq!(entry["star_lost"].as_bool(), Some(true), "entry {entry}");
    assert!(
        entry["hfd"].is_null(),
        "star-lost entry carries no hfd: {entry}"
    );
}

#[then(expr = "the equipment {string} slot should be {string}")]
async fn equipment_slot_named(world: &mut GuiderWorld, slot: String, name: String) {
    let body = &world.last_response().body;
    assert_eq!(
        body[&slot]["name"].as_str(),
        Some(name.as_str()),
        "slot {slot} in {body}"
    );
}

#[then(expr = "the equipment {string} slot should be null")]
async fn equipment_slot_null(world: &mut GuiderWorld, slot: String) {
    let body = &world.last_response().body;
    assert!(body[&slot].is_null(), "slot {slot} in {body}");
}

#[then(expr = "the response error should be {string} mentioning {string}")]
async fn response_error_mentioning(world: &mut GuiderWorld, code: String, needle: String) {
    let body = &world.last_response().body;
    assert_eq!(body["error"].as_str(), Some(code.as_str()), "in {body}");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(&needle),
        "message {message:?} does not mention {needle:?}"
    );
}

// ---------------------------------------------------------------------------
// Thens — mock RPC log assertions
// ---------------------------------------------------------------------------

/// The settle sequence the mock emits after `guide`/`dither` runs on
/// its own thread; give a just-issued RPC a moment to reach the log
/// before declaring it absent.
async fn settle_log(world: &GuiderWorld) {
    let _ = world;
    tokio::time::sleep(Duration::from_millis(100)).await;
}

fn assert_settle(params: &serde_json::Value, pixels: f64, time: u64, timeout: u64) {
    let settle = &params["settle"];
    approx(settle["pixels"].as_f64().expect("settle.pixels"), pixels);
    assert_eq!(
        settle["time"].as_u64(),
        Some(time),
        "settle.time in {settle}"
    );
    assert_eq!(
        settle["timeout"].as_u64(),
        Some(timeout),
        "settle.timeout in {settle}"
    );
}

#[then(expr = "the mock PHD2 should have received a {string} request")]
async fn mock_received(world: &mut GuiderWorld, method: String) {
    settle_log(world).await;
    assert!(
        !world.logged_rpcs_named(&method).is_empty(),
        "no {method} RPC in log: {:?}",
        world.logged_rpcs()
    );
}

#[then(expr = "the mock PHD2 should not have received a {string} request")]
async fn mock_not_received(world: &mut GuiderWorld, method: String) {
    settle_log(world).await;
    assert!(
        world.logged_rpcs_named(&method).is_empty(),
        "unexpected {method} RPC in log"
    );
}

#[then(
    expr = "the mock PHD2 should have received a {string} request with settle pixels {float}, time {int}, and timeout {int}"
)]
async fn mock_received_with_settle(
    world: &mut GuiderWorld,
    method: String,
    pixels: f64,
    time: u64,
    timeout: u64,
) {
    settle_log(world).await;
    let rpcs = world.logged_rpcs_named(&method);
    let rpc = rpcs
        .last()
        .unwrap_or_else(|| panic!("no {method} RPC in log"));
    assert_settle(&rpc["params"], pixels, time, timeout);
}

#[then("the mock PHD2 guide request should not ask for recalibration")]
async fn mock_guide_no_recalibrate(world: &mut GuiderWorld) {
    let rpcs = world.logged_rpcs_named("guide");
    let rpc = rpcs.last().expect("no guide RPC in log");
    assert_eq!(rpc["params"]["recalibrate"].as_bool(), Some(false));
}

#[then(
    expr = "the mock PHD2 should have received a dither request with amount {float}, raOnly true, settle pixels {float}, time {int}, and timeout {int}"
)]
async fn mock_received_dither(
    world: &mut GuiderWorld,
    amount: f64,
    pixels: f64,
    time: u64,
    timeout: u64,
) {
    settle_log(world).await;
    let rpcs = world.logged_rpcs_named("dither");
    let rpc = rpcs.last().expect("no dither RPC in log");
    approx(rpc["params"]["amount"].as_f64().expect("amount"), amount);
    assert_eq!(rpc["params"]["raOnly"].as_bool(), Some(true));
    assert_settle(&rpc["params"], pixels, time, timeout);
}

#[then("the mock PHD2 should have received a full pause request")]
async fn mock_received_full_pause(world: &mut GuiderWorld) {
    settle_log(world).await;
    let rpcs = world.logged_rpcs_named("set_paused");
    let full_pause = rpcs
        .iter()
        .any(|rpc| rpc["params"]["paused"] == true && rpc["params"]["full"] == "full");
    assert!(full_pause, "no full set_paused RPC in log: {rpcs:?}");
}

#[then("the mock PHD2 should have received an unpause request")]
async fn mock_received_unpause(world: &mut GuiderWorld) {
    settle_log(world).await;
    let rpcs = world.logged_rpcs_named("set_paused");
    let unpause = rpcs.iter().any(|rpc| rpc["params"]["paused"] == false);
    assert!(unpause, "no unpause set_paused RPC in log: {rpcs:?}");
}
