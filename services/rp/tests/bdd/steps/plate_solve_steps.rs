//! BDD step definitions for the `plate_solve` MCP tool.
//!
//! Shared steps live in `tool_steps.rs`:
//! - `the MCP client lists available tools`
//! - `the tool list should include {string}`
//! - `the tool call should return an error`
//! - `the error message should contain {string}`
//! - `the MCP client calls "capture" with camera ... for ... ms`
//!   (stores `last_image_path` and `last_document_id` on the world)
//!
//! `I fetch the exposure document for the captured document_id`,
//! `the exposure document should contain a section named ...`, and
//! `the {string} section should contain {string}` are reused from
//! `measure_basic_steps.rs`.

use std::time::Duration;

use cucumber::{given, then, when};
use serde_json::{Map, Value};

use bdd_infra::rp_harness::{MountConfig, PlateSolverConfig, PlateSolverStub, StubBehavior};

use crate::steps::tool_steps::{add_camera, ensure_mcp_client, ensure_omnisim, start_rp};
use crate::world::RpWorld;

// --- Given steps: stub server lifecycle ------------------------------

#[given("a stub plate solver returning a canned WCS")]
async fn stub_plate_solver_canned(world: &mut RpWorld) {
    let stub = PlateSolverStub::start(StubBehavior::default_canned_wcs()).await;
    world.plate_solver = Some(PlateSolverConfig {
        url: stub.url.clone(),
        timeout: None,
        default_search_radius_deg: None,
    });
    world.plate_solver_stub = Some(stub);
}

#[given(expr = "a stub plate solver returning error code {string} with message {string}")]
async fn stub_plate_solver_error(world: &mut RpWorld, code: String, message: String) {
    let stub = PlateSolverStub::start(StubBehavior::Error { code, message }).await;
    world.plate_solver = Some(PlateSolverConfig {
        url: stub.url.clone(),
        timeout: None,
        default_search_radius_deg: None,
    });
    world.plate_solver_stub = Some(stub);
}

#[given(expr = "a stub plate solver returning a canned WCS after {int} ms")]
async fn stub_plate_solver_delayed(world: &mut RpWorld, delay_ms: u64) {
    let stub = PlateSolverStub::start(StubBehavior::delayed_canned_wcs(Duration::from_millis(
        delay_ms,
    )))
    .await;
    world.plate_solver = Some(PlateSolverConfig {
        url: stub.url.clone(),
        timeout: None,
        default_search_radius_deg: None,
    });
    world.plate_solver_stub = Some(stub);
}

// --- Given steps: composite "rp running with ..." -------------------

#[given(expr = "rp is running with a camera on the simulator and plate_solver timeout {int} ms")]
async fn rp_with_camera_and_plate_solver_timeout(world: &mut RpWorld, timeout_ms: u64) {
    ensure_omnisim(world).await;
    add_camera(world);
    let url =
        world.plate_solver.as_ref().map(|ps| ps.url.clone()).expect(
            "this step expects a prior 'Given a stub plate solver ...' step to set the URL",
        );
    world.plate_solver = Some(PlateSolverConfig {
        url,
        timeout: Some(Duration::from_millis(timeout_ms)),
        default_search_radius_deg: None,
    });
    start_rp(world).await;
}

#[given(
    expr = "rp is running with a camera on the simulator and plate_solver default_search_radius_deg {float}"
)]
async fn rp_with_camera_and_default_radius(world: &mut RpWorld, radius: f64) {
    ensure_omnisim(world).await;
    add_camera(world);
    // Reuse the URL the prior `Given a stub plate solver ...` step
    // placed on the world, but layer on the operator-set default
    // radius. If no stub was registered, that's a scenario authoring
    // error — fail loud with a clear message.
    let url =
        world.plate_solver.as_ref().map(|ps| ps.url.clone()).expect(
            "this step expects a prior 'Given a stub plate solver ...' step to set the URL",
        );
    world.plate_solver = Some(PlateSolverConfig {
        url,
        timeout: None,
        default_search_radius_deg: Some(radius),
    });
    start_rp(world).await;
}

