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

// --- Given steps: composite "rp running with ..." -------------------

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
    expr = "the MCP client calls \"plate_solve\" with the captured document_id and use_mount_hints {word}"
)]
async fn mcp_call_plate_solve_with_use_mount_hints(world: &mut RpWorld, value: String) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");
    let use_mount_hints = parse_bool(&value);

    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            document_id: Some(document_id),
            use_mount_hints: Some(use_mount_hints),
            ..Default::default()
        },
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"plate_solve\" with the captured document_id, pointing_hint ra_deg {float} dec_deg {float}, and use_mount_hints {word}"
)]
async fn mcp_call_plate_solve_with_both_hint_modes(
    world: &mut RpWorld,
    ra_deg: f64,
    dec_deg: f64,
    use_mount_hints: String,
) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");

    call_plate_solve_with_args(
        world,
        PlateSolveArgs {
            document_id: Some(document_id),
            pointing_hint: Some((ra_deg, dec_deg)),
            use_mount_hints: Some(parse_bool(&use_mount_hints)),
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

/// Record what the mount reports, so the `use_mount_hints` assertion
/// can pin the forwarded hints against the mount's own value instead
/// of against the literal the scenario synced to.
///
/// Why that matters: `sync_mount` does not land `OmniSim` exactly on
/// the requested RA. `OmniSim` builds the mount axis from a *cached*
/// sidereal time (refreshed only on its 100 ms `MoveAxes` tick) and
/// then re-derives the reported RA from a freshly read one, so the
/// sync absorbs however stale that cache was; the device restart our
/// per-scenario hook issues resets the tracking book-keeping that
/// would otherwise back-fill the difference, making the offset
/// permanent. Tracking then *holds* the offset position, so this is a
/// fixed error and not a drift — but its size is whatever wall-clock
/// gap the runner happened to leave, and on a loaded `windows-latest`
/// runner it reached 46 arcsec (issue #1252). Reading the mount back
/// takes the runner's speed out of the assertion entirely.
///
/// Deliberately stores into its own world field rather than
/// `last_tool_result`: the `plate_solve` result must stay inspectable
/// by any Then step that follows.
#[when("I record the mount position reported by get_mount_position")]
async fn record_mount_position(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("get_mount_position", Value::Object(Map::new()))
        .await
        .expect("get_mount_position should succeed");
    let ra_hours = result.get("ra").and_then(Value::as_f64).unwrap_or_else(|| {
        panic!("expected ra field in get_mount_position result, got: {result:?}")
    });
    let dec_deg = result
        .get("dec")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| {
            panic!("expected dec field in get_mount_position result, got: {result:?}")
        });
    world.recorded_mount_positions.push((ra_hours, dec_deg));
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

/// Hours-to-degrees factor rp applies to the mount's Alpaca
/// `RightAscension` before putting it on the wrapper wire. A missed
/// conversion is the defect the `use_mount_hints` scenario exists to
/// catch, so the factor is named here and spelled out in the step
/// expression rather than folded into a tolerance.
const RA_HOURS_TO_DEGREES: f64 = 15.0;

/// Agreement budget between an explicit `pointing_hint` and what the
/// wrapper received. rp forwards `pointing_hint` verbatim (no unit
/// conversion, no mount involved), so the only spread is JSON f64
/// round-tripping — exact in practice.
const HINT_PASSTHROUGH_TOLERANCE_DEG: f64 = 1e-9;

/// Agreement budget between rp's mount read (taken inside
/// `plate_solve`) and the BDD's own `get_mount_position` read taken
/// straight after it. Both see a *tracking* mount holding a fixed RA,
/// so the only spread is `OmniSim`'s per-tick numerical noise —
/// measured at ~2e-6°. 0.001° (3.6 arcsec) leaves three orders of
/// magnitude of headroom, and is still ~5 orders tighter than a
/// missed ×15, which lands RA ~150° out.
///
/// Unlike the 0.01° literal bound this replaced, it is *not* a
/// wall-clock budget: nothing here is charged for how long the
/// capture or the solve took, which is what made the old assertion
/// flaky on slow runners (issue #1252).
const MOUNT_READBACK_TOLERANCE_DEG: f64 = 0.001;

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

/// Assert the forwarded hints against a *bracket* of mount reads taken
/// either side of the `plate_solve` call, rather than against one read
/// taken after it.
///
/// rp reads the mount inside the call, so its value necessarily lies
/// between a read before and a read after — whatever the mount did in
/// between. That is what makes this independent of runner speed, and a
/// single sample cannot achieve it: `OmniSim` reports `RightAscension`
/// frozen between its ~100 ms `MoveAxes` ticks and rebuilds the axis
/// from a cached sidereal time, so a sync lands offset and the reported
/// position then moves for several ticks; under enough load the tick
/// starves and RA slips at roughly the sidereal rate indefinitely. Any
/// fixed budget between two reads at different instants is therefore a
/// wager on how fast the runner is.
///
/// [`MOUNT_READBACK_TOLERANCE_DEG`] still applies, now as slack on the
/// bracket for the per-tick numerical noise, which is what it was
/// always sized for. The bracket widens on its own as drift grows, and
/// a missed ×15 puts RA ~150° out — five orders of magnitude outside
/// any bracket this scenario can produce.
#[then(
    expr = "the stub plate solver should have received ra_hint and dec_hint bracketed by the recorded mount positions"
)]
async fn stub_hints_bracketed_by_recorded_mount(world: &mut RpWorld) {
    let recorded = world.recorded_mount_positions.clone();
    assert!(
        recorded.len() >= 2,
        "need a mount read either side of the call to bracket its own read; got {}",
        recorded.len()
    );
    let request = last_stub_request(world).await;
    let (actual_ra, actual_dec) = request_hints(&request);

    let ra_bounds: Vec<f64> = recorded
        .iter()
        .map(|(ra_hours, _)| ra_hours * RA_HOURS_TO_DEGREES)
        .collect();
    let dec_bounds: Vec<f64> = recorded.iter().map(|&(_, dec)| dec).collect();
    let bracket = |bounds: &[f64]| {
        bounds
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)))
    };
    let (ra_lo, ra_hi) = bracket(&ra_bounds);
    let (dec_lo, dec_hi) = bracket(&dec_bounds);

    assert!(
        actual_ra >= ra_lo - MOUNT_READBACK_TOLERANCE_DEG
            && actual_ra <= ra_hi + MOUNT_READBACK_TOLERANCE_DEG,
        "ra_hint should be the mount's RA × {RA_HOURS_TO_DEGREES}, so within \
         [{ra_lo}, {ra_hi}]° (± {MOUNT_READBACK_TOLERANCE_DEG}) of the reads taken \
         either side of the call; got {actual_ra}"
    );
    assert!(
        actual_dec >= dec_lo - MOUNT_READBACK_TOLERANCE_DEG
            && actual_dec <= dec_hi + MOUNT_READBACK_TOLERANCE_DEG,
        "dec_hint should be the mount's Dec forwarded verbatim, so within \
         [{dec_lo}, {dec_hi}]° (± {MOUNT_READBACK_TOLERANCE_DEG}); got {actual_dec}"
    );
}

