//! BDD step definitions for the plan-data `schema`/`validate` MCP tools
//! (`plan_schema_validation.feature`, rp.md § Plan schema and
//! validation).
//!
//! Reuses the shared `the tool call should succeed` step from
//! `cover_calibrator_steps`, the `... should return an error` / `the
//! error message should contain` steps from `tool_steps`, and
//! `list_targets should report exactly N targets` from
//! `target_store_crud_steps`.

use cucumber::{then, when};
use serde_json::Value;

use crate::world::RpWorld;

use super::tool_steps::ensure_mcp_client;

/// The payload most recently handed to `validate_plan`, so the
/// agreement scenarios can feed the identical bytes to `add_target`
/// rather than restating them (a restatement could drift and the
/// scenario would still pass).
fn remember_validated(world: &mut RpWorld, value: Value) {
    world.last_validated_payload = Some(value);
}

fn validation_payload(world: &RpWorld) -> &Value {
    world
        .last_tool_result
        .as_ref()
        .expect("no tool result — call validate_plan first")
        .as_ref()
        .expect("validate_plan returned an error, not a validation report")
}

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

#[when(expr = "the MCP client calls \"get_plan_schema\" for entity {string}")]
async fn call_get_plan_schema(world: &mut RpWorld, entity: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("get_plan_schema", serde_json::json!({ "entity": entity }))
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client validates entity {string} with value:")]
async fn call_validate_plan(world: &mut RpWorld, step: &cucumber::gherkin::Step, entity: String) {
    let raw = step
        .docstring
        .as_ref()
        .expect("validate step needs a docstring payload");
    let value: Value = serde_json::from_str(raw.trim()).expect("payload must be valid JSON");
    remember_validated(world, value.clone());

    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "validate_plan",
            serde_json::json!({ "entity": entity, "value": value }),
        )
        .await;
    world.last_tool_result = Some(result);
}

#[when("the MCP client adds the target it just validated")]
async fn call_add_validated_target(world: &mut RpWorld) {
    let payload = world
        .last_validated_payload
        .clone()
        .expect("no validated payload — call validate_plan first");
    ensure_mcp_client(world).await;
    let result = world.mcp().call_tool("add_target", payload).await;
    world.last_tool_result = Some(result);
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

#[then(expr = "the returned schema should describe properties {string}")]
fn schema_describes_properties(world: &mut RpWorld, expected: String) {
    let payload = validation_payload(world);
    let properties = payload["schema"]["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("schema has no properties object: {payload}"));
    for name in expected.split(',').map(str::trim) {
        assert!(
            properties.contains_key(name),
            "schema is missing property {name:?}; has {:?}",
            properties.keys().collect::<Vec<_>>()
        );
    }
}

#[then("the validation should report valid")]
fn validation_valid(world: &mut RpWorld) {
    let payload = validation_payload(world);
    assert_eq!(
        payload["valid"], true,
        "expected valid, got errors: {}",
        payload["errors"]
    );
}

#[then("the validation should report invalid")]
fn validation_invalid(world: &mut RpWorld) {
    let payload = validation_payload(world);
    assert_eq!(payload["valid"], false, "expected invalid, got: {payload}");
}

#[then("the validation should report no errors")]
fn validation_no_errors(world: &mut RpWorld) {
    let payload = validation_payload(world);
    let errors = payload["errors"].as_array().expect("errors must be a list");
    assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
}

#[then(expr = "the validation should report at least {int} errors")]
fn validation_error_count(world: &mut RpWorld, minimum: usize) {
    let payload = validation_payload(world);
    let errors = payload["errors"].as_array().expect("errors must be a list");
    assert!(
        errors.len() >= minimum,
        "expected at least {minimum} errors, got {}: {errors:?}",
        errors.len()
    );
}

#[then(expr = "the validation errors should include path {string}")]
fn validation_includes_path(world: &mut RpWorld, expected: String) {
    let payload = validation_payload(world);
    let errors = payload["errors"].as_array().expect("errors must be a list");
    let paths: Vec<&str> = errors.iter().filter_map(|e| e["path"].as_str()).collect();
    assert!(
        paths.contains(&expected.as_str()),
        "expected an error at path {expected:?}, got paths {paths:?}"
    );
}

#[then(expr = "the validation error at path {string} should mention {string}")]
fn validation_message_mentions(world: &mut RpWorld, path: String, needle: String) {
    let payload = validation_payload(world);
    let errors = payload["errors"].as_array().expect("errors must be a list");
    let msg = errors
        .iter()
        .find(|e| e["path"].as_str() == Some(path.as_str()))
        .and_then(|e| e["msg"].as_str())
        .unwrap_or_else(|| panic!("no error at path {path:?}: {errors:?}"));
    assert!(
        msg.contains(&needle),
        "error at {path:?} does not mention {needle:?}: {msg:?}"
    );
}
