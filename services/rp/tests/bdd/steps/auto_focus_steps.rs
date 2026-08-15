//! BDD step definitions for the `auto_focus` MCP tool.
//!
//! Shared steps live in `tool_steps.rs`:
//! - `the MCP client lists available tools`
//! - `the tool list should include {string}`
//! - `the tool call should return an error`
//! - `the error message should contain {string}`
//! - `an MCP client connected to rp`
//!
//! Focuser-config helpers (the `add_focuser` builder) are reused from
//! `focuser_steps.rs`. The auto_focus-specific Given steps below add a
//! camera *alongside* the focuser, which the focuser-only steps in
//! `focuser_steps.rs` don't do — that's the only reason this module
//! defines its own composition steps rather than reusing
//! `rp_running_with_focuser` directly.

use cucumber::{given, then, when};
use serde_json::{Map, Value};

use bdd_infra::rp_harness::{CameraConfig, FocuserConfig};

use crate::steps::focuser_steps::add_focuser;
use crate::steps::tool_steps::{add_camera, ensure_mcp_client, ensure_omnisim, start_rp};
use crate::world::RpWorld;

// --- Given steps: equipment composition unique to auto_focus ---

#[given("rp is running with a camera and a focuser on the simulator")]
async fn rp_with_camera_and_focuser(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_focuser(world, None, None);
    start_rp(world).await;
}

#[given(
    expr = "rp is running with a camera and a focuser on the simulator with bounds {int}..{int}"
)]
async fn rp_with_camera_and_bounded_focuser(world: &mut RpWorld, min: i32, max: i32) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_focuser(world, Some(min), Some(max));
    start_rp(world).await;
}

#[given("rp is running with a camera on the simulator and an unreachable focuser")]
async fn rp_with_camera_and_unreachable_focuser(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    world.focusers.push(FocuserConfig {
        id: "main-focuser".to_string(),
        alpaca_url: "http://127.0.0.1:1".to_string(),
        device_number: 0,
        min_position: None,
        max_position: None,
    });
    start_rp(world).await;
}

#[given("rp is running with a focuser on the simulator and an unreachable camera")]
async fn rp_with_focuser_and_unreachable_camera(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_focuser(world, None, None);
    world.cameras.push(CameraConfig {
        id: "main-cam".to_string(),
        alpaca_url: "http://127.0.0.1:1".to_string(),
        device_number: 0,
        cooler_targets_c: Vec::new(),
    });
    start_rp(world).await;
}

// --- When steps ---

#[when(
    expr = "the MCP client calls auto_focus with focuser {string} camera {string} duration {string} step_size {int} half_width {int} min_area {int} max_area {int}"
)]
#[allow(clippy::too_many_arguments)]
async fn mcp_call_auto_focus_full(
    world: &mut RpWorld,
    focuser_id: String,
    camera_id: String,
    duration: String,
    step_size: i64,
    half_width: i64,
    min_area: i64,
    max_area: i64,
) {
    let mut args = baseline_args();
    args.insert("focuser_id".into(), Value::String(focuser_id));
    args.insert("camera_id".into(), Value::String(camera_id));
    args.insert("duration".into(), Value::String(duration));
    args.insert("step_size".into(), Value::from(step_size));
    args.insert("half_width".into(), Value::from(half_width));
    args.insert("min_area".into(), Value::from(min_area));
    args.insert("max_area".into(), Value::from(max_area));
    call_auto_focus(world, args).await;
}

#[when(expr = "the MCP client calls auto_focus with camera {string} and focuser {string}")]
async fn mcp_call_auto_focus_devices(world: &mut RpWorld, camera_id: String, focuser_id: String) {
    let mut args = baseline_args();
    args.insert("camera_id".into(), Value::String(camera_id));
    args.insert("focuser_id".into(), Value::String(focuser_id));
    call_auto_focus(world, args).await;
}

#[when(expr = "the MCP client calls auto_focus omitting {string}")]
async fn mcp_call_auto_focus_omitting(world: &mut RpWorld, missing_param: String) {
    let mut args = baseline_args();
    args.remove(missing_param.as_str());
    call_auto_focus(world, args).await;
}

#[when(expr = "the MCP client calls auto_focus with step_size {int}")]
async fn mcp_call_auto_focus_with_step_size(world: &mut RpWorld, step_size: i64) {
    let mut args = baseline_args();
    args.insert("step_size".into(), Value::from(step_size));
    call_auto_focus(world, args).await;
}

#[when(expr = "the MCP client calls auto_focus with half_width {int}")]
async fn mcp_call_auto_focus_with_half_width(world: &mut RpWorld, half_width: i64) {
    let mut args = baseline_args();
    args.insert("half_width".into(), Value::from(half_width));
    call_auto_focus(world, args).await;
}

#[when(expr = "the MCP client calls auto_focus with min_fit_points {int}")]
async fn mcp_call_auto_focus_with_min_fit_points(world: &mut RpWorld, min_fit_points: i64) {
    let mut args = baseline_args();
    args.insert("min_fit_points".into(), Value::from(min_fit_points));
    call_auto_focus(world, args).await;
}

// Train addressing: bare `train_id` (sweep parameters come from the
// train's auto_focus config block), a per-call override on top, and
// the mutually-exclusive combination with an explicit device id.

