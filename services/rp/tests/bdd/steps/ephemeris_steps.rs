//! BDD step definitions for the ephemeris primitive MCP tools.

use cucumber::{given, then, when};
use serde_json::Value;

use crate::steps::tool_steps::ensure_mcp_client;
use crate::world::RpWorld;

// --- Given steps ---

#[given(expr = "rp is configured with site latitude {float} longitude {float}")]
const fn site_configured(world: &mut RpWorld, lat: f64, lon: f64) {
    world.site = Some((lat, lon));
}

// Planner-target seeding for the `planner` feature now goes through the
// target store (post-boot `add_target` fixtures) — see
// `target_store_planner_steps.rs`. The `get_next_target` /
// `record_exposure` / `get_session_progress` When+Then steps below are
// tool-agnostic and stay here.

// --- When steps ---

/// Polaris ICRS coords (J2000.0): RA = 2.530... h, Dec = +89.264°.
const POLARIS_RA: f64 = 2.530_194_4;
const POLARIS_DEC: f64 = 89.264_111_1;

#[when("the MCP client calls \"compute_alt_az\" for Polaris")]
async fn call_alt_az_polaris(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "compute_alt_az",
            serde_json::json!({"ra": POLARIS_RA, "dec": POLARIS_DEC}),
        )
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"compute_alt_az\" with ra {string} dec {string}")]
async fn call_alt_az_explicit(world: &mut RpWorld, ra: String, dec: String) {
    ensure_mcp_client(world).await;
    let ra: f64 = ra.parse().expect("ra must parse as f64");
    let dec: f64 = dec.parse().expect("dec must parse as f64");
    let result = world
        .mcp()
        .call_tool("compute_alt_az", serde_json::json!({"ra": ra, "dec": dec}))
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"get_local_sidereal_time\" with time {string}")]
async fn call_lst(world: &mut RpWorld, time: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("get_local_sidereal_time", serde_json::json!({"time": time}))
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"get_target_status\" for target {string}")]
async fn call_target_status(world: &mut RpWorld, name: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "get_target_status",
            serde_json::json!({"target_name": name}),
        )
        .await;
    world.last_tool_result = Some(result);
}

#[when("the MCP client calls \"get_next_target\"")]
async fn call_next_target(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("get_next_target", serde_json::json!({}))
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"get_next_target\" at time {string}")]
async fn call_next_target_at(world: &mut RpWorld, time: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("get_next_target", serde_json::json!({ "time": time }))
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"record_exposure\" for target {string} filter {string}")]
async fn call_record_exposure(world: &mut RpWorld, target: String, filter: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "record_exposure",
            serde_json::json!({ "target": target, "filter": filter }),
        )
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"record_exposure\" for target {string} with no filter")]
async fn call_record_exposure_unfiltered(world: &mut RpWorld, target: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("record_exposure", serde_json::json!({ "target": target }))
        .await;
    world.last_tool_result = Some(result);
}

#[when("the MCP client calls \"get_site\"")]
async fn call_get_site(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("get_site", serde_json::json!({}))
        .await;
    world.last_tool_result = Some(result);
}

#[when("the MCP client calls \"get_session_progress\"")]
async fn call_session_progress(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("get_session_progress", serde_json::json!({}))
        .await;
    world.last_tool_result = Some(result);
}

#[then(expr = "the result target_name should be {string}")]
fn result_target_name(world: &mut RpWorld, expected: String) {
    let value = success_payload(world);
    let name = value
        .get("target_name")
        .and_then(|v| v.as_str())
        .expect("missing `target_name`");
    assert_eq!(name, expected.as_str());
}

#[then("the result altitude_degrees should be a finite number")]
fn result_altitude_finite(world: &mut RpWorld) {
    let value = success_payload(world);
    let alt = value
        .get("altitude_degrees")
        .and_then(serde_json::Value::as_f64)
        .expect("missing `altitude_degrees`");
    assert!(alt.is_finite(), "altitude_degrees not finite: {alt}");
}

#[then(expr = "the result reason should be {string}")]
fn result_reason(world: &mut RpWorld, expected: String) {
    let value = success_payload(world);
    let reason = value
        .get("reason")
        .and_then(|v| v.as_str())
        .expect("missing `reason`");
    assert_eq!(reason, expected.as_str());
}

#[then("the result target should be null")]
fn result_target_null(world: &mut RpWorld) {
    let value = success_payload(world);
    assert!(
        value.get("target").is_some_and(serde_json::Value::is_null),
        "expected target=null, got: {value}"
    );
}

#[then(expr = "the recommended target should be {string}")]
fn recommended_target(world: &mut RpWorld, expected: String) {
    let value = success_payload(world);
    let name = value
        .pointer("/target/name")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("missing `target.name` in: {value}"));
    assert_eq!(name, expected.as_str());
}

