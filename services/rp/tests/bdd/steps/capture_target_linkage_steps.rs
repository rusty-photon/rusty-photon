//! BDD step definitions for `capture`'s `target`/`frame_type`
//! parameters (`capture_target_linkage.feature`, rp.md § Capture Tool
//! Details, rp-targets.md § File-naming template — Decision 11).

use cucumber::{given, then, when};

use bdd_infra::rp_harness::OpticalTrainConfig;

use crate::steps::tool_steps::{add_camera, add_filter_wheel, ensure_mcp_client, ensure_omnisim};
use crate::world::RpWorld;

const DEFAULT_PATTERN: &str =
    "{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_fpos_{filter_position}_{sensor_temp}_{uuid8}";

// ---------------------------------------------------------------------------
// Given
// ---------------------------------------------------------------------------

fn configure_naming_templates(world: &mut RpWorld) {
    world.file_naming_pattern = Some(DEFAULT_PATTERN.to_string());
    // `directory_pattern` is left unset — the documented default
    // ("{target}/{night_date}/{frame_type}") applies whenever
    // `file_naming_pattern` is configured.
    world.site = Some((47.6062, -122.3321));
}

#[given("rp is running with a capture rig and naming templates configured")]
async fn rp_running_with_capture_rig(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_filter_wheel(world);
    world.optical_trains.push(OpticalTrainConfig {
        id: "main".to_string(),
        purpose: None,
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices: vec!["main-fw".to_string(), "main-cam".to_string()],
        auto_focus: None,
    });
    configure_naming_templates(world);
    crate::steps::tool_steps::start_rp(world).await;
}

#[given("rp is running with a capture rig and no naming templates configured")]
async fn rp_running_with_capture_rig_no_templates(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_filter_wheel(world);
    world.optical_trains.push(OpticalTrainConfig {
        id: "main".to_string(),
        purpose: None,
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices: vec!["main-fw".to_string(), "main-cam".to_string()],
        auto_focus: None,
    });
    world.site = Some((47.6062, -122.3321));
    // `RpConfigBuilder::build` bakes in the documented default pattern
    // unconditionally (most scenarios *want* that) — this scenario
    // needs it genuinely absent, so force-clear it.
    world.clear_file_naming_pattern = true;
    crate::steps::tool_steps::start_rp(world).await;
}

#[given("rp is running with a filter-wheel-less capture rig and naming templates configured")]
async fn rp_running_with_filterless_capture_rig(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    // No filter wheel, no train at all: `capture`'s live-filter lookup
    // finds no train for the camera and falls back to "NA"/0.
    configure_naming_templates(world);
    crate::steps::tool_steps::start_rp(world).await;
}

// `the MCP client has added a target named ... at ra_hours ... dec_degrees
// ...` is defined once, in `target_store_crud_steps.rs` — cucumber matches
// step text globally across every steps file in the binary, so redefining
// identical text here would be an ambiguous-match error, not a harmless
// duplicate.

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

async fn call_capture(world: &mut RpWorld, args: serde_json::Value) {
    ensure_mcp_client(world).await;
    let result = world.mcp().call_tool("capture", args).await;
    if let Ok(ref v) = result {
        world.last_image_path = v
            .get("image_path")
            .and_then(|v| v.as_str())
            .map(String::from);
        world.last_document_id = v
            .get("document_id")
            .and_then(|v| v.as_str())
            .map(String::from);
    }
    world.last_tool_result = Some(result);
}

