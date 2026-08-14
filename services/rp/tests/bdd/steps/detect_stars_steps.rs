//! BDD step definitions for the `detect_stars` MCP tool.
//!
//! Shared steps live in `tool_steps.rs` (`capture`, `lists available
//! tools`, `the tool call should return an error`, `the error message
//! should contain ...`). The exposure-document fetch step
//! (`I fetch the exposure document for the captured document_id`) and the
//! generic section-presence assertions live in `measure_basic_steps.rs`
//! and are reused here -- all three tools (`measure_basic`,
//! `estimate_background`, `detect_stars`) persist into the same exposure
//! document, so the document-fetch plumbing is shared.

use cucumber::{then, when};
use serde_json::Value;

use crate::steps::tool_steps::ensure_mcp_client;
use crate::world::RpWorld;

// --- When steps ---

#[when("the MCP client calls \"detect_stars\" with the captured image path")]
async fn mcp_call_with_last_path(world: &mut RpWorld) {
    let image_path = world
        .last_image_path
        .clone()
        .expect("no captured image path available");

    call_tool(world, Some(&image_path), None, None, true, true).await;
}

#[when("the MCP client calls \"detect_stars\" with the captured document_id")]
async fn mcp_call_with_last_document_id(world: &mut RpWorld) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");

    call_tool(world, None, Some(&document_id), None, true, true).await;
}

#[when(
    expr = "the MCP client calls \"detect_stars\" with the captured image path and threshold_sigma {float}"
)]
async fn mcp_call_with_threshold(world: &mut RpWorld, threshold_sigma: f64) {
    let image_path = world
        .last_image_path
        .clone()
        .expect("no captured image path available");

    call_tool(
        world,
        Some(&image_path),
        None,
        Some(threshold_sigma),
        true,
        true,
    )
    .await;
}

#[when(expr = "the MCP client calls \"detect_stars\" with image path {string}")]
async fn mcp_call_with_path(world: &mut RpWorld, image_path: String) {
    call_tool(world, Some(&image_path), None, None, true, true).await;
}

#[when(expr = "the MCP client calls \"detect_stars\" with document_id {string}")]
async fn mcp_call_with_document_id(world: &mut RpWorld, document_id: String) {
    call_tool(world, None, Some(&document_id), None, true, true).await;
}

#[when("the MCP client calls \"detect_stars\" with no arguments")]
async fn mcp_call_no_args(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("detect_stars", serde_json::json!({}))
        .await;

    record_result(world, result);
}

#[when("the MCP client calls \"detect_stars\" with the captured image path but no min_area")]
async fn mcp_call_missing_min_area(world: &mut RpWorld) {
    let image_path = world
        .last_image_path
        .clone()
        .expect("no captured image path available");

    call_tool(world, Some(&image_path), None, None, false, true).await;
}

#[when("the MCP client calls \"detect_stars\" with the captured image path but no max_area")]
async fn mcp_call_missing_max_area(world: &mut RpWorld) {
    let image_path = world
        .last_image_path
        .clone()
        .expect("no captured image path available");

    call_tool(world, Some(&image_path), None, None, true, false).await;
}

// --- Then steps ---

#[then(expr = "the detect_stars result should contain {string} as a non-negative integer")]
fn result_contains_non_negative_integer(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in detect_stars result, got: {result:?}"));

    assert!(
        value.as_u64().is_some() || value.as_i64().is_some_and(|v| v >= 0),
        "expected '{field}' to be a non-negative integer, got: {value:?}"
    );
}

#[then(expr = "the detect_stars result should contain {string} as a non-negative number")]
fn result_contains_non_negative_number(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in detect_stars result, got: {result:?}"));

    let num = value
        .as_f64()
        .unwrap_or_else(|| panic!("expected '{field}' to be a number, got: {value:?}"));

    assert!(
        num >= 0.0,
        "expected '{field}' to be non-negative, got: {num}"
    );
}

#[then(expr = "the detect_stars result should contain {string} as an array")]
fn result_contains_array(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in detect_stars result, got: {result:?}"));

    assert!(
        value.is_array(),
        "expected '{field}' to be an array, got: {value:?}"
    );
}

#[then(expr = "the detect_stars result should contain {string} as an empty array")]
fn result_contains_empty_array(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in detect_stars result, got: {result:?}"));

    let arr = value
        .as_array()
        .unwrap_or_else(|| panic!("expected '{field}' to be an array, got: {value:?}"));

    assert!(
        arr.is_empty(),
        "expected '{}' to be empty, got {} entries",
        field,
        arr.len()
    );
}

#[then(expr = "the detect_stars result should contain {string} with value {int}")]
fn result_field_equals_int(world: &mut RpWorld, field: String, expected: i64) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in detect_stars result, got: {result:?}"));

    let actual = value
        .as_i64()
        .or_else(|| value.as_u64().map(u64::cast_signed))
        .unwrap_or_else(|| panic!("expected '{field}' to be an integer, got: {value:?}"));

    assert_eq!(
        actual, expected,
        "expected '{field}' to equal {expected}, got: {actual}"
    );
}

// --- Helpers ---

async fn call_tool(
    world: &mut RpWorld,
    image_path: Option<&str>,
    document_id: Option<&str>,
    threshold_sigma: Option<f64>,
    include_min_area: bool,
    include_max_area: bool,
) {
    ensure_mcp_client(world).await;

    let mut args = serde_json::Map::new();
    if let Some(path) = image_path {
        args.insert("image_path".to_string(), Value::String(path.to_string()));
    }
    if let Some(doc_id) = document_id {
        args.insert("document_id".to_string(), Value::String(doc_id.to_string()));
    }
    if let Some(threshold) = threshold_sigma {
        args.insert("threshold_sigma".to_string(), serde_json::json!(threshold));
    }
    // min_area / max_area are required parameters with no defaults (they
    // encode pixel-scale assumptions per docs/services/rp.md). Same fixture
    // values as measure_basic_steps: 5 admits any plausible PSF, 65536
    // admits very large smoothed components on the OmniSim synthetic frame.
    if include_min_area {
        args.insert("min_area".to_string(), serde_json::json!(5));
    }
    if include_max_area {
        args.insert("max_area".to_string(), serde_json::json!(65_536));
    }

    let result = world
        .mcp()
        .call_tool("detect_stars", Value::Object(args))
        .await;

    record_result(world, result);
}

fn record_result(world: &mut RpWorld, result: Result<Value, String>) {
    match &result {
        Ok(v) => world.last_detect_stars_result = Some(v.clone()),
        Err(_) => world.last_detect_stars_result = None,
    }
    world.last_tool_result = Some(result);
}

const fn result_or_panic(world: &RpWorld) -> &Value {
    world
        .last_detect_stars_result
        .as_ref()
        .expect("no detect_stars result")
}