#[then(
    expr = "the recorded mount position should be within {float} degrees of ra {float} and dec {float}"
)]
fn recorded_mount_position_near(
    world: &mut RpWorld,
    tolerance_deg: f64,
    ra_deg: f64,
    dec_deg: f64,
) {
    let &(mount_ra_hours, mount_dec_deg) = world
        .recorded_mount_positions
        .last()
        .expect("no mount position recorded — run the record step before this assertion");
    let mount_ra_deg = mount_ra_hours * RA_HOURS_TO_DEGREES;
    assert!(
        (mount_ra_deg - ra_deg).abs() < tolerance_deg,
        "mount RA {mount_ra_deg}° ({mount_ra_hours} h) is more than {tolerance_deg}° from the synced {ra_deg}°"
    );
    assert!(
        (mount_dec_deg - dec_deg).abs() < tolerance_deg,
        "mount Dec {mount_dec_deg}° is more than {tolerance_deg}° from the synced {dec_deg}°"
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

#[then("the stub plate solver should have received a request with no hint fields")]
async fn stub_received_blind_request(world: &mut RpWorld) {
    let request = last_stub_request(world).await;
    for field in ["ra_hint", "dec_hint", "fov_hint_deg", "search_radius_deg"] {
        let v = request.get(field);
        assert!(
            v.is_none() || v == Some(&Value::Null),
            "expected '{field}' to be absent or null on a blind solve, got: {v:?}"
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
    use_mount_hints: Option<bool>,
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
    if let Some(b) = args.use_mount_hints {
        params.insert("use_mount_hints".to_string(), Value::Bool(b));
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