#[given(
    "rp is running with a camera on the simulator and plate_solver pointing at an unbound port"
)]
async fn rp_with_camera_and_unbound_plate_solver(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    // Port 1 is reserved and reliably unbound on Linux dev hosts /
    // CI runners, mirroring the `unreachable focuser` /
    // `unreachable camera` pattern in auto_focus_steps.rs.
    world.plate_solver = Some(PlateSolverConfig {
        url: "http://127.0.0.1:1".to_string(),
        timeout: None,
        default_search_radius_deg: None,
    });
    start_rp(world).await;
}

#[given("rp is running with a camera and a mount on the simulator")]
async fn rp_with_camera_and_mount(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    let url = world.omnisim_url();
    world.mount = Some(MountConfig {
        alpaca_url: url,
        device_number: 0,
        settle_after_slew: None,
    });
    start_rp(world).await;
}

// --- When steps -----------------------------------------------------

#[when("the MCP client calls \"plate_solve\" with the captured image path")]
async fn mcp_call_plate_solve_with_last_path(world: &mut RpWorld) {
    let image_path = world
        .last_image_path
        .clone()
        .expect("no captured image path available");

    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            image_path: Some(image_path),
            ..Default::default()
        },
    )
    .await;
}

#[when("the MCP client calls \"plate_solve\" with the captured document_id")]
async fn mcp_call_plate_solve_with_last_doc(world: &mut RpWorld) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");

    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            document_id: Some(document_id),
            ..Default::default()
        },
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"plate_solve\" with both the captured document_id and image path {string}"
)]
async fn mcp_call_plate_solve_with_both(world: &mut RpWorld, image_path: String) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");

    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            document_id: Some(document_id),
            image_path: Some(image_path),
            ..Default::default()
        },
    )
    .await;
}

#[when(expr = "the MCP client calls \"plate_solve\" with image path {string}")]
async fn mcp_call_plate_solve_with_path(world: &mut RpWorld, image_path: String) {
    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            image_path: Some(image_path),
            ..Default::default()
        },
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"plate_solve\" with the captured document_id and pointing_hint ra_deg {float} dec_deg {float}"
)]
async fn mcp_call_plate_solve_with_pointing_hint(world: &mut RpWorld, ra_deg: f64, dec_deg: f64) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");

    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            document_id: Some(document_id),
            pointing_hint: Some((ra_deg, dec_deg)),
            ..Default::default()
        },
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"plate_solve\" with the captured document_id and blind {word}"
)]
async fn mcp_call_plate_solve_with_blind(world: &mut RpWorld, blind: String) {
    let document_id = captured_document_id(world);
    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            document_id: Some(document_id),
            blind: Some(parse_bool(&blind)),
            ..Default::default()
        },
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"plate_solve\" with the captured document_id, pointing_hint ra_deg {float} dec_deg {float}, and blind {word}"
)]
async fn mcp_call_plate_solve_with_pointing_hint_and_blind(
    world: &mut RpWorld,
    ra_deg: f64,
    dec_deg: f64,
    blind: String,
) {
    let document_id = captured_document_id(world);
    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            document_id: Some(document_id),
            pointing_hint: Some((ra_deg, dec_deg)),
            blind: Some(parse_bool(&blind)),
            ..Default::default()
        },
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"plate_solve\" with the captured document_id, search_radius_deg {float}, and blind {word}"
)]
async fn mcp_call_plate_solve_with_search_radius_and_blind(
    world: &mut RpWorld,
    search_radius_deg: f64,
    blind: String,
) {
    let document_id = captured_document_id(world);
    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            document_id: Some(document_id),
            search_radius_deg: Some(search_radius_deg),
            blind: Some(parse_bool(&blind)),
            ..Default::default()
        },
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"plate_solve\" with the captured document_id, pointing_hint ra_deg {float} dec_deg {float}, and search_radius_deg {float}"
)]
async fn mcp_call_plate_solve_with_pointing_hint_and_search_radius(
    world: &mut RpWorld,
    ra_deg: f64,
    dec_deg: f64,
    search_radius_deg: f64,
) {
    let document_id = captured_document_id(world);
    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            document_id: Some(document_id),
            pointing_hint: Some((ra_deg, dec_deg)),
            search_radius_deg: Some(search_radius_deg),
            ..Default::default()
        },
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"plate_solve\" with the captured document_id and timeout {string}"
)]
async fn mcp_call_plate_solve_with_timeout(world: &mut RpWorld, timeout: String) {
    let document_id = captured_document_id(world);
    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            document_id: Some(document_id),
            timeout: Some(timeout),
            ..Default::default()
        },
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"plate_solve\" with the captured document_id, fov_hint_deg {float}, search_radius_deg {float}, and timeout {string}"
)]
async fn mcp_call_plate_solve_with_optionals(
    world: &mut RpWorld,
    fov_hint_deg: f64,
    search_radius_deg: f64,
    timeout: String,
) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");

    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            document_id: Some(document_id),
            fov_hint_deg: Some(fov_hint_deg),
            search_radius_deg: Some(search_radius_deg),
            timeout: Some(timeout),
            ..Default::default()
        },
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"plate_solve\" with the captured document_id and search_radius_deg {float}"
)]
async fn mcp_call_plate_solve_with_search_radius(world: &mut RpWorld, search_radius_deg: f64) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");

    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            document_id: Some(document_id),
            search_radius_deg: Some(search_radius_deg),
            ..Default::default()
        },
    )
    .await;
}

