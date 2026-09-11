//! BDD step definitions for `capture`'s `binning` parameter and the
//! frame geometry rp writes before every exposure
//! (`capture_binning.feature`, rp.md § Capture Tool Details,
//! "Binning").

use cucumber::{given, then, when};

use bdd_infra::rp_harness::OmniSimHandle;

use crate::steps::tool_steps::ensure_mcp_client;
use crate::world::RpWorld;

// ---------------------------------------------------------------------------
// Given
// ---------------------------------------------------------------------------

#[given(expr = "another client has binned the simulator camera to {string}")]
async fn foreign_client_binned_the_camera(_world: &mut RpWorld, binning: String) {
    let (bin_x, bin_y) = split_binning(&binning);
    OmniSimHandle::set_camera_binning(bin_x, bin_y)
        .await
        .expect("failed to bin the simulator camera");
}

#[given(expr = "another client has cropped the simulator camera to a {int} by {int} subframe")]
async fn foreign_client_cropped_the_camera(_world: &mut RpWorld, num_x: u32, num_y: u32) {
    OmniSimHandle::set_camera_subframe(100, 100, num_x, num_y)
        .await
        .expect("failed to crop the simulator camera");
}

/// `"2x2"` → `(2, 2)`. Panics on anything else: a scenario that writes
/// a malformed binning into an arranging step has a typo, not a
/// contract to exercise (the malformed-input contract is exercised
/// through the tool call itself).
fn split_binning(binning: &str) -> (u8, u8) {
    let (x, y) = binning
        .split_once('x')
        .unwrap_or_else(|| panic!("binning {binning:?} is not AxB"));
    (
        x.parse().expect("binning x factor"),
        y.parse().expect("binning y factor"),
    )
}

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
    expr = "the MCP client calls \"capture\" with camera {string} for {int} ms at binning {string}"
)]
async fn mcp_call_capture_binning(
    world: &mut RpWorld,
    camera_id: String,
    duration_ms: i32,
    binning: String,
) {
    call_capture(
        world,
        serde_json::json!({
            "camera_id": camera_id,
            "duration": format!("{}ms", duration_ms),
            "binning": binning,
        }),
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"capture\" with camera {string} for {int} ms at binning {string} and frame_type {string}"
)]
async fn mcp_call_capture_binning_frame_type(
    world: &mut RpWorld,
    camera_id: String,
    duration_ms: i32,
    binning: String,
    frame_type: String,
) {
    call_capture(
        world,
        serde_json::json!({
            "camera_id": camera_id,
            "duration": format!("{}ms", duration_ms),
            "binning": binning,
            "frame_type": frame_type,
        }),
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"capture\" with camera {string} for {int} ms at binning {string}, the added target, and frame_type {string}"
)]
async fn mcp_call_capture_binning_target_frame_type(
    world: &mut RpWorld,
    camera_id: String,
    duration_ms: i32,
    binning: String,
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
            "binning": binning,
            "target": target,
            "frame_type": frame_type,
        }),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

#[then(expr = "the captured frame should be {int} by {int} pixels")]
fn captured_frame_dimensions(world: &mut RpWorld, width: u64, height: u64) {
    let doc = world
        .last_document_response_body
        .as_ref()
        .expect("no document fetched — add an 'I fetch the document ...' step first");
    let actual_width = doc
        .get("width")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| panic!("document has no numeric width: {doc}"));
    let actual_height = doc
        .get("height")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| panic!("document has no numeric height: {doc}"));
    assert_eq!(
        (actual_width, actual_height),
        (width, height),
        "captured frame size mismatch — the binning or the subframe rp wrote did not take"
    );
}