#[when(expr = "the MCP client calls auto_focus with train {string}")]
async fn mcp_call_auto_focus_with_train(world: &mut RpWorld, train_id: String) {
    let mut args = Map::new();
    args.insert("train_id".into(), Value::String(train_id));
    call_auto_focus(world, args).await;
}

#[when(expr = "the MCP client calls auto_focus with train {string} and step_size {int}")]
async fn mcp_call_auto_focus_with_train_and_step(
    world: &mut RpWorld,
    train_id: String,
    step_size: i64,
) {
    let mut args = Map::new();
    args.insert("train_id".into(), Value::String(train_id));
    args.insert("step_size".into(), Value::from(step_size));
    call_auto_focus(world, args).await;
}

#[when(expr = "the MCP client calls auto_focus with train {string} and camera {string}")]
async fn mcp_call_auto_focus_with_train_and_camera(
    world: &mut RpWorld,
    train_id: String,
    camera_id: String,
) {
    let mut args = Map::new();
    args.insert("train_id".into(), Value::String(train_id));
    args.insert("camera_id".into(), Value::String(camera_id));
    call_auto_focus(world, args).await;
}

// --- Then steps ---

#[then(expr = "{int} FITS files should exist in the pinned data directory")]
async fn fits_files_in_pinned_dir(world: &mut RpWorld, expected: usize) {
    let dir = world
        .pinned_data_directory
        .as_ref()
        .expect("data_directory not pinned — add the 'pinned to a fresh tempdir' step");
    let count = count_extension(dir, "fits").await;
    assert_eq!(
        count, expected,
        "expected {expected} FITS files in {dir}, found {count}"
    );
}

#[then(expr = "every sidecar JSON in the pinned data directory should contain an {string} section")]
async fn every_sidecar_has_section(world: &mut RpWorld, section_name: String) {
    let dir = world
        .pinned_data_directory
        .as_ref()
        .expect("data_directory not pinned");
    let sidecars = read_sidecars(dir).await;
    assert!(
        !sidecars.is_empty(),
        "no .json sidecars in {dir} — sweep did not run"
    );
    for (path, body) in &sidecars {
        let sections = body.get("sections").unwrap_or_else(|| {
            panic!(
                "sidecar {} has no 'sections' field, body: {body:?}",
                path.display()
            )
        });
        assert!(
            sections.get(&section_name).is_some(),
            "sidecar {} missing '{section_name}' section, sections: {sections:?}",
            path.display()
        );
    }
}

#[then(expr = "no sidecar JSON in the pinned data directory should contain an {string} section")]
async fn no_sidecar_has_section(world: &mut RpWorld, section_name: String) {
    let dir = world
        .pinned_data_directory
        .as_ref()
        .expect("data_directory not pinned");
    for (path, body) in read_sidecars(dir).await {
        let sections = body.get("sections").unwrap_or(&Value::Null);
        assert!(
            sections.get(&section_name).is_none(),
            "sidecar {} unexpectedly contains '{}' section: {:?}",
            path.display(),
            section_name,
            sections.get(&section_name)
        );
    }
}

// --- Helpers ---

/// Baseline arg map representing a "valid" `auto_focus` call against the
/// default camera + focuser. Individual scenarios mutate this baseline
/// (insert override / remove field) before dispatching, so that the
/// missing-parameter scenarios can each carve out exactly one field
/// without re-stating the whole arg list.
fn baseline_args() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("camera_id".into(), Value::String("main-cam".into()));
    m.insert("focuser_id".into(), Value::String("main-focuser".into()));
    m.insert("duration".into(), Value::String("100ms".into()));
    m.insert("step_size".into(), Value::from(100));
    m.insert("half_width".into(), Value::from(200));
    m.insert("min_area".into(), Value::from(5));
    m.insert("max_area".into(), Value::from(65_536));
    m
}

async fn call_auto_focus(world: &mut RpWorld, args: Map<String, Value>) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("auto_focus", Value::Object(args))
        .await;
    match &result {
        Ok(v) => world.last_auto_focus_result = Some(v.clone()),
        Err(_) => world.last_auto_focus_result = None,
    }
    world.last_tool_result = Some(result);
}

async fn count_extension(dir: &str, ext: &str) -> usize {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .unwrap_or_else(|e| panic!("failed to read pinned data directory {dir:?}: {e}"));
    let mut count = 0usize;
    while let Some(entry) = entries.next_entry().await.expect("read_dir entry") {
        if entry.path().extension().and_then(|s| s.to_str()) == Some(ext) {
            count += 1;
        }
    }
    count
}

async fn read_sidecars(dir: &str) -> Vec<(std::path::PathBuf, Value)> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .unwrap_or_else(|e| panic!("failed to read pinned data directory {dir:?}: {e}"));
    let mut out = Vec::new();
    while let Some(entry) = entries.next_entry().await.expect("read_dir entry") {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = tokio::fs::read(&path)
            .await
            .unwrap_or_else(|e| panic!("failed to read sidecar {}: {e}", path.display()));
        let body: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("failed to parse sidecar {}: {e}", path.display()));
        out.push((path, body));
    }
    out
}