#[when("the MCP client calls \"plate_solve\" with no arguments")]
async fn mcp_call_plate_solve_no_args(world: &mut RpWorld) {
    call_plate_solve_with_args(world, PlateSolveArgs::default()).await;
}

// --- Then steps: result-shape assertions -----------------------------

#[then(expr = "the plate_solve result should contain {string} with value {float}")]
fn plate_solve_field_equals_float(world: &mut RpWorld, field: String, expected: f64) {
    let result = world
        .last_plate_solve_result
        .as_ref()
        .expect("no plate_solve result");
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in plate_solve result, got: {result:?}"));
    let actual = value
        .as_f64()
        .unwrap_or_else(|| panic!("expected '{field}' to be a number, got: {value:?}"));
    assert!(
        (actual - expected).abs() < 1e-6,
        "expected '{field}' to equal {expected}, got: {actual}"
    );
}

#[then(expr = "the plate_solve result should contain {string} with value {string}")]
fn plate_solve_field_equals_string(world: &mut RpWorld, field: String, expected: String) {
    let result = world
        .last_plate_solve_result
        .as_ref()
        .expect("no plate_solve result");
    let actual = result
        .get(&field)
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("expected '{field}' as string in {result:?}"));
    assert_eq!(actual, expected);
}

#[then("the plate_solve result wcs_matrix should match these values:")]
fn plate_solve_wcs_matrix_matches(world: &mut RpWorld, step: &cucumber::gherkin::Step) {
    let result = world
        .last_plate_solve_result
        .as_ref()
        .expect("no plate_solve result");
    let matrix = result
        .get("wcs_matrix")
        .unwrap_or_else(|| panic!("expected 'wcs_matrix' in plate_solve result, got: {result:?}"));
    let table = step.table.as_ref().expect("step requires a data table");
    // Skip the header row (field | value).
    for row in table.rows.iter().skip(1) {
        assert_eq!(row.len(), 2, "table row must be field, value: {row:?}");
        let field = row[0].trim();
        let expected: f64 = row[1]
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("value must be f64 in row {row:?}"));
        let actual = matrix
            .get(field)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or_else(|| panic!("expected '{field}' as number in wcs_matrix: {matrix:?}"));
        assert!(
            (actual - expected).abs() < 1e-9,
            "wcs_matrix.{field}: expected {expected}, got {actual}"
        );
    }
}

// --- Then steps: stub request-log inspection ------------------------

/// Hours-to-degrees factor rp applies to a recorded pointing's Alpaca
/// `ra_hours` before putting it on the wrapper wire. A missed
/// conversion is the defect the recorded-pointing scenario exists to
/// catch, so the factor is named here and spelled out in the step
/// expression rather than folded into a tolerance.
const RA_HOURS_TO_DEGREES: f64 = 15.0;

/// Agreement budget between a hint's source — an explicit
/// `pointing_hint`, or the frame's recorded `pointing` times 15 — and
/// what the wrapper received. Neither involves a live read, so the only
/// spread is JSON f64 round-tripping — exact in practice.
const HINT_PASSTHROUGH_TOLERANCE_DEG: f64 = 1e-9;

