//! BDD step definitions for rp's plain-REST config endpoints
//! (`config_rest.feature`).
//!
//! These scenarios spawn their own rp on port 0 from a scenario-private temp
//! config file and never touch `OmniSim`, so the feature is untagged (no
//! `@serial`). The camera used by the secret scenarios points at an
//! unreachable Alpaca URL — rp boots regardless (equipment connects lazily /
//! records the failure) and the config endpoints only need the file.

use std::path::PathBuf;

use cucumber::{given, then, when};
use serde_json::Value;

use bdd_infra::ServiceHandle;

use crate::world::RpWorld;

/// Write a scenario-private rp config (port 0, temp data directory, the given
/// `equipment` block) and remember its path for later file assertions.
/// `pub(crate)`: the optical-trains validation steps stage their configs
/// through the same path.
pub fn write_scenario_config(world: &mut RpWorld, equipment: Value) {
    write_scenario_config_with_server(
        world,
        equipment,
        serde_json::json!({ "port": 0, "bind_address": "127.0.0.1" }),
    );
}

/// [`write_scenario_config`] with an explicit `server` block — the
/// Host-allowlist scenarios need the wildcard bind that derives rp's
/// advertised hostname URL.
pub fn write_scenario_config_with_server(world: &mut RpWorld, equipment: Value, server: Value) {
    let dir = tempfile::tempdir().expect("create temp dir for rp config");
    let config = serde_json::json!({
        "session": { "data_directory": dir.path().join("data").to_string_lossy() },
        "equipment": equipment,
        "server": server
    });
    let path = dir.path().join("rp.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&config).expect("serialize rp config"),
    )
    .expect("write rp config file");
    world.config_rest_path = Some(path);
    world.config_rest_dir = Some(dir);
}

fn config_rest_path(world: &RpWorld) -> PathBuf {
    world
        .config_rest_path
        .clone()
        .expect("no temp rp config — add a 'Given a temp rp config ...' step")
}

fn config_file_value(world: &RpWorld) -> Value {
    let path = config_rest_path(world);
    serde_json::from_str(&std::fs::read_to_string(&path).expect("read rp config file"))
        .expect("rp config file is JSON")
}

const fn last_response_json(world: &RpWorld) -> &Value {
    world
        .last_config_response_json
        .as_ref()
        .expect("last config response was not JSON — check the request step ran")
}

pub async fn send_put_config(world: &mut RpWorld, body: String) {
    let client = reqwest::Client::new();
    let response = client
        .put(format!("{}/api/config", world.rp_url()))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("PUT /api/config request failed");
    record_response(world, response).await;
}

async fn record_response(world: &mut RpWorld, response: reqwest::Response) {
    world.last_config_response_status = Some(response.status().as_u16());
    let text = response.text().await.expect("read config response body");
    world.last_config_response_json = serde_json::from_str(&text).ok();
    world.last_config_response_text = Some(text);
}

// ---------------------------------------------------------------------------
// Given
// ---------------------------------------------------------------------------

#[given("a temp rp config with no equipment")]
fn temp_config_no_equipment(world: &mut RpWorld) {
    write_scenario_config(world, serde_json::json!({}));
}

#[given(expr = "a temp rp config with a camera whose stored auth password is {string}")]
fn temp_config_with_camera_password(world: &mut RpWorld, password: String) {
    write_scenario_config(
        world,
        serde_json::json!({
            "cameras": [{
                "id": "main-cam",
                // Unreachable on purpose: the scenario never connects the
                // camera; only the stored credential matters.
                "alpaca_url": "http://127.0.0.1:1",
                "auth": { "username": "observatory", "password": password }
            }]
        }),
    );
}

#[given(
    expr = "a temp rp config with cameras {string} and {string} whose stored auth passwords are {string} and {string}"
)]
fn temp_config_with_two_camera_passwords(
    world: &mut RpWorld,
    first_id: String,
    second_id: String,
    first_password: String,
    second_password: String,
) {
    let camera = |id: &str, password: &str| {
        serde_json::json!({
            "id": id,
            "alpaca_url": "http://127.0.0.1:1",
            "auth": { "username": "observatory", "password": password }
        })
    };
    write_scenario_config(
        world,
        serde_json::json!({
            "cameras": [camera(&first_id, &first_password), camera(&second_id, &second_password)]
        }),
    );
}

#[given("rp is started with that config file")]
async fn rp_started_with_config_file(world: &mut RpWorld) {
    let path = config_rest_path(world);
    let handle =
        ServiceHandle::start(env!("CARGO_PKG_NAME"), path.to_str().expect("utf-8 path")).await;
    world.rp = Some(handle);
    assert!(
        world.wait_for_rp_healthy().await,
        "rp did not become healthy"
    );
}

