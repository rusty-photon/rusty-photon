//! BDD step definitions for the `measure_basic` MCP tool.
//!
//! Shared steps live in `tool_steps.rs`:
//! - `the MCP client calls "capture" with camera ... for ... ms`
//!   (stores `last_image_path` and `last_document_id` on the world)
//! - `the MCP client lists available tools`
//! - `the tool list should include {string}`
//! - `the tool call should return an error`
//! - `the error message should contain {string}`

use cucumber::{then, when};
use serde_json::Value;

use crate::steps::tool_steps::ensure_mcp_client;
use crate::world::RpWorld;

// --- When steps ---

#[when("the MCP client calls \"measure_basic\" with the captured image path")]
async fn mcp_call_measure_basic_with_last_path(world: &mut RpWorld) {
    let image_path = world
        .last_image_path
        .clone()
        .expect("no captured image path available");

    call_measure_basic(world, Some(&image_path), None, None).await;
}

#[when("the MCP client calls \"measure_basic\" with the captured document_id")]
async fn mcp_call_measure_basic_with_last_document_id(world: &mut RpWorld) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");

    call_measure_basic(world, None, Some(&document_id), None).await;
}

#[when(
    expr = "the MCP client calls \"measure_basic\" with the captured image path and threshold_sigma {float}"
)]
async fn mcp_call_measure_basic_with_threshold(world: &mut RpWorld, threshold_sigma: f64) {
    let image_path = world
        .last_image_path
        .clone()
        .expect("no captured image path available");

    call_measure_basic(world, Some(&image_path), None, Some(threshold_sigma)).await;
}

#[when(expr = "the MCP client calls \"measure_basic\" with image path {string}")]
async fn mcp_call_measure_basic_with_path(world: &mut RpWorld, image_path: String) {
    call_measure_basic(world, Some(&image_path), None, None).await;
}

#[when(expr = "the MCP client calls \"measure_basic\" with document_id {string}")]
async fn mcp_call_measure_basic_with_document_id(world: &mut RpWorld, document_id: String) {
    call_measure_basic(world, None, Some(&document_id), None).await;
}

#[when("the MCP client calls \"measure_basic\" with no arguments")]
async fn mcp_call_measure_basic_no_args(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("measure_basic", serde_json::json!({}))
        .await;

    record_result(world, result);
}

#[when("I fetch the exposure document for the captured document_id")]
async fn fetch_exposure_document(world: &mut RpWorld) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");

    let url = format!("{}/api/documents/{}", world.rp_url(), document_id);
    let client = reqwest::Client::new();

    if let Ok(resp) = client.get(&url).send().await {
        world.last_api_status = Some(resp.status().as_u16());
        match resp.json::<Value>().await {
            Ok(body) => world.last_exposure_document = Some(body),
            Err(_) => world.last_exposure_document = None,
        }
    } else {
        world.last_api_status = None;
        world.last_exposure_document = None;
    }
}

// --- Then steps ---

#[then(expr = "the measure_basic result should contain {string}")]
fn measure_basic_contains_field(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    assert!(
        result.get(&field).is_some(),
        "expected '{field}' in measure_basic result, got: {result:?}"
    );
}

#[then(expr = "the measure_basic result should contain {string} as a non-negative integer")]
fn measure_basic_contains_non_negative_integer(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in measure_basic result, got: {result:?}"));

    assert!(
        value.as_u64().is_some() || value.as_i64().is_some_and(|v| v >= 0),
        "expected '{field}' to be a non-negative integer, got: {value:?}"
    );
}

#[then(expr = "the measure_basic result should contain {string} as a non-negative number")]
fn measure_basic_contains_non_negative_number(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in measure_basic result, got: {result:?}"));

    let num = value
        .as_f64()
        .unwrap_or_else(|| panic!("expected '{field}' to be a number, got: {value:?}"));

    assert!(
        num >= 0.0,
        "expected '{field}' to be non-negative, got: {num}"
    );
}

#[then(expr = "the measure_basic result should contain {string} as a positive integer")]
fn measure_basic_contains_positive_integer(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in measure_basic result, got: {result:?}"));

    let num = value.as_u64().unwrap_or_else(|| {
        panic!("expected '{field}' to be a non-negative integer, got: {value:?}")
    });

    assert!(num > 0, "expected '{field}' to be positive, got: {num}");
}

#[then(expr = "the measure_basic result should contain {string} with value null")]
fn measure_basic_field_is_null(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in measure_basic result, got: {result:?}"));

    assert!(
        value.is_null(),
        "expected '{field}' to be null, got: {value:?}"
    );
}

#[then(expr = "the measure_basic result should contain {string} with value {int}")]
fn measure_basic_field_equals_int(world: &mut RpWorld, field: String, expected: i64) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in measure_basic result, got: {result:?}"));

    let actual = value
        .as_i64()
        .or_else(|| value.as_u64().map(u64::cast_signed))
        .unwrap_or_else(|| panic!("expected '{field}' to be an integer, got: {value:?}"));

    assert_eq!(
        actual, expected,
        "expected '{field}' to equal {expected}, got: {actual}"
    );
}

#[then(expr = "the exposure document should contain a section named {string}")]
fn exposure_document_has_section(world: &mut RpWorld, section_name: String) {
    let doc = world
        .last_exposure_document
        .as_ref()
        .expect("no exposure document fetched");

    let sections = doc
        .get("sections")
        .unwrap_or_else(|| panic!("exposure document has no 'sections' field, got: {doc:?}"));

    assert!(
        sections.get(&section_name).is_some(),
        "expected section '{section_name}' in exposure document, got sections: {sections:?}"
    );
}

#[then(expr = "the {string} section should contain {string}")]
fn section_contains_field(world: &mut RpWorld, section_name: String, field: String) {
    let doc = world
        .last_exposure_document
        .as_ref()
        .expect("no exposure document fetched");

    let section = doc
        .get("sections")
        .and_then(|s| s.get(&section_name))
        .unwrap_or_else(|| {
            panic!("expected section '{section_name}' in exposure document, got: {doc:?}")
        });

    assert!(
        section.get(&field).is_some(),
        "expected '{field}' in '{section_name}' section, got: {section:?}"
    );
}

// --- Helpers ---

async fn call_measure_basic(
    world: &mut RpWorld,
    image_path: Option<&str>,
    document_id: Option<&str>,
    threshold_sigma: Option<f64>,
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
    // min_area and max_area are required parameters (no defaults — they encode
    // pixel-scale assumptions per docs/services/rp.md). Bake test-fixture
    // values: 5 admits any plausible PSF, 65536 admits even very large
    // smoothed components on the OmniSim synthetic frame.
    args.insert("min_area".to_string(), serde_json::json!(5));
    args.insert("max_area".to_string(), serde_json::json!(65_536));

    let result = world
        .mcp()
        .call_tool("measure_basic", Value::Object(args))
        .await;

    record_result(world, result);
}

fn record_result(world: &mut RpWorld, result: Result<Value, String>) {
    match &result {
        Ok(v) => world.last_measure_basic_result = Some(v.clone()),
        Err(_) => world.last_measure_basic_result = None,
    }
    world.last_tool_result = Some(result);
}

const fn result_or_panic(world: &RpWorld) -> &Value {
    world
        .last_measure_basic_result
        .as_ref()
        .expect("no measure_basic result")
}