#[then(
    expr = "the stub plate solver should have received a request with ra_hint {float} and dec_hint {float}"
)]
async fn stub_received_pointing_hints(world: &mut RpWorld, ra_hint: f64, dec_hint: f64) {
    let request = last_stub_request(world).await;
    let (actual_ra, actual_dec) = request_hints(&request);
    assert!(
        (actual_ra - ra_hint).abs() < HINT_PASSTHROUGH_TOLERANCE_DEG,
        "expected ra_hint {ra_hint} forwarded verbatim, got {actual_ra}"
    );
    assert!(
        (actual_dec - dec_hint).abs() < HINT_PASSTHROUGH_TOLERANCE_DEG,
        "expected dec_hint {dec_hint} forwarded verbatim, got {actual_dec}"
    );
}

/// The forwarded hints are the frame's recorded `pointing`: its
/// `ra_hours` times [`RA_HOURS_TO_DEGREES`], folded into `[0, 360)`, and
/// its `dec_degrees`. Compared against the record itself, so nothing is
/// charged for how far the mount moved after the exposure started.
#[then(
    expr = "the stub plate solver should have received the document's pointing as ra_hint and dec_hint, its ra_hours times 15"
)]
async fn stub_received_recorded_pointing(world: &mut RpWorld) {
    let doc = last_exposure_document(world);
    let pointing = doc
        .get("pointing")
        .unwrap_or_else(|| panic!("the document records no pointing: {doc}"));
    let ra_hours = pointing["ra_hours"].as_f64().expect("pointing.ra_hours");
    let dec_degrees = pointing["dec_degrees"]
        .as_f64()
        .expect("pointing.dec_degrees");
    let request = last_stub_request(world).await;
    let (actual_ra, actual_dec) = request_hints(&request);
    let expected_ra = (ra_hours * RA_HOURS_TO_DEGREES).rem_euclid(360.0);
    assert!(
        (actual_ra - expected_ra).abs() < HINT_PASSTHROUGH_TOLERANCE_DEG,
        "ra_hint should be pointing.ra_hours {ra_hours} x {RA_HOURS_TO_DEGREES} = {expected_ra}, got {actual_ra}"
    );
    assert!(
        (actual_dec - dec_degrees).abs() < HINT_PASSTHROUGH_TOLERANCE_DEG,
        "dec_hint should be pointing.dec_degrees {dec_degrees}, got {actual_dec}"
    );
}

#[then("the stub plate solver should have received the document's optics fov_height_deg as fov_hint_deg")]
async fn stub_received_recorded_fov(world: &mut RpWorld) {
    let doc = last_exposure_document(world);
    let fov = doc
        .pointer("/optics/fov_height_deg")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("the document records no optics.fov_height_deg: {doc}"));
    let request = last_stub_request(world).await;
    assert_eq!(
        request.get("fov_hint_deg").and_then(Value::as_f64),
        Some(fov),
        "fov_hint_deg should be optics.fov_height_deg in {request:?}"
    );
}

#[then("the stub plate solver should have received a request that is not blind")]
async fn stub_received_hinted_request(world: &mut RpWorld) {
    let request = last_stub_request(world).await;
    let blind = request.get("blind");
    assert!(
        blind.is_none() || blind == Some(&Value::Bool(false)),
        "expected no blind flag, got: {request:?}"
    );
}

/// Pull `ra_hint` / `dec_hint` out of a recorded stub request, failing
/// loud with the whole request body when either is missing.
fn request_hints(request: &Value) -> (f64, f64) {
    let ra = request
        .get("ra_hint")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("expected ra_hint in request, got: {request:?}"));
    let dec = request
        .get("dec_hint")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("expected dec_hint in request, got: {request:?}"));
    (ra, dec)
}

#[then(
    "the stub plate solver should have received a blind request with no position and no search radius"
)]
async fn stub_received_blind_request(world: &mut RpWorld) {
    let request = last_stub_request(world).await;
    assert_eq!(
        request.get("blind"),
        Some(&Value::Bool(true)),
        "expected blind true, got: {request:?}"
    );
    for field in ["ra_hint", "dec_hint", "search_radius_deg"] {
        let v = request.get(field);
        assert!(
            v.is_none() || v == Some(&Value::Null),
            "expected '{field}' to be absent on a blind solve, got: {v:?}"
        );
    }
}

