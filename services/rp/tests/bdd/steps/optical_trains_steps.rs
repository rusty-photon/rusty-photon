//! BDD step definitions for the optical-trains configuration model
//! (`optical_trains.feature`).
//!
//! The validation scenarios reuse the plain-REST config machinery from
//! `config_rest_steps.rs` (GET / PUT `/api/config`, apply-status and
//! error-path assertions); the capture scenarios reuse the `OmniSim` +
//! MCP steps from `tool_steps.rs` and the document lookup steps from
//! `document_http_api_steps.rs`.

use cucumber::gherkin::Step;
use cucumber::{given, then, when};
use serde_json::Value;

use bdd_infra::rp_harness::OpticalTrainConfig;

use crate::steps::config_rest_steps::{send_put_config, write_scenario_config};
use crate::steps::event_steps::json_values_match;
use crate::steps::focuser_steps::add_focuser;
use crate::steps::rotator_steps::add_offline_camera;
use crate::steps::tool_steps::{
    add_camera, add_filter_wheel, ensure_mcp_client, ensure_omnisim, start_rp,
};
use crate::world::RpWorld;

/// The reference roster + trains from rp.md § Optical Trains: two
/// cameras, two focusers, a rotator, and a filter wheel, a mount
/// carrying the guiding block, and the main/guide trains sharing
/// `main-focuser`. The Alpaca URLs are deliberately *invalid* (not
/// merely unbound): an invalid URL fails client construction before
/// the connect retry loop, so rp starts instantly instead of paying
/// 3-attempt backoff per device — these scenarios only exercise the
/// config endpoints. The guiding service URL stays a well-formed
/// unbound address (it is never dialed at startup).
#[given("a temp rp config with the reference optical trains")]
fn temp_config_reference_trains(world: &mut RpWorld) {
    write_scenario_config(
        world,
        serde_json::json!({
            "cameras": [
                { "id": "main-cam", "alpaca_url": "not-a-url" },
                { "id": "guide-cam", "alpaca_url": "not-a-url" }
            ],
            "focusers": [
                { "id": "main-focuser", "alpaca_url": "not-a-url" },
                { "id": "guide-focuser", "alpaca_url": "not-a-url" }
            ],
            "rotators": [
                { "id": "falcon", "alpaca_url": "not-a-url" }
            ],
            "filter_wheels": [
                { "id": "main-fw", "alpaca_url": "not-a-url" }
            ],
            "cover_calibrators": [
                { "id": "flat-panel", "alpaca_url": "not-a-url" },
                { "id": "dust-cap", "alpaca_url": "not-a-url" }
            ],
            "mount": {
                "alpaca_url": "not-a-url",
                "guiding": { "url": "http://127.0.0.1:1",
                             "focus_watch": { "window": 3,
                                              "degrade_ratio": 1.25 } }
            },
            "optical_trains": [
                { "id": "main", "purpose": "imaging", "focal_length_mm": 1000.0,
                  "devices": ["main-focuser", "main-fw", "falcon", "main-cam"],
                  "auto_focus": { "duration": "3s", "step_size": 100,
                                  "half_width": 1000, "min_area": 4,
                                  "max_area": 500 } },
                { "id": "guide", "purpose": "guiding", "focal_length_mm": 200.0,
                  "devices": ["main-focuser", "guide-focuser", "guide-cam"],
                  "auto_focus": { "step_size": 50, "half_width": 500,
                                  "frames_per_step": 3 } }
            ]
        }),
    );
}

#[given(
    expr = "rp is running with a camera on the simulator in an imaging train with focal length {float}"
)]
async fn rp_with_camera_in_train(world: &mut RpWorld, focal_length_mm: f64) {
    ensure_omnisim(world).await;
    add_camera(world);
    world.optical_trains.push(OpticalTrainConfig {
        aperture_mm: None,
        id: "main".to_string(),
        purpose: Some("imaging".to_string()),
        focal_length_mm: Some(focal_length_mm),
        default_position_angle_degrees: None,
        devices: vec!["main-cam".to_string()],
        auto_focus: None,
    });
    start_rp(world).await;
}

