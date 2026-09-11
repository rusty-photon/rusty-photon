//! BDD step definitions for the `center_on_target` MCP tool.
//!
//! Shared steps live in `tool_steps.rs`:
//! - `the MCP client lists available tools`
//! - `the tool list should include {string}`
//! - `the tool call should return an error`
//! - `the error message should contain {string}`
//! - `the tool call should succeed` (in `cover_calibrator_steps.rs`)
//! - `an MCP client connected to rp`
//!
//! Shared steps live in `mount_steps.rs`:
//! - `the mount tracking is set to {word}`
//! - `the MCP client calls "sync_mount" with ra {string} dec {string}`
//! - `the MCP client calls "get_mount_position"`
//!
//! Shared steps live in `plate_solve_steps.rs`:
//! - `a stub plate solver returning a canned WCS`
//! - `a stub plate solver returning error code {string} with message {string}`
//! - `rp is running with a camera and a mount on the simulator`
//!
//! Shared FITS / sidecar inspection steps live in `auto_focus_steps.rs`:
//! - `{int} FITS files should exist in the pinned data directory`
//! - `every sidecar JSON in the pinned data directory should contain an {string} section`
//!
//! `rp's data_directory is pinned to a fresh tempdir` is in
//! `document_http_api_steps.rs`.

use cucumber::gherkin::Step;
use cucumber::{given, then, when};
use serde_json::{Map, Value};

use bdd_infra::rp_harness::{
    CameraConfig, CannedWcs, MountConfig, PlateSolverConfig, PlateSolverStub, StubBehavior,
};

use crate::steps::tool_steps::{add_camera, ensure_mcp_client, ensure_omnisim, start_rp};
use crate::world::RpWorld;

// --- Given steps: stub variants unique to center_on_target ---

/// Multi-iteration scenarios need different solved coordinates per
/// solve call so the residual can decrease across iterations. The
/// stub walks the table top-to-bottom; once the queue is exhausted
/// it keeps returning the final entry (so a scenario can pin the
/// "convergent" WCS without counting exact iterations).
#[given("a stub plate solver returning these per-call WCS responses:")]
async fn stub_plate_solver_sequence(world: &mut RpWorld, step: &Step) {
    let table = step
        .table
        .as_ref()
        .expect("step requires a data table of WCS responses");
    let mut responses = Vec::new();
    for row in table.rows.iter().skip(1) {
        assert_eq!(
            row.len(),
            2,
            "table row must have ra_center, dec_center: {row:?}"
        );
        let ra_center: f64 = row[0]
            .parse()
            .unwrap_or_else(|_| panic!("ra_center must be f64, got {}", row[0]));
        let dec_center: f64 = row[1]
            .parse()
            .unwrap_or_else(|_| panic!("dec_center must be f64, got {}", row[1]));
        responses.push(CannedWcs {
            ra_center,
            dec_center,
            pixel_scale_arcsec: 1.05,
            rotation_deg: 12.3,
            solver: "stub-astap-1.0".to_string(),
            wcs_matrix: None,
        });
    }
    let stub = PlateSolverStub::start(StubBehavior::Sequence(responses)).await;
    world.plate_solver = Some(PlateSolverConfig {
        url: stub.url.clone(),
        timeout: None,
        default_search_radius_deg: None,
    });
    world.plate_solver_stub = Some(stub);
}

// --- Given steps: equipment composition unique to center_on_target ---

#[given("rp is running with an unreachable camera and a mount on the simulator")]
async fn rp_with_unreachable_camera_and_mount(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    // Reserved port 1 is reliably unbound on Linux dev hosts /
    // CI runners — same pattern as auto_focus's `unreachable_camera`.
    world.cameras.push(CameraConfig {
        id: "main-cam".to_string(),
        alpaca_url: "http://127.0.0.1:1".to_string(),
        device_number: 0,
        cooler_targets_c: Vec::new(),
    });
    let url = world.omnisim_url();
    world.mount = Some(MountConfig {
        alpaca_url: url,
        device_number: 0,
        settle_after_slew: None,
    });
    start_rp(world).await;
}

#[given("rp is running with a camera on the simulator and an unreachable mount")]
async fn rp_with_camera_and_unreachable_mount(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    world.mount = Some(MountConfig {
        alpaca_url: "http://127.0.0.1:1".to_string(),
        device_number: 0,
        settle_after_slew: None,
    });
    start_rp(world).await;
}

// --- When steps ---

#[when(
    expr = "the MCP client calls center_on_target with camera {string} ra {float} dec {float} duration {string} tolerance_arcsec {float} max_attempts {int}"
)]
async fn mcp_call_center_on_target_full(
    world: &mut RpWorld,
    camera_id: String,
    ra: f64,
    dec: f64,
    duration: String,
    tolerance_arcsec: f64,
    max_attempts: i64,
) {
    let mut args = baseline_args();
    args.insert("camera_id".into(), Value::String(camera_id));
    args.insert("ra".into(), serde_json::json!(ra));
    args.insert("dec".into(), serde_json::json!(dec));
    args.insert("duration".into(), Value::String(duration));
    args.insert(
        "tolerance_arcsec".into(),
        serde_json::json!(tolerance_arcsec),
    );
    args.insert("max_attempts".into(), serde_json::json!(max_attempts));
    call_center_on_target(world, args).await;
}

/// The baseline call with a binning bolted on: the numeric parameters
/// are the shared `baseline_args` values, so the step text carries only
/// what the scenario is about.
#[when(expr = "the MCP client calls center_on_target at binning {string}")]
async fn mcp_call_center_on_target_binned(world: &mut RpWorld, binning: String) {
    let mut args = baseline_args();
    args.insert("binning".into(), Value::String(binning));
    call_center_on_target(world, args).await;
}