// The per-goal progress assertions ("the reported progress should be
// exactly:" / "the progress for target {string} should be exactly:")
// live in target_store_progress_steps.rs, alongside the frame-seeding
// Given that produces the counts. The filter-keyed `{completed, goal}`
// steps they replaced are gone with the counters themselves.

// `get_next_target` surfaces the recommended plan entry as a nested
// `exposure: {filter, duration_secs}` object (null when the target has
// no plan), so these steps navigate into it.

#[then(expr = "the result filter should be {string}")]
fn result_filter(world: &mut RpWorld, expected: String) {
    let value = success_payload(world);
    let filter = value
        .pointer("/exposure/filter")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("missing `exposure.filter` in: {value}"));
    assert_eq!(filter, expected.as_str());
}

#[then("the result filter should be null")]
fn result_filter_null(world: &mut RpWorld) {
    let value = success_payload(world);
    let exposure = value.get("exposure").expect("missing `exposure`");
    assert!(
        exposure.is_null()
            || exposure
                .get("filter")
                .is_some_and(serde_json::Value::is_null),
        "expected filter=null, got: {value}"
    );
}

#[then(expr = "the result duration_secs should be {float}")]
fn result_duration_secs(world: &mut RpWorld, expected: f64) {
    let value = success_payload(world);
    let duration = value
        .pointer("/exposure/duration_secs")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("missing `exposure.duration_secs` in: {value}"));
    assert!(
        (duration - expected).abs() < f64::EPSILON,
        "expected duration_secs={expected}, got {duration}"
    );
}

#[then("the result duration_secs should be null")]
fn result_duration_secs_null(world: &mut RpWorld) {
    let value = success_payload(world);
    let exposure = value.get("exposure").expect("missing `exposure`");
    assert!(
        exposure.is_null()
            || exposure
                .get("duration_secs")
                .is_some_and(serde_json::Value::is_null),
        "expected duration_secs=null, got: {value}"
    );
}

#[when(expr = "the MCP client calls \"get_twilight\" for date {string} kind {string}")]
async fn call_twilight(world: &mut RpWorld, date: String, kind: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "get_twilight",
            serde_json::json!({"date": date, "kind": kind}),
        )
        .await;
    world.last_tool_result = Some(result);
}

// --- Then steps ---

#[then("the result lst_hours should be in the range [0, 24)")]
fn lst_in_range(world: &mut RpWorld) {
    let value = success_payload(world);
    let lst = value
        .get("lst_hours")
        .and_then(serde_json::Value::as_f64)
        .expect("missing `lst_hours`");
    assert!((0.0..24.0).contains(&lst), "lst_hours {lst} not in [0, 24)");
}

#[then(expr = "the result altitude_degrees should be approximately {float} within {float}")]
fn altitude_within(world: &mut RpWorld, expected: f64, tolerance: f64) {
    let value = success_payload(world);
    let alt = value
        .get("altitude_degrees")
        .and_then(serde_json::Value::as_f64)
        .expect("missing `altitude_degrees`");
    assert!(
        (alt - expected).abs() < tolerance,
        "altitude_degrees {alt} not within {tolerance} of expected {expected}"
    );
}

#[then(expr = "the tool error message should mention {string}")]
fn error_mentions(world: &mut RpWorld, fragment: String) {
    let result = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref();
    let msg = match result {
        Err(e) => e.as_str(),
        Ok(_) => panic!("expected tool call error, got success"),
    };
    assert!(
        msg.contains(fragment.as_str()),
        "expected error to contain {fragment:?}, got: {msg}"
    );
}

// --- Helpers ---

#[then(expr = "the result latitude_degrees should be {float}")]
fn result_latitude_degrees(world: &mut RpWorld, expected: f64) {
    let value = success_payload(world);
    let lat = value
        .get("latitude_degrees")
        .and_then(Value::as_f64)
        .expect("missing `latitude_degrees`");
    assert!(
        (lat - expected).abs() < 1e-9,
        "latitude_degrees {lat} != {expected}"
    );
}

#[then(expr = "the result longitude_degrees should be {float}")]
fn result_longitude_degrees(world: &mut RpWorld, expected: f64) {
    let value = success_payload(world);
    let lon = value
        .get("longitude_degrees")
        .and_then(Value::as_f64)
        .expect("missing `longitude_degrees`");
    assert!(
        (lon - expected).abs() < 1e-9,
        "longitude_degrees {lon} != {expected}"
    );
}

fn success_payload(world: &RpWorld) -> &Value {
    world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("expected tool call to succeed")
}