#[given("rp is running with a camera and filter wheel on the simulator in an imaging train")]
async fn rp_with_camera_and_wheel_in_train(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_filter_wheel(world);
    world.optical_trains.push(OpticalTrainConfig {
        aperture_mm: None,
        id: "main".to_string(),
        purpose: Some("imaging".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices: vec!["main-fw".to_string(), "main-cam".to_string()],
        auto_focus: None,
    });
    start_rp(world).await;
}

/// `main` = [flat-panel, main-fw, main-cam] on the simulator — the
/// `get_train_info` fixture (calibrator-flats-provider plan, D4).
#[given(
    "rp is running with a cover calibrator, a filter wheel and a camera on the simulator in an imaging train"
)]
async fn rp_with_calibrator_wheel_and_camera_in_train(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_filter_wheel(world);
    crate::steps::cover_calibrator_steps::add_cover_calibrator(world);
    world.optical_trains.push(OpticalTrainConfig {
        aperture_mm: None,
        id: "main".to_string(),
        purpose: Some("imaging".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices: vec![
            "flat-panel".to_string(),
            "main-fw".to_string(),
            "main-cam".to_string(),
        ],
        auto_focus: None,
    });
    start_rp(world).await;
}

// --- Train optics and the refocus plan (rp.md § Train optics, ---------
// --- § get_refocus_plan Contract) --------------------------------------

/// `focusers[].microns_per_step` for the focuser the running-rp Given
/// below adds; precedes it in the scenario.
#[given(expr = "the focuser is configured with microns_per_step {float}")]
const fn focuser_configured_with_microns_per_step(world: &mut RpWorld, microns: f64) {
    world.focuser_microns_per_step = Some(microns);
}

/// A `{name, wavelength_nm}` entry for the wheel the running-rp Given
/// below adds; precedes it in the scenario.
#[given(expr = "the filter wheel's {string} filter is configured at {float} nm")]
fn filter_configured_at_wavelength(world: &mut RpWorld, name: String, nm: f64) {
    world.filter_wavelengths_nm.push((name, nm));
}

/// `main` = [main-focuser, main-fw, main-cam] on the simulator with
/// the two configured optics facts — the `get_train_info.optics`
/// fixture.
#[given(
    expr = "rp is running with a camera, a focuser and a filter wheel on the simulator in train {string} with focal length {float} and aperture {float}"
)]
async fn rp_with_optics_train(
    world: &mut RpWorld,
    train_id: String,
    focal_length_mm: f64,
    aperture_mm: f64,
) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_focuser(world, None, None, None);
    add_filter_wheel(world);
    world.optical_trains.push(OpticalTrainConfig {
        id: train_id,
        purpose: Some("imaging".to_string()),
        aperture_mm: Some(aperture_mm),
        focal_length_mm: Some(focal_length_mm),
        default_position_angle_degrees: None,
        devices: vec![
            "main-focuser".to_string(),
            "main-fw".to_string(),
            "main-cam".to_string(),
        ],
        auto_focus: None,
    });
    start_rp(world).await;
}

/// The train alone, no rp yet — a scenario that also registers a tool
/// provider starts rp through the provider's Given.
#[given(expr = "a camera and a focuser on the simulator in train {string}")]
async fn camera_and_focuser_in_train(world: &mut RpWorld, train_id: String) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_focuser(world, None, None, None);
    world.optical_trains.push(OpticalTrainConfig {
        id: train_id,
        purpose: Some("imaging".to_string()),
        aperture_mm: None,
        focal_length_mm: Some(1000.0),
        default_position_angle_degrees: None,
        devices: vec!["main-focuser".to_string(), "main-cam".to_string()],
        auto_focus: None,
    });
}

#[given(expr = "the stub focuser reports a step size of {float} microns")]
fn stub_focuser_reports_step_size(world: &mut RpWorld, microns: f64) {
    world
        .alpaca_stub
        .as_ref()
        .expect("no Alpaca stub — add a 'Given a stub Alpaca service hosting a focuser ...' step")
        .set_focuser_step_size(Some(microns));
}

/// The stub-hosted focuser (added by "rp is configured with a focuser
/// on the stub service") ahead of a camera that never connects: the
/// train model resolves, the camera's invariants stay unknown.
#[given(expr = "rp is running with the stub focuser and an offline camera in train {string}")]
async fn rp_with_stub_focuser_train(world: &mut RpWorld, train_id: String) {
    let focuser_id = world
        .focusers
        .first()
        .map(|f| f.id.clone())
        .expect("no focuser — add 'rp is configured with a focuser on the stub service' first");
    add_offline_camera(world, "main-cam");
    world.optical_trains.push(OpticalTrainConfig {
        id: train_id,
        purpose: Some("imaging".to_string()),
        aperture_mm: None,
        focal_length_mm: Some(1000.0),
        default_position_angle_degrees: None,
        devices: vec![focuser_id, "main-cam".to_string()],
        auto_focus: None,
    });
    start_rp(world).await;
}

