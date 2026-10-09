//! BDD step definitions for same-camera capture serialization
//! (`capture_concurrency.feature`, rp.md § Capture Tool Details,
//! "Binning" → Concurrency).
//!
//! The overlap is driven from two MCP sessions, the same way
//! `motion_gate.feature` drives the gate: the background capture runs
//! on a second session via `spawn_background_call`, and the scenario
//! puts its `exposure_started` event between that spawn and the
//! foreground capture as the barrier proving the background capture
//! holds the camera.

use cucumber::when;
use serde_json::json;

use crate::steps::document_http_api_steps::fetch_document;
use crate::steps::motion_gate_steps::spawn_background_call;
use crate::world::RpWorld;

#[when(
    expr = "a second MCP client starts a {string} capture of camera {string} at binning {string} in the background"
)]
async fn start_binned_capture_in_background(
    world: &mut RpWorld,
    duration: String,
    camera_id: String,
    binning: String,
) {
    spawn_background_call(
        world,
        "capture",
        json!({ "camera_id": camera_id, "duration": duration, "binning": binning }),
    )
    .await;
}

#[when("I fetch the document for the background capture")]
async fn fetch_background_capture_document(world: &mut RpWorld) {
    let result = world.last_background_result.as_ref().expect(
        "no background call joined yet — add 'the background \"capture\" call should succeed' first",
    );
    let document_id = result
        .get("document_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("the background call returned no document_id: {result}"))
        .to_string();
    fetch_document(world, &document_id).await;
}