#[when(
    expr = "the MCP client calls \"capture\" with camera {string} for {int} ms and frame_type {string}"
)]
async fn mcp_call_capture_frame_type(
    world: &mut RpWorld,
    camera_id: String,
    duration_ms: i32,
    frame_type: String,
) {
    call_capture(
        world,
        serde_json::json!({
            "camera_id": camera_id,
            "duration": format!("{}ms", duration_ms),
            "frame_type": frame_type,
        }),
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"capture\" with camera {string} for {int} ms, target {string}, and frame_type {string}"
)]
async fn mcp_call_capture_target_frame_type(
    world: &mut RpWorld,
    camera_id: String,
    duration_ms: i32,
    target: String,
    frame_type: String,
) {
    call_capture(
        world,
        serde_json::json!({
            "camera_id": camera_id,
            "duration": format!("{}ms", duration_ms),
            "target": target,
            "frame_type": frame_type,
        }),
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"capture\" with camera {string} for {int} ms, the added target, and frame_type {string}"
)]
async fn mcp_call_capture_added_target_frame_type(
    world: &mut RpWorld,
    camera_id: String,
    duration_ms: i32,
    frame_type: String,
) {
    let target = world
        .last_target_slug
        .clone()
        .expect("no target added yet — add one via 'the MCP client has added a target ...' first");
    call_capture(
        world,
        serde_json::json!({
            "camera_id": camera_id,
            "duration": format!("{}ms", duration_ms),
            "target": target,
            "frame_type": frame_type,
        }),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

// `the tool call should succeed` is defined once, in
// `cover_calibrator_steps.rs` — see the ambiguous-match note above.

fn captured_image_path(world: &RpWorld) -> &str {
    world
        .last_image_path
        .as_deref()
        .expect("no captured image_path recorded — did the capture call succeed?")
}

#[then("the captured image_path should be a flat uuid8-named .fits file")]
fn captured_path_is_flat_uuid8(world: &mut RpWorld) {
    let path = captured_image_path(world);
    let file_name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let is_flat_uuid8 = file_name.len() == 13
        && std::path::Path::new(file_name).extension() == Some(std::ffi::OsStr::new("fits"))
        && file_name
            .get(..8)
            .is_some_and(|uuid8| uuid8.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(
        is_flat_uuid8,
        "expected image_path {path:?} to be today's flat <uuid8>.fits (no target/frame_type \
         subdirectories), got file name {file_name:?}"
    );
}

#[then(expr = "the captured image_path should contain {string}")]
fn captured_path_contains(world: &mut RpWorld, needle: String) {
    // `directory_pattern`'s literal `/` separators stay literal, but the
    // outer join against `data_directory` uses the OS separator — on
    // Windows that mixes `\` and `/` in one path. Windows treats both
    // interchangeably for real I/O, so normalize here for the
    // assertion; this is a test-only concern, not production behavior.
    let path = captured_image_path(world).replace('\\', "/");
    assert!(
        path.contains(&needle),
        "expected image_path {path:?} to contain {needle:?}"
    );
}

#[then("the captured image_path should exist on disk")]
fn captured_path_exists(world: &mut RpWorld) {
    let path = captured_image_path(world);
    assert!(
        std::path::Path::new(path).exists(),
        "expected a file at {path:?}"
    );
}

const fn last_document(world: &RpWorld) -> &serde_json::Value {
    world
        .last_document_response_body
        .as_ref()
        .expect("no document fetched — add an 'I fetch the document ...' step first")
}

#[then(expr = "the document field {string} should be {string}")]
fn document_field_string(world: &mut RpWorld, field: String, expected: String) {
    let doc = last_document(world);
    let actual = doc
        .get(&field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("document field '{field}' missing or not a string: {doc}"));
    assert_eq!(actual, expected, "document field '{field}' mismatch");
}

#[then(expr = "the document's target slug should be {string}")]
fn document_target_slug(world: &mut RpWorld, expected: String) {
    let doc = last_document(world);
    let actual = doc
        .get("target")
        .and_then(|t| t.get("slug"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("document has no target.slug: {doc}"));
    assert_eq!(actual, expected, "document target.slug mismatch");
}

#[then(expr = "the document's target display_name should be {string}")]
fn document_target_display_name(world: &mut RpWorld, expected: String) {
    let doc = last_document(world);
    let actual = doc
        .get("target")
        .and_then(|t| t.get("display_name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("document has no target.display_name: {doc}"));
    assert_eq!(actual, expected, "document target.display_name mismatch");
}

#[then("the document's target display_name should be absent")]
fn document_target_display_name_absent(world: &mut RpWorld) {
    let doc = last_document(world);
    let target = doc
        .get("target")
        .unwrap_or_else(|| panic!("document has no target: {doc}"));
    assert!(
        target.get("display_name").is_none(),
        "expected target.display_name to be absent, got: {target}"
    );
}