// `the MCP client calls "get_train_info" with train {string}` is the
// train-addressed step in `cover_calibrator_steps.rs`; it records the
// result the assertions below read.

#[when(expr = "the MCP client calls \"get_refocus_plan\" with train {string}")]
async fn mcp_call_get_refocus_plan(world: &mut RpWorld, train_id: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "get_refocus_plan",
            serde_json::json!({ "train_id": train_id }),
        )
        .await;
    world.last_tool_result = Some(result);
}

/// A regex step so the expected value can be any JSON — a number,
/// `null`, a boolean, or a nested object or array — addressed by an
/// RFC-6901 pointer into the last tool result.
#[then(regex = r#"^the tool result at "([^"]+)" should be the JSON (.+)$"#)]
fn tool_result_at_is_json(world: &mut RpWorld, pointer: String, expected: String) {
    let expected: Value = serde_json::from_str(&expected).expect("the expected value must be JSON");
    let result = last_tool_success(world);
    let actual = result
        .pointer(&pointer)
        .unwrap_or_else(|| panic!("no value at {pointer} in the tool result: {result}"));
    assert!(
        json_values_match(actual, &expected),
        "expected {pointer} to be {expected}, got {actual} (result: {result})"
    );
}

/// The `optics` block's own inputs must reproduce its derived scale,
/// the same identity the exposure document's block is held to.
#[then(
    "the tool result optics pixel scale should equal 206.265 times pixel size over focal length"
)]
fn tool_result_optics_pixel_scale_consistent(world: &mut RpWorld) {
    let result = last_tool_success(world);
    let optics = result
        .get("optics")
        .unwrap_or_else(|| panic!("no optics block in the tool result: {result}"));
    let pixel_size = optics_f64(optics, "pixel_size_um");
    let focal_length = optics_f64(optics, "focal_length_mm");
    let scale = optics_f64(optics, "pixel_scale_arcsec_per_pixel");
    let expected = 206.265 * pixel_size / focal_length;
    assert!(
        (scale - expected).abs() < 1e-9,
        "pixel_scale_arcsec_per_pixel {scale} != 206.265 × {pixel_size} / {focal_length} = {expected}"
    );
}

fn last_tool_success(world: &RpWorld) -> &Value {
    world
        .last_tool_result
        .as_ref()
        .expect("no tool result — add a 'When the MCP client calls ...' step first")
        .as_ref()
        .unwrap_or_else(|e| panic!("the tool call failed: {e}"))
}

#[when(expr = "the MCP client calls \"capture\" with train {string} for {int} ms")]
async fn mcp_call_capture_train(world: &mut RpWorld, train_id: String, duration_ms: i32) {
    call_capture(
        world,
        serde_json::json!({
            "train_id": train_id,
            "duration": format!("{duration_ms}ms"),
        }),
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"capture\" with both camera {string} and train {string} for {int} ms"
)]
async fn mcp_call_capture_camera_and_train(
    world: &mut RpWorld,
    camera_id: String,
    train_id: String,
    duration_ms: i32,
) {
    call_capture(
        world,
        serde_json::json!({
            "camera_id": camera_id,
            "train_id": train_id,
            "duration": format!("{duration_ms}ms"),
        }),
    )
    .await;
}

async fn call_capture(world: &mut RpWorld, args: Value) {
    ensure_mcp_client(world).await;
    let result = world.mcp().call_tool("capture", args).await;
    if let Ok(ref v) = result {
        world.last_document_id = v
            .get("document_id")
            .and_then(Value::as_str)
            .map(String::from);
    }
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"set_filter\" with train {string} and filter {string}")]
async fn mcp_call_set_filter_train(world: &mut RpWorld, train_id: String, filter_name: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "set_filter",
            serde_json::json!({
                "train_id": train_id,
                "filter_name": filter_name,
            }),
        )
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "I PUT \\/api\\/config with the fetched config after setting {string} to:")]
async fn put_config_with_pointer_set_docstring(world: &mut RpWorld, pointer: String, step: &Step) {
    let raw = step
        .docstring()
        .expect("this step needs a docstring with the JSON value")
        .trim();
    let value: Value =
        serde_json::from_str(raw).expect("the docstring must be valid JSON for this step");
    let mut config = world
        .fetched_config
        .clone()
        .expect("no fetched config — add a 'When I GET /api/config' step first");
    *config
        .pointer_mut(&pointer)
        .unwrap_or_else(|| panic!("pointer {pointer} not present in fetched config")) = value;
    send_put_config(world, config.to_string()).await;
}