#[then(
    expr = "the stub plate solver should have received a request with fov_hint_deg {float} and search_radius_deg {float} and timeout {string}"
)]
async fn stub_received_optional_fields(
    world: &mut RpWorld,
    fov_hint_deg: f64,
    search_radius_deg: f64,
    timeout: String,
) {
    let request = last_stub_request(world).await;
    assert_eq!(
        request
            .get("fov_hint_deg")
            .and_then(serde_json::Value::as_f64),
        Some(fov_hint_deg),
        "fov_hint_deg mismatch in {request:?}"
    );
    assert_eq!(
        request
            .get("search_radius_deg")
            .and_then(serde_json::Value::as_f64),
        Some(search_radius_deg),
        "search_radius_deg mismatch in {request:?}"
    );
    assert_eq!(
        request.get("timeout").and_then(|v| v.as_str()),
        Some(timeout.as_str()),
        "timeout mismatch in {request:?}"
    );
}

#[then(
    expr = "the stub plate solver should have received a request with search_radius_deg {float}"
)]
async fn stub_received_search_radius(world: &mut RpWorld, search_radius_deg: f64) {
    let request = last_stub_request(world).await;
    assert_eq!(
        request
            .get("search_radius_deg")
            .and_then(serde_json::Value::as_f64),
        Some(search_radius_deg),
        "search_radius_deg mismatch in {request:?}"
    );
}

#[then("the stub plate solver should have received a request whose fits_path matches the captured FITS")]
async fn stub_received_captured_fits_path(world: &mut RpWorld) {
    let expected = world
        .last_image_path
        .clone()
        .expect("no captured image path available");
    let request = last_stub_request(world).await;
    let fits_path = request
        .get("fits_path")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("expected fits_path in request, got: {request:?}"));
    assert_eq!(
        fits_path, expected,
        "expected fits_path to be the captured FITS path"
    );
}

// --- Helpers --------------------------------------------------------

#[derive(Default)]
struct PlateSolveArgs {
    document_id: Option<String>,
    image_path: Option<String>,
    pointing_hint: Option<(f64, f64)>,
    blind: Option<bool>,
    fov_hint_deg: Option<f64>,
    search_radius_deg: Option<f64>,
    timeout: Option<String>,
}

async fn call_plate_solve_with_args(world: &mut RpWorld, args: PlateSolveArgs) {
    ensure_mcp_client(world).await;

    let mut params = Map::new();
    if let Some(doc) = args.document_id {
        params.insert("document_id".to_string(), Value::String(doc));
    }
    if let Some(path) = args.image_path {
        params.insert("image_path".to_string(), Value::String(path));
    }
    if let Some((ra_deg, dec_deg)) = args.pointing_hint {
        params.insert(
            "pointing_hint".to_string(),
            serde_json::json!({
                "ra_deg": ra_deg,
                "dec_deg": dec_deg,
            }),
        );
    }
    if let Some(b) = args.blind {
        params.insert("blind".to_string(), Value::Bool(b));
    }
    if let Some(f) = args.fov_hint_deg {
        params.insert("fov_hint_deg".to_string(), serde_json::json!(f));
    }
    if let Some(r) = args.search_radius_deg {
        params.insert("search_radius_deg".to_string(), serde_json::json!(r));
    }
    if let Some(t) = args.timeout {
        params.insert("timeout".to_string(), Value::String(t));
    }

    let result = world
        .mcp()
        .call_tool("plate_solve", Value::Object(params))
        .await;

    match &result {
        Ok(v) => world.last_plate_solve_result = Some(v.clone()),
        Err(_) => world.last_plate_solve_result = None,
    }
    world.last_tool_result = Some(result);
}

fn captured_document_id(world: &RpWorld) -> String {
    world
        .last_document_id
        .clone()
        .expect("no captured document_id available")
}

const fn last_exposure_document(world: &RpWorld) -> &Value {
    world
        .last_exposure_document
        .as_ref()
        .expect("no exposure document fetched — run the fetch step first")
}

async fn last_stub_request(world: &RpWorld) -> Value {
    let stub = world
        .plate_solver_stub
        .as_ref()
        .expect("no plate solver stub registered for this scenario");
    let mut requests = stub.requests().await;
    requests
        .pop()
        .expect("stub plate solver received no requests")
}

fn parse_bool(s: &str) -> bool {
    match s {
        "true" => true,
        "false" => false,
        other => panic!("expected true|false, got {other}"),
    }
}
