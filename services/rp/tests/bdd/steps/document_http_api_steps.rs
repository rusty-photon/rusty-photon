//! BDD step definitions for the document HTTP API:
//! `GET /api/documents/{document_id}`.
//!
//! Also hosts the lifecycle/configuration steps the cross-restart and
//! eviction scenarios rely on (`data_directory` pinning, imaging cache
//! overrides, rp restart). These don't fit cleanly under any other
//! existing step module — they're specific to exercising the
//! "live as long as the file is on disk" contract.

use cucumber::{given, then, when};
use serde_json::Value;

use crate::steps::tool_steps::start_rp;
use crate::world::RpWorld;

// --- Given steps: harness configuration ---

#[given("rp's data_directory is pinned to a fresh tempdir")]
fn pin_data_directory(world: &mut RpWorld) {
    let dir = tempfile::tempdir().expect("failed to create tempdir for pinned data_directory");
    world.pinned_data_directory = Some(dir.path().to_string_lossy().into_owned());
    world.pinned_data_dir_holder = Some(dir);
}

#[given(expr = "rp's image cache holds at most {int} image")]
#[given(expr = "rp's image cache holds at most {int} images")]
const fn pin_imaging_cache_max_images(world: &mut RpWorld, max_images: usize) {
    // A generous MiB budget keeps the image-count cap as the operative
    // limit — exactly what the eviction scenario wants.
    world.pinned_imaging_overrides = Some((1024, max_images));
}

// --- When steps: lifecycle ---

#[when("rp is restarted")]
async fn restart_rp(world: &mut RpWorld) {
    // Drop the MCP client first — its session is bound to the old port,
    // and rp's graceful shutdown blocks on its own MCP sessions
    // terminating. Matches the pattern documented in
    // docs/skills/testing.md.
    world.mcp_client = None;
    if let Some(mut handle) = world.rp.take() {
        handle.stop().await;
    }
    start_rp(world).await;
}

// --- When steps: document lookup ---

#[when("I fetch the document for the captured document_id")]
async fn fetch_document_captured(world: &mut RpWorld) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");
    fetch_document(world, &document_id).await;
}

#[when(expr = "I fetch the document for document_id {string}")]
async fn fetch_document_explicit(world: &mut RpWorld, document_id: String) {
    fetch_document(world, &document_id).await;
}

#[when(expr = "I remember the captured document_id as {string}")]
fn remember_captured_document_id(world: &mut RpWorld, name: String) {
    let id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");
    world.remembered_document_ids.insert(name, id);
}

#[when(expr = "I fetch the document for remembered document_id {string}")]
async fn fetch_document_remembered(world: &mut RpWorld, name: String) {
    let document_id = world
        .remembered_document_ids
        .get(&name)
        .cloned()
        .unwrap_or_else(|| panic!("no remembered document_id under name {name:?}"));
    fetch_document(world, &document_id).await;
}

pub async fn fetch_document(world: &mut RpWorld, document_id: &str) {
    let url = format!("{}/api/documents/{}", world.rp_url(), document_id);
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /api/documents/{id}");
    world.last_document_response_status = Some(resp.status().as_u16());
    world.last_document_response_body = resp.json::<Value>().await.ok();
}

// --- Then steps ---

#[then(expr = "the document response status should be {int}")]
fn document_status_eq(world: &mut RpWorld, expected: u16) {
    let status = world
        .last_document_response_status
        .expect("no document response recorded");
    assert_eq!(status, expected, "unexpected document status");
}

#[then(expr = "the document body should contain {string}")]
fn document_body_contains_field(world: &mut RpWorld, field: String) {
    let body = body_or_panic(world);
    assert!(
        body.get(&field).is_some(),
        "expected '{field}' in document body, got: {body:?}"
    );
}

#[then(expr = "the document sections should contain {string}")]
fn document_sections_contains(world: &mut RpWorld, section: String) {
    let body = body_or_panic(world);
    let sections = body
        .get("sections")
        .unwrap_or_else(|| panic!("expected 'sections' in document body, got: {body:?}"));
    assert!(
        sections.get(&section).is_some(),
        "expected section '{section}' in document body sections, got: {sections:?}"
    );
}

// --- Helpers ---

const fn body_or_panic(world: &RpWorld) -> &Value {
    world
        .last_document_response_body
        .as_ref()
        .expect("no document response body recorded")
}