/// Insert a key that the fetched config does not carry (the retired-key
/// scenarios): `pointer_mut` on the full pointer would fail, so resolve
/// the parent object and insert the final segment into it.
///
/// The value is the rest of the line, parsed as JSON. A `{string}`
/// parameter cannot carry the quotes inside a JSON object or around a
/// JSON string, so rows inserting either would match nothing.
#[when(
    regex = r#"^I PUT /api/config with the fetched config after inserting "([^"]+)" set to the JSON (.+)$"#
)]
async fn put_config_with_pointer_inserted(world: &mut RpWorld, pointer: String, raw: String) {
    let value: Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("step value is not JSON ({e}): {raw}"));
    let mut config = world
        .fetched_config
        .clone()
        .expect("no fetched config — add a 'When I GET /api/config' step first");
    let (parent, key) = pointer
        .rsplit_once('/')
        .unwrap_or_else(|| panic!("pointer {pointer} has no '/'"));
    let parent_value = if parent.is_empty() {
        &mut config
    } else {
        config
            .pointer_mut(parent)
            .unwrap_or_else(|| panic!("parent pointer {parent} not present in fetched config"))
    };
    parent_value
        .as_object_mut()
        .unwrap_or_else(|| panic!("parent at {parent} is not a JSON object"))
        .insert(key.to_string(), value);
    send_put_config(world, config.to_string()).await;
}

#[then(expr = "the config response body should contain {string}")]
fn config_response_body_contains(world: &mut RpWorld, needle: String) {
    let body = world
        .last_config_response_text
        .as_ref()
        .expect("no config response recorded — check the request step ran");
    assert!(
        body.contains(&needle),
        "expected the response body to contain {needle:?}; body was: {body}"
    );
}

#[then(expr = "the document body should not contain {string}")]
fn document_body_lacks_field(world: &mut RpWorld, field: String) {
    let body = document_body(world);
    assert!(
        body.get(&field).is_none(),
        "expected no '{field}' in document body, got: {:?}",
        body.get(&field)
    );
}

#[then(expr = "the document optics focal length should be {float}")]
fn document_optics_focal_length(world: &mut RpWorld, expected: f64) {
    let optics = document_optics(world);
    assert_eq!(
        optics.get("focal_length_mm").and_then(Value::as_f64),
        Some(expected),
        "unexpected optics.focal_length_mm; optics was: {optics}"
    );
}

/// Self-consistency of the documented derivation: each axis's
/// `pixel_scale_*_arcsec_per_pixel` equals
/// `206.265 × pixel_size_*_um / focal_length_mm` computed from the
/// document's own fields.
#[then("the document optics pixel scale should equal 206.265 times pixel size over focal length")]
fn document_optics_pixel_scale_consistent(world: &mut RpWorld) {
    let optics = document_optics(world).clone();
    let focal_length = optics_f64(&optics, "focal_length_mm");
    for axis in ["x", "y"] {
        let pixel_size = optics_f64(&optics, &format!("pixel_size_{axis}_um"));
        let scale = optics_f64(&optics, &format!("pixel_scale_{axis}_arcsec_per_pixel"));
        let expected = 206.265 * pixel_size / focal_length;
        assert!(
            (scale - expected).abs() < 1e-9,
            "pixel_scale_{axis}: expected {expected}, got {scale}; optics was: {optics}"
        );
    }
}

const fn document_body(world: &RpWorld) -> &Value {
    world
        .last_document_response_body
        .as_ref()
        .expect("no document response body recorded")
}

fn document_optics(world: &RpWorld) -> &Value {
    document_body(world).get("optics").unwrap_or_else(|| {
        panic!(
            "document carries no optics block: {:?}",
            document_body(world)
        )
    })
}

fn optics_f64(optics: &Value, field: &str) -> f64 {
    optics
        .get(field)
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("optics field {field} missing or non-numeric in {optics}"))
}