#[given("I remember the config file bytes")]
fn remember_config_file_bytes(world: &mut RpWorld) {
    let path = config_rest_path(world);
    world.config_file_snapshot = Some(std::fs::read_to_string(&path).expect("read rp config file"));
}

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

#[when("I GET /api/config")]
async fn get_api_config(world: &mut RpWorld) {
    let response = reqwest::get(format!("{}/api/config", world.rp_url()))
        .await
        .expect("GET /api/config request failed");
    record_response(world, response).await;
    world.fetched_config = world
        .last_config_response_json
        .as_ref()
        .and_then(|body| body.get("config").cloned());
}

#[when("I GET /api/config/schema")]
async fn get_api_config_schema(world: &mut RpWorld) {
    let response = reqwest::get(format!("{}/api/config/schema", world.rp_url()))
        .await
        .expect("GET /api/config/schema request failed");
    record_response(world, response).await;
}

#[when(expr = "I PUT \\/api\\/config with the fetched config after setting {string} to {string}")]
async fn put_config_with_pointer_set(world: &mut RpWorld, pointer: String, raw: String) {
    let mut config = world
        .fetched_config
        .clone()
        .expect("no fetched config — add a 'When I GET /api/config' step first");
    // The raw step value parses as JSON where possible ("256" → number),
    // falling back to a plain string.
    let value: Value = serde_json::from_str(&raw).unwrap_or(Value::String(raw));
    *config
        .pointer_mut(&pointer)
        .unwrap_or_else(|| panic!("pointer {pointer} not present in fetched config")) = value;
    send_put_config(world, config.to_string()).await;
}

#[when(
    expr = "I PUT \\/api\\/config with the fetched config after setting the site latitude to {float}"
)]
async fn put_config_with_site_latitude(world: &mut RpWorld, latitude: f64) {
    let mut config = world
        .fetched_config
        .clone()
        .expect("no fetched config — add a 'When I GET /api/config' step first");
    // Inserted rather than assigned through a pointer: an unset `site` is
    // absent from the fetched config, not a null, so there is no node to
    // overwrite. This is also the shape the scenario describes — an
    // operator adding a site block whose latitude is out of range.
    config
        .as_object_mut()
        .expect("fetched config is a JSON object")
        .insert(
            "site".to_string(),
            serde_json::json!({
                "latitude_degrees": latitude,
                "longitude_degrees": 0.0
            }),
        );
    send_put_config(world, config.to_string()).await;
}

#[when(expr = "I PUT \\/api\\/config with the fetched config after removing camera {string}")]
async fn put_config_with_camera_removed(world: &mut RpWorld, id: String) {
    let mut config = world
        .fetched_config
        .clone()
        .expect("no fetched config — add a 'When I GET /api/config' step first");
    let cameras = config
        .pointer_mut("/equipment/cameras")
        .and_then(Value::as_array_mut)
        .expect("fetched config carries an equipment.cameras array");
    let before = cameras.len();
    cameras.retain(|camera| camera.get("id").and_then(Value::as_str) != Some(id.as_str()));
    assert_eq!(
        cameras.len(),
        before - 1,
        "no camera with id {id} in the fetched config"
    );
    send_put_config(world, config.to_string()).await;
}

#[when("I PUT /api/config with the fetched config unchanged")]
async fn put_config_unchanged(world: &mut RpWorld) {
    let config = world
        .fetched_config
        .clone()
        .expect("no fetched config — add a 'When I GET /api/config' step first");
    send_put_config(world, config.to_string()).await;
}

#[when("I PUT /api/config with a body just over the 2 MiB request limit")]
async fn put_config_oversized_body(world: &mut RpWorld) {
    // Just over axum's default `DefaultBodyLimit` (2 MiB). If the limit were
    // ever disabled, this body would reach the parser and fail 400, not 413.
    send_put_config(world, "x".repeat(2 * 1024 * 1024 + 4096)).await;
}