#[when(expr = "the MCP client calls center_on_target omitting {string}")]
async fn mcp_call_center_on_target_omitting(world: &mut RpWorld, missing_param: String) {
    let mut args = baseline_args();
    args.remove(missing_param.as_str());
    call_center_on_target(world, args).await;
}

#[when(expr = "the MCP client calls center_on_target with override {string} set to {float}")]
async fn mcp_call_center_on_target_override(world: &mut RpWorld, field: String, value: f64) {
    let mut args = baseline_args();
    // Numeric outline: the integer-vs-float distinction matters for
    // serde — `max_attempts` is `usize`, the other fields are `f64`.
    // We dispatch on the field name so the JSON shape matches.
    let json_value = match field.as_str() {
        "max_attempts" => serde_json::json!(value as i64),
        _ => serde_json::json!(value),
    };
    args.insert(field, json_value);
    call_center_on_target(world, args).await;
}

// --- Then steps ---

#[then(expr = "the center_on_target result should report attempts {int}")]
fn center_on_target_attempts(world: &mut RpWorld, expected: i64) {
    let result = world
        .last_center_on_target_result
        .as_ref()
        .expect("no center_on_target result");
    let actual = result
        .get("attempts")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_else(|| panic!("expected 'attempts' in result, got: {result:?}"));
    assert_eq!(actual, expected);
}

#[then(expr = "the center_on_target iterations[{int}] action should be {string}")]
fn center_on_target_iteration_action(world: &mut RpWorld, index: usize, expected: String) {
    let result = world
        .last_center_on_target_result
        .as_ref()
        .expect("no center_on_target result");
    let iterations = result
        .get("iterations")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("expected 'iterations' array in result, got: {result:?}"));
    let entry = iterations.get(index).unwrap_or_else(|| {
        panic!(
            "iterations has only {} entries, asked for index {}",
            iterations.len(),
            index
        )
    });
    let actual = entry
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("iteration {index} has no 'action' field: {entry:?}"));
    assert_eq!(actual, expected);
}

#[then(expr = "the center_on_target result should contain {string}")]
fn center_on_target_result_field_present(world: &mut RpWorld, field: String) {
    let result = world
        .last_center_on_target_result
        .as_ref()
        .expect("no center_on_target result");
    assert!(
        result.get(&field).is_some(),
        "expected '{field}' in center_on_target result, got: {result:?}"
    );
}

#[then(expr = "the stub plate solver should have received {int} solve calls")]
async fn stub_solve_call_count(world: &mut RpWorld, expected: usize) {
    let stub = world
        .plate_solver_stub
        .as_ref()
        .expect("no plate solver stub registered for this scenario");
    let requests = stub.requests().await;
    assert_eq!(
        requests.len(),
        expected,
        "expected {} solve calls, got {}",
        expected,
        requests.len()
    );
}

#[then(expr = "the mount position should be approximately ra {float} dec {float}")]
fn mount_position_approx(world: &mut RpWorld, expected_ra: f64, expected_dec: f64) {
    let result = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .unwrap_or_else(|e| panic!("expected get_mount_position to succeed, got error: {e}"));
    let actual_ra = result
        .get("ra")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("expected 'ra' in mount position, got: {result:?}"));
    let actual_dec = result
        .get("dec")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("expected 'dec' in mount position, got: {result:?}"));
    // OmniSim's slew-echo drift is ~0.001° on both axes (~3.6
    // arcsec) — same root cause as mount.feature's slew tolerance.
    // For the dec axis (degrees) the 0.01 cap here gives ~3× slack
    // over the drift; for the ra axis (hours) 0.01h = 0.15° = ~540
    // arcsec, far above the ~0.001° / ~0.000067h drift but still
    // tight enough to distinguish the iter-1 sync-then-slew outcome
    // (≈ input ra/dec) from an iter-N solved position that lands
    // tens of arcsec — or hours, in our scenarios — away.
    assert!(
        (actual_ra - expected_ra).abs() < 0.01,
        "expected ra ≈ {expected_ra}, got {actual_ra}"
    );
    assert!(
        (actual_dec - expected_dec).abs() < 0.01,
        "expected dec ≈ {expected_dec}, got {actual_dec}"
    );
}

// --- Helpers ---

/// Baseline arg map representing a "valid" `center_on_target` call.
/// Individual scenarios mutate this baseline (insert override / remove
/// field) before dispatching, so that the missing-parameter and
/// numeric-range scenarios can each carve out exactly one field
/// without re-stating the whole arg list. The baseline targets the
/// canned WCS response (`10.6848°`, `41.269°`) so it converges on
/// iteration 1 of a happy-path scenario.
fn baseline_args() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("camera_id".into(), Value::String("main-cam".into()));
    // 0.7123 hours × 15 = 10.6845° — within the canned WCS's
    // 10.6848° response so a default-baseline call converges
    // immediately when reused for the missing-parameter Outline
    // (where each row drops one field; the rest must be valid so the
    // body validator reaches the missing-field check rather than
    // tripping a numeric-range check first).
    m.insert("ra".into(), serde_json::json!(0.7123_f64));
    m.insert("dec".into(), serde_json::json!(41.269_f64));
    m.insert("duration".into(), Value::String("100ms".into()));
    m.insert("tolerance_arcsec".into(), serde_json::json!(60.0_f64));
    m.insert("max_attempts".into(), serde_json::json!(5_i64));
    m
}

async fn call_center_on_target(world: &mut RpWorld, args: Map<String, Value>) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("center_on_target", Value::Object(args))
        .await;
    match &result {
        Ok(v) => world.last_center_on_target_result = Some(v.clone()),
        Err(_) => world.last_center_on_target_result = None,
    }
    world.last_tool_result = Some(result);
}
