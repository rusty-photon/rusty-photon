//! BDD step definitions for the `compute_snr` MCP tool.
//!
//! Shared steps live in `tool_steps.rs` (`capture`, `lists available
//! tools`, `the tool call should return an error`, `the error message
//! should contain ...`). The exposure-document fetch step
//! (`I fetch the exposure document for the captured document_id`) and the
//! generic section-presence assertions live in `measure_basic_steps.rs`
//! and are reused here -- all five imaging tools persist into the same
//! exposure document, so the document-fetch plumbing is shared.

use cucumber::{then, when};
use serde_json::Value;

use crate::steps::tool_steps::ensure_mcp_client;
use crate::world::RpWorld;

// --- When steps ---

#[when("the MCP client calls \"compute_snr\" with the captured image path")]
async fn mcp_call_with_last_path(world: &mut RpWorld) {
    let image_path = world
        .last_image_path
        .clone()
        .expect("no captured image path available");

    call_tool(world, Some(&image_path), None, None, true, true).await;
}

#[when("the MCP client calls \"compute_snr\" with the captured document_id")]
async fn mcp_call_with_last_document_id(world: &mut RpWorld) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");

    call_tool(world, None, Some(&document_id), None, true, true).await;
}

#[when(
    expr = "the MCP client calls \"compute_snr\" with the captured image path and threshold_sigma {float}"
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

#[when(expr = "the MCP client calls \"compute_snr\" with image path {string}")]
async fn mcp_call_with_path(world: &mut RpWorld, image_path: String) {
    call_tool(world, Some(&image_path), None, None, true, true).await;
}

#[when(expr = "the MCP client calls \"compute_snr\" with document_id {string}")]
async fn mcp_call_with_document_id(world: &mut RpWorld, document_id: String) {
    call_tool(world, None, Some(&document_id), None, true, true).await;
}

#[when("the MCP client calls \"compute_snr\" with no arguments")]
async fn mcp_call_no_args(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("compute_snr", serde_json::json!({}))
        .await;

    record_result(world, result);
}

#[when("the MCP client calls \"compute_snr\" with the captured image path but no min_area")]
async fn mcp_call_missing_min_area(world: &mut RpWorld) {
    let image_path = world
        .last_image_path
        .clone()
        .expect("no captured image path available");

    call_tool(world, Some(&image_path), None, None, false, true).await;
}

#[when("the MCP client calls \"compute_snr\" with the captured image path but no max_area")]
async fn mcp_call_missing_max_area(world: &mut RpWorld) {
    let image_path = world
        .last_image_path
        .clone()
        .expect("no captured image path available");

    call_tool(world, Some(&image_path), None, None, true, false).await;
}

// --- Then steps ---

#[then(expr = "the compute_snr result should contain {string}")]
fn result_contains_field(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    assert!(
        result.get(&field).is_some(),
        "expected '{field}' in compute_snr result, got: {result:?}"
    );
}

#[then(expr = "the compute_snr result should contain {string} as a non-negative integer")]
fn result_contains_non_negative_integer(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in compute_snr result, got: {result:?}"));

    assert!(
        value.as_u64().is_some() || value.as_i64().is_some_and(|v| v >= 0),
        "expected '{field}' to be a non-negative integer, got: {value:?}"
    );
}

#[then(expr = "the compute_snr result should contain {string} as a non-negative number")]
fn result_contains_non_negative_number(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in compute_snr result, got: {result:?}"));

    let num = value
        .as_f64()
        .unwrap_or_else(|| panic!("expected '{field}' to be a number, got: {value:?}"));

    assert!(
        num >= 0.0,
        "expected '{field}' to be non-negative, got: {num}"
    );
}

#[then(expr = "the compute_snr result should contain {string} with value null")]
fn result_field_is_null(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in compute_snr result, got: {result:?}"));

    assert!(
        value.is_null(),
        "expected '{field}' to be null, got: {value:?}"
    );
}

#[then(expr = "the compute_snr result should contain {string} with value {int}")]
fn result_field_equals_int(world: &mut RpWorld, field: String, expected: i64) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in compute_snr result, got: {result:?}"));

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
    // Same fixture sizing as the other imaging tools.
    if include_min_area {
        args.insert("min_area".to_string(), serde_json::json!(5));
    }
    if include_max_area {
        args.insert("max_area".to_string(), serde_json::json!(65_536));
    }

    let result = world
        .mcp()
        .call_tool("compute_snr", Value::Object(args))
        .await;

    record_result(world, result);
}

fn record_result(world: &mut RpWorld, result: Result<Value, String>) {
    match &result {
        Ok(v) => world.last_compute_snr_result = Some(v.clone()),
        Err(_) => world.last_compute_snr_result = None,
    }
    world.last_tool_result = Some(result);
}

const fn result_or_panic(world: &RpWorld) -> &Value {
    world
        .last_compute_snr_result
        .as_ref()
        .expect("no compute_snr result")
}