#[when(expr = "I PUT \\/api\\/config with body {string}")]
async fn put_config_raw_body(world: &mut RpWorld, body: String) {
    send_put_config(world, body).await;
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

#[then(expr = "the config response status should be {int}")]
fn config_response_status(world: &mut RpWorld, expected: u16) {
    assert_eq!(
        world.last_config_response_status,
        Some(expected),
        "unexpected status; body was: {:?}",
        world.last_config_response_text
    );
}

// The expected value is compared verbatim, so the redaction sentinel
// ("********") lives in the feature file — the spec — and a change to the
// shared constant would fail the scenario loudly.
#[then(expr = "the fetched config field {string} should be {string}")]
fn fetched_config_field_should_be(world: &mut RpWorld, pointer: String, expected: String) {
    let config = world
        .fetched_config
        .as_ref()
        .expect("no fetched config — add a 'When I GET /api/config' step first");
    assert_eq!(
        config.pointer(&pointer).and_then(Value::as_str),
        Some(expected.as_str()),
        "unexpected value at {pointer}; config was: {config}"
    );
}

#[then("the config overrides list should be empty")]
fn config_overrides_empty(world: &mut RpWorld) {
    let body = last_response_json(world);
    assert_eq!(
        body["overrides"],
        serde_json::json!([]),
        "rp has no CLI overrides; got: {body}"
    );
}

#[then(expr = "the schema read-only fields should be exactly {string}")]
fn schema_read_only_fields(world: &mut RpWorld, expected: String) {
    let body = last_response_json(world);
    assert_eq!(
        body["read_only_fields"],
        serde_json::json!([expected]),
        "unexpected read_only_fields; body was: {body}"
    );
}

#[then("the schema locked fields should be empty")]
fn schema_locked_fields_empty(world: &mut RpWorld) {
    let body = last_response_json(world);
    assert_eq!(
        body["locked_fields"],
        serde_json::json!([]),
        "unexpected locked_fields; body was: {body}"
    );
}

#[then(expr = "the apply status should be {string}")]
fn apply_status_should_be(world: &mut RpWorld, expected: String) {
    let body = last_response_json(world);
    assert_eq!(
        body["status"].as_str(),
        Some(expected.as_str()),
        "unexpected apply status; body was: {body}"
    );
}

#[then(expr = "the restart-required list should be exactly {string}")]
fn restart_required_exactly(world: &mut RpWorld, path: String) {
    let body = last_response_json(world);
    assert_eq!(
        body["restart_required"],
        serde_json::json!([path]),
        "unexpected restart_required; body was: {body}"
    );
}

#[then("the restart-required list should be empty")]
fn restart_required_empty(world: &mut RpWorld) {
    let body = last_response_json(world);
    assert_eq!(
        body["restart_required"],
        serde_json::json!([]),
        "unexpected restart_required; body was: {body}"
    );
}

#[then("the reload list should be empty")]
fn reload_list_empty(world: &mut RpWorld) {
    let body = last_response_json(world);
    assert_eq!(
        body["reload"],
        serde_json::json!([]),
        "rp never reloads in-process; body was: {body}"
    );
}

#[then(expr = "the apply errors should name path {string}")]
fn apply_errors_name_path(world: &mut RpWorld, path: String) {
    let body = last_response_json(world);
    let errors = body["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("apply response carries no errors[]; body was: {body}"));
    assert!(
        errors.iter().any(|e| e["path"] == path.as_str()),
        "no error names path {path}; errors were: {errors:?}"
    );
}

#[then(expr = "the apply error at path {string} should mention {string}")]
fn apply_error_at_path_mentions(world: &mut RpWorld, path: String, needle: String) {
    let body = last_response_json(world);
    let errors = body["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("apply response carries no errors[]; body was: {body}"));
    let messages: Vec<&str> = errors
        .iter()
        .filter(|e| e["path"] == path.as_str())
        .filter_map(|e| e["msg"].as_str())
        .collect();
    assert!(
        messages.iter().any(|m| m.contains(&needle)),
        "no error at path {path} mentions {needle:?}; errors were: {errors:?}"
    );
}

#[then("the config file bytes should be unchanged")]
fn config_file_bytes_unchanged(world: &mut RpWorld) {
    let snapshot = world
        .config_file_snapshot
        .as_ref()
        .expect("no snapshot — add a 'Given I remember the config file bytes' step");
    let path = config_rest_path(world);
    let current = std::fs::read_to_string(&path).expect("read rp config file");
    assert_eq!(
        &current, snapshot,
        "the config file must stay byte-identical"
    );
}

#[then(expr = "the config file JSON at {string} should be {string}")]
fn config_file_json_string_at(world: &mut RpWorld, pointer: String, expected: String) {
    let on_disk = config_file_value(world);
    assert_eq!(
        on_disk.pointer(&pointer).and_then(Value::as_str),
        Some(expected.as_str()),
        "unexpected on-disk value at {pointer}; file was: {on_disk}"
    );
}

#[then(expr = "the config file JSON at {string} should be the number {int}")]
fn config_file_json_number_at(world: &mut RpWorld, pointer: String, expected: i64) {
    let on_disk = config_file_value(world);
    assert_eq!(
        on_disk.pointer(&pointer).and_then(Value::as_i64),
        Some(expected),
        "unexpected on-disk value at {pointer}; file was: {on_disk}"
    );
}
