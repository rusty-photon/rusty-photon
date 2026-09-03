//! BDD step definitions for the end-to-end polar-alignment workflow.
//!
//! The scenarios spawn `OmniSim` (telescope + camera), rp, and
//! polar-align, plus an in-process plate-solver stub whose canned
//! solves are choreographed from a known injected axis error. The
//! completion contract is asserted through the plugin's `/status`
//! endpoint (rp deliberately ignores completion bodies) and rp's
//! session status.

use std::time::Duration;

use bdd_infra::rp_harness::{
    start_rp, write_temp_config_file, CannedWcs, CannedWcsMatrix, OmniSimHandle, PlateSolverStub,
    StubBehavior,
};
use bdd_infra::ServiceHandle;
use cucumber::{given, then, when};
use polar_align::math::{unit_from_radec, wcs_from_attitude, Mat3, Vec3};
use rp_ephemeris::{AltAz, Ephemeris, ErfarsEphemeris, Site};

use crate::world::{
    PolarAlignWorld, OMNISIM_SITE_LATITUDE_DEG, OMNISIM_SITE_LONGITUDE_DEG, SITE_LATITUDE_DEG,
    SITE_LONGITUDE_DEG,
};

/// [`choreograph_solves_at`] in the world's shared site — what every
/// scenario whose plugin config carries that site uses.
fn choreograph_solves(east_arcmin: f64, alt_arcmin: f64, off_axis_deg: f64) -> Vec<CannedWcs> {
    choreograph_solves_at(
        SITE_LATITUDE_DEG,
        SITE_LONGITUDE_DEG,
        east_arcmin,
        alt_arcmin,
        off_axis_deg,
    )
}

/// Generate the three measurement solves (plus one adjustment solve
/// the Sequence clamps on) for a mount whose RA axis sits at the
/// given error from the pole: `east_arcmin` of true angle toward the
/// east horizon, `alt_arcmin` above it. The camera starts
/// `off_axis_deg` from the axis and each point rotates the full
/// attitude 45° about it, so the per-point `wcs_matrix` encodes the
/// same rotation the centers trace — both axis methods must agree on
/// these solves. Pure geometry in the given site, unrefracted —
/// matching the service config's `refraction.enabled = false`. The
/// site must be the one the *plugin* resolves (config or rp), or the
/// recovered error cannot match the injected one.
fn choreograph_solves_at(
    site_latitude_deg: f64,
    site_longitude_deg: f64,
    east_arcmin: f64,
    alt_arcmin: f64,
    off_axis_deg: f64,
) -> Vec<CannedWcs> {
    let site = Site::new(site_latitude_deg, site_longitude_deg).expect("test site");
    let eph = ErfarsEphemeris::new();
    let now = chrono::Utc::now();

    let axis_alt = site_latitude_deg + alt_arcmin / 60.0;
    let axis_az = ((east_arcmin / 60.0_f64).to_radians().sin() / axis_alt.to_radians().cos())
        .asin()
        .to_degrees();
    let axis_icrs = eph
        .icrs_from_alt_az(
            &site,
            AltAz {
                azimuth_degrees: axis_az,
                altitude_degrees: axis_alt,
            },
            now,
            None,
        )
        .expect("axis observed→ICRS");
    let axis = unit_from_radec(axis_icrs.ra_hours * 15.0, axis_icrs.dec_degrees);

    // Initial camera attitude: boresight off_axis_deg from the axis,
    // pixel axes from the boresight's local frame (any rigid choice
    // works — only relative rotations enter the axis extraction).
    let perp = axis
        .cross(Vec3::new(0.0, 0.0, 1.0))
        .normalized()
        .expect("axis is not the celestial pole");
    let boresight = Mat3::from_axis_angle(perp, off_axis_deg.to_radians()).mul_vec(axis);
    let x = Vec3::new(0.0, 0.0, 1.0)
        .cross(boresight)
        .normalized()
        .expect("boresight is not the celestial pole");
    let initial_attitude = Mat3::from_columns(x, boresight.cross(x), boresight);

    (0..4)
        .map(|i| {
            let sweep = f64::from(i.min(2)) * 45.0_f64.to_radians();
            let attitude = Mat3::from_axis_angle(axis, sweep).mul_mat(initial_attitude);
            let frame = wcs_from_attitude(attitude, 2.9167e-4, (512.0, 384.0))
                .expect("choreographed boresight stays away from the poles");
            CannedWcs {
                ra_center: frame.center_ra_deg,
                dec_center: frame.center_dec_deg,
                pixel_scale_arcsec: 1.05,
                rotation_deg: 0.0,
                solver: "stub-astap".to_string(),
                wcs_matrix: Some(CannedWcsMatrix {
                    crpix1: frame.matrix.crpix1,
                    crpix2: frame.matrix.crpix2,
                    cd1_1: frame.matrix.cd1_1,
                    cd1_2: frame.matrix.cd1_2,
                    cd2_1: frame.matrix.cd2_1,
                    cd2_2: frame.matrix.cd2_2,
                }),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Given steps
// ---------------------------------------------------------------------------

#[given("a running Alpaca simulator")]
async fn running_alpaca_simulator(world: &mut PolarAlignWorld) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
}

#[given(
    expr = "a stub plate solver choreographed for an axis error of {float} arcminutes east and {float} arcminutes in altitude"
)]
async fn stub_solver_choreographed(world: &mut PolarAlignWorld, east: f64, alt: f64) {
    // Scope ~5° off-axis, as the dec-85 near-pole sweep produces.
    let stub =
        PlateSolverStub::start(StubBehavior::Sequence(choreograph_solves(east, alt, 5.0))).await;
    world.plate_solver = Some(bdd_infra::rp_harness::PlateSolverConfig {
        url: stub.url.clone(),
        timeout: None,
        default_search_radius_deg: None,
    });
    world.plate_solver_stub = Some(stub);
    world.injected_error_arcmin = Some((east, alt));
}

#[given(
    expr = "a stub plate solver choreographed for an axis error of {float} arcminutes east and {float} arcminutes in altitude from a mid-sky pointing"
)]
async fn stub_solver_choreographed_mid_sky(world: &mut PolarAlignWorld, east: f64, alt: f64) {
    // Boresight 35° from the axis — the start-from-anywhere case the
    // attitude-based measurement exists for.
    let stub =
        PlateSolverStub::start(StubBehavior::Sequence(choreograph_solves(east, alt, 35.0))).await;
    world.plate_solver = Some(bdd_infra::rp_harness::PlateSolverConfig {
        url: stub.url.clone(),
        timeout: None,
        default_search_radius_deg: None,
    });
    world.plate_solver_stub = Some(stub);
    world.injected_error_arcmin = Some((east, alt));
}

#[given("a stub plate solver whose solves stop carrying a WCS matrix after the second point")]
async fn stub_solver_loses_matrix(world: &mut PolarAlignWorld) {
    // Points 1–2 carry the matrix; point 3 and the clamped adjustment
    // entry do not. A near-pole measurement needs only centers, so it
    // succeeds with no attitude seed and every adjustment solve then
    // fails the iteration ("solve carried no wcs_matrix"); a
    // current-position measurement aborts at point 3.
    let mut solves = choreograph_solves(30.0, -20.0, 5.0);
    for solve in solves.iter_mut().skip(2) {
        solve.wcs_matrix = None;
    }
    let stub = PlateSolverStub::start(StubBehavior::Sequence(solves)).await;
    world.plate_solver = Some(bdd_infra::rp_harness::PlateSolverConfig {
        url: stub.url.clone(),
        timeout: None,
        default_search_radius_deg: None,
    });
    world.plate_solver_stub = Some(stub);
}

#[given("the simulator mount is synced to a mid-sky pointing west of the meridian")]
async fn sync_mount_mid_sky(world: &mut PolarAlignWorld) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
    // OmniSim refuses a sync with tracking off.
    OmniSimHandle::set_telescope_tracking(true)
        .await
        .expect("enable simulator tracking");
    let site = Site::new(SITE_LATITUDE_DEG, SITE_LONGITUDE_DEG).expect("test site");
    let eph = ErfarsEphemeris::new();
    let lst_hours = eph.sidereal_time(&site, chrono::Utc::now()).lst_hours;
    // Hour angle +2h (30° west of the meridian) at declination 60:
    // circumpolar at the test latitude, so every sweep target clears
    // the workflow's altitude floor whatever the wall clock says.
    let ra_hours = (lst_hours - 2.0).rem_euclid(24.0);
    OmniSimHandle::sync_telescope_to(ra_hours, 60.0)
        .await
        .expect("sync the simulator mount");
    world.synced_ra_deg = Some(ra_hours * 15.0);
}

#[given("the measurement starts from the current mount position")]
async fn measurement_from_current_position(world: &mut PolarAlignWorld) {
    world
        .measurement_overrides
        .insert("mode".to_string(), serde_json::json!("current_position"));
    // A short sweep keeps the two OmniSim slews cheap; the canned
    // solves do not depend on the commanded sweep.
    world
        .measurement_overrides
        .insert("sweep_deg".to_string(), serde_json::json!(15.0));
}

#[given("the measurement uses manual rotation")]
async fn measurement_manual_rotation(world: &mut PolarAlignWorld) {
    world
        .measurement_overrides
        .insert("mode".to_string(), serde_json::json!("manual_rotation"));
}

#[given("rp is configured with the simulator's default observer site")]
async fn rp_configured_with_omnisim_site(world: &mut PolarAlignWorld) {
    world.rp_site = Some((OMNISIM_SITE_LATITUDE_DEG, OMNISIM_SITE_LONGITUDE_DEG));
}

#[given("the polar-align config carries no site block")]
async fn plugin_config_without_site(world: &mut PolarAlignWorld) {
    world.omit_plugin_site = true;
}

#[given(
    expr = "a stub plate solver choreographed for an axis error of {float} arcminutes east and {float} arcminutes in altitude at the simulator's default site"
)]
async fn stub_solver_choreographed_omnisim_site(world: &mut PolarAlignWorld, east: f64, alt: f64) {
    // Choreographed in the site the plugin will RESOLVE (rp's, which
    // must equal OmniSim's default) — measuring these solves against
    // any other site is degrees wrong, so the 2′ recovery assertion
    // only passes if the site really came from rp.
    let stub = PlateSolverStub::start(StubBehavior::Sequence(choreograph_solves_at(
        OMNISIM_SITE_LATITUDE_DEG,
        OMNISIM_SITE_LONGITUDE_DEG,
        east,
        alt,
        5.0,
    )))
    .await;
    world.plate_solver = Some(bdd_infra::rp_harness::PlateSolverConfig {
        url: stub.url.clone(),
        timeout: None,
        default_search_radius_deg: None,
    });
    world.plate_solver_stub = Some(stub);
    world.injected_error_arcmin = Some((east, alt));
}

#[given(expr = "the workflow tolerates at most {int} consecutive failed solves")]
async fn limit_solve_failures(world: &mut PolarAlignWorld, max: u32) {
    world
        .adjustment_overrides
        .insert("max_solve_failures".to_string(), serde_json::json!(max));
}

#[given(expr = "the adjustment window is limited to {int} second(s)")]
async fn limit_adjustment_window(world: &mut PolarAlignWorld, seconds: u64) {
    world.adjustment_overrides.insert(
        "max_duration".to_string(),
        serde_json::json!(format!("{seconds}s")),
    );
}

#[given("a stub plate solver that always fails")]
async fn stub_solver_failing(world: &mut PolarAlignWorld) {
    let stub = PlateSolverStub::start(StubBehavior::Error {
        code: "solve_failed".to_string(),
        message: "no stars matched".to_string(),
    })
    .await;
    world.plate_solver = Some(bdd_infra::rp_harness::PlateSolverConfig {
        url: stub.url.clone(),
        timeout: None,
        default_search_radius_deg: None,
    });
    world.plate_solver_stub = Some(stub);
}

#[given(
    "rp is running with a camera, a mount, the stub plate solver, and the polar-align orchestrator"
)]
async fn rp_running_with_polar_align(world: &mut PolarAlignWorld) {
    configure_default_equipment(world).await;
    start_rp_service(world).await;
    start_polar_align_service(world).await;
}

#[given("the polar-align service is running standalone")]
async fn polar_align_standalone(world: &mut PolarAlignWorld) {
    start_polar_align_service(world).await;
}

// ---------------------------------------------------------------------------
// When steps
// ---------------------------------------------------------------------------

/// Start a run at polar-align's own `POST /runs`.
#[when("a run is started")]
async fn start_run(world: &mut PolarAlignWorld) {
    let url = format!("{}/runs", world.polar_align_url());
    let resp = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .expect("failed to POST /runs");

    world.last_api_status = Some(resp.status().as_u16());
    let text = resp
        .text()
        .await
        .expect("failed to read the /runs response");
    assert_eq!(
        world.last_api_status,
        Some(202),
        "POST /runs was not accepted: {text}"
    );
    world.last_api_body = serde_json::from_str(&text).ok();
}

#[when(expr = "the polar-align workflow reaches the {string} phase")]
async fn workflow_reaches_phase(world: &mut PolarAlignWorld, expected: String) {
    // Measurement = three OmniSim slews + captures + solves; allow a
    // generous bound and poll the plugin's own status endpoint.
    let client = reqwest::Client::new();
    let url = format!("{}/status", world.polar_align_url());
    let mut last_phase = String::new();
    for _ in 0..720 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Ok(resp) = client.get(&url).send().await {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                last_phase = body
                    .get("phase")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if last_phase == expected {
                    return;
                }
            }
        }
    }
    panic!("polar-align did not reach phase '{expected}' within 180s (last phase: '{last_phase}')");
}

#[when("an invocation without a workflow id is posted via the REST API")]
async fn post_invocation_missing_workflow_id(world: &mut PolarAlignWorld) {
    let client = reqwest::Client::new();
    let url = format!("{}/invoke", world.polar_align_url());
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "mcp_server_url": "http://localhost:1/mcp" }))
        .send()
        .await
        .expect("failed to POST /invoke");
    world.last_api_status = Some(resp.status().as_u16());
    world.last_api_body = resp.json().await.ok();
}

#[when(expr = "the polar-align workflow awaits measurement point {int}")]
async fn workflow_awaits_point(world: &mut PolarAlignWorld, point: u64) {
    let client = reqwest::Client::new();
    let url = format!("{}/status", world.polar_align_url());
    let mut last = String::new();
    for _ in 0..720 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Ok(resp) = client.get(&url).send().await {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                if body
                    .get("awaiting_point")
                    .and_then(serde_json::Value::as_u64)
                    == Some(point)
                {
                    return;
                }
                last = body.to_string();
            }
        }
    }
    panic!("polar-align never awaited measurement point {point} (last status: {last})");
}

#[when("the operator confirms the manual rotation")]
async fn operator_confirms_rotation(world: &mut PolarAlignWorld) {
    let client = reqwest::Client::new();
    let url = format!("{}/measure/continue", world.polar_align_url());
    let resp = client
        .post(&url)
        .send()
        .await
        .expect("failed to POST /measure/continue");
    world.last_api_status = Some(resp.status().as_u16());
    world.last_api_body = resp.json().await.ok();
}

#[when("the preview endpoint is fetched")]
async fn fetch_preview(world: &mut PolarAlignWorld) {
    let client = reqwest::Client::new();
    let url = format!("{}/preview.png", world.polar_align_url());
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /preview.png");
    world.last_api_status = Some(resp.status().as_u16());
    world.last_api_body = resp.json().await.ok();
}

#[when("the adjustment is finished via the REST API")]
async fn finish_adjustment(world: &mut PolarAlignWorld) {
    let client = reqwest::Client::new();
    let url = format!("{}/adjust/finish", world.polar_align_url());
    let resp = client
        .post(&url)
        .send()
        .await
        .expect("failed to POST /adjust/finish");
    world.last_api_status = Some(resp.status().as_u16());
    world.last_api_body = resp.json().await.ok();
}

// ---------------------------------------------------------------------------
// Then steps
// ---------------------------------------------------------------------------

/// The run's end is its `/status` phase — `complete` or `error` — and
/// the workflow slot is free again; nothing is posted to rp.
#[then("the polar-align run has ended")]
async fn run_has_ended(world: &mut PolarAlignWorld) {
    let client = reqwest::Client::new();
    let url = format!("{}/status", world.polar_align_url());
    let mut actual = String::new();
    for _ in 0..120 {
        let resp = client
            .get(&url)
            .send()
            .await
            .expect("failed to GET /status");
        let body: serde_json::Value = resp.json().await.expect("failed to parse /status");
        actual = body
            .get("phase")
            .and_then(|v| v.as_str())
            .expect("phase field missing")
            .to_string();
        if actual == "complete" || actual == "error" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("the polar-align run had not ended after 30s (phase: {actual})");
}

#[then(
    expr = "the polar-align status should report an azimuth error within {float} arcminutes of {float}"
)]
async fn status_azimuth_error(world: &mut PolarAlignWorld, tolerance: f64, expected: f64) {
    let value = measurement_field(world, "azimuth_error_arcmin").await;
    assert!(
        (value - expected).abs() <= tolerance,
        "azimuth error {value}′ not within {tolerance}′ of {expected}′"
    );
}

#[then(
    expr = "the polar-align status should report an altitude error within {float} arcminutes of {float}"
)]
async fn status_altitude_error(world: &mut PolarAlignWorld, tolerance: f64, expected: f64) {
    let value = measurement_field(world, "altitude_error_arcmin").await;
    assert!(
        (value - expected).abs() <= tolerance,
        "altitude error {value}′ not within {tolerance}′ of {expected}′"
    );
}

#[then(expr = "the polar-align status should report an axis cross-check below {float} arcseconds")]
async fn status_cross_check(world: &mut PolarAlignWorld, max_arcsec: f64) {
    let value = measurement_field(world, "cross_check_arcsec").await;
    assert!(
        value < max_arcsec,
        "axis cross-check {value}″ is not below {max_arcsec}″"
    );
}

#[then(
    expr = "the first solve request should be hinted within {float} degrees of the synced pointing"
)]
async fn first_solve_hint_near_synced_pointing(world: &mut PolarAlignWorld, tolerance: f64) {
    let synced_ra_deg = world
        .synced_ra_deg
        .expect("no synced pointing recorded by a Given step");
    let stub = world
        .plate_solver_stub
        .as_ref()
        .expect("plate solver stub not started");
    let requests = stub.requests().await;
    let first = requests.first().expect("no solve requests recorded");
    let ra_hint = first
        .get("ra_hint")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("first solve request carries no ra_hint: {first}"));
    let dec_hint = first
        .get("dec_hint")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("first solve request carries no dec_hint: {first}"));
    // The first point of a current-position measurement is captured
    // in place, so its hint must be the mount's own position — which
    // pins the tool's hours→degrees normalization too. Tracking
    // holds RA still between the sync and the capture.
    let ra_diff = (ra_hint - synced_ra_deg + 180.0).rem_euclid(360.0) - 180.0;
    assert!(
        ra_diff.abs() <= tolerance,
        "first solve ra_hint {ra_hint}° is {ra_diff:.3}° from the synced RA {synced_ra_deg}°"
    );
    assert!(
        (dec_hint - 60.0).abs() <= tolerance,
        "first solve dec_hint {dec_hint}° is not the synced declination 60°"
    );
}

#[then("the first solve request should carry no pointing hint")]
async fn first_solve_request_unhinted(world: &mut PolarAlignWorld) {
    let stub = world
        .plate_solver_stub
        .as_ref()
        .expect("plate solver stub not started");
    let requests = stub.requests().await;
    let first = requests.first().expect("no solve requests recorded");
    // Manual rotation solves blind AND pins rp's mount hints off — a
    // hint from the (unrelated) simulator mount would defeat the
    // no-mount contract.
    assert!(
        first.get("ra_hint").is_none() && first.get("dec_hint").is_none(),
        "first solve request must be blind, got: {first}"
    );
}

#[then(expr = "the preview endpoint should serve a PNG at most {int} pixels wide")]
async fn preview_serves_png(world: &mut PolarAlignWorld, max_width: u32) {
    let client = reqwest::Client::new();
    let url = format!("{}/preview.png", world.polar_align_url());
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /preview.png");
    assert_eq!(resp.status().as_u16(), 200, "preview must be available");
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
    let bytes = resp.bytes().await.expect("failed to read preview body");
    assert!(
        bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "body does not start with the PNG magic"
    );
    // IHDR width lives at bytes 16..20, big-endian.
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("truncated IHDR"));
    assert!(
        width > 0 && width <= max_width,
        "preview width {width} outside (0, {max_width}]"
    );
}

#[then(expr = "the continue request should be rejected with status {int}")]
async fn continue_rejected(world: &mut PolarAlignWorld, expected: u16) {
    assert_eq!(
        world.last_api_status,
        Some(expected),
        "unexpected /measure/continue status (body: {:?})",
        world.last_api_body
    );
}

#[then(expr = "the preview request should be rejected with status {int}")]
async fn preview_rejected(world: &mut PolarAlignWorld, expected: u16) {
    assert_eq!(
        world.last_api_status,
        Some(expected),
        "unexpected /preview.png status (body: {:?})",
        world.last_api_body
    );
}

#[then(expr = "the stub plate solver should have received at least {int} solve requests")]
async fn stub_received_requests(world: &mut PolarAlignWorld, count: usize) {
    let stub = world
        .plate_solver_stub
        .as_ref()
        .expect("plate solver stub not started");
    let requests = stub.requests().await;
    assert!(
        requests.len() >= count,
        "expected at least {} solve requests, saw {}",
        count,
        requests.len()
    );
}

#[then(expr = "the finish request should be rejected with status {int}")]
async fn finish_rejected(world: &mut PolarAlignWorld, expected: u16) {
    assert_eq!(
        world.last_api_status,
        Some(expected),
        "unexpected /adjust/finish status (body: {:?})",
        world.last_api_body
    );
}

#[then(expr = "the invoke request should be rejected with status {int}")]
async fn invoke_rejected(world: &mut PolarAlignWorld, expected: u16) {
    assert_eq!(
        world.last_api_status,
        Some(expected),
        "unexpected /invoke status (body: {:?})",
        world.last_api_body
    );
}

#[then(expr = "the polar-align status error should mention {string}")]
async fn polar_align_error_mentions(world: &mut PolarAlignWorld, needle: String) {
    let client = reqwest::Client::new();
    let url = format!("{}/status", world.polar_align_url());
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /status");
    let body: serde_json::Value = resp.json().await.expect("failed to parse /status");
    let error = body
        .get("error")
        .and_then(|e| e.as_str())
        .unwrap_or_default();
    assert!(
        error.contains(&needle),
        "status error {error:?} does not mention {needle:?}"
    );
}

#[then(expr = "the polar-align workflow phase should be {string}")]
async fn polar_align_phase_is(world: &mut PolarAlignWorld, expected: String) {
    let client = reqwest::Client::new();
    let url = format!("{}/status", world.polar_align_url());
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /status");
    let body: serde_json::Value = resp.json().await.expect("failed to parse /status");
    let phase = body.get("phase").and_then(|p| p.as_str()).unwrap_or("");
    assert_eq!(phase, expected, "unexpected phase (body: {body})");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn measurement_field(world: &mut PolarAlignWorld, field: &str) -> f64 {
    let client = reqwest::Client::new();
    let url = format!("{}/status", world.polar_align_url());
    let body: serde_json::Value = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /status")
        .json()
        .await
        .expect("failed to parse /status");
    body.get("measurement")
        .and_then(|m| m.get(field))
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("measurement.{field} missing from /status: {body}"))
}

async fn configure_default_equipment(world: &mut PolarAlignWorld) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
    let alpaca_url = world
        .omnisim
        .as_ref()
        .expect("OmniSim just started")
        .base_url
        .clone();

    if world.cameras.is_empty() {
        world.cameras.push(bdd_infra::rp_harness::CameraConfig {
            id: "main-cam".to_string(),
            alpaca_url: alpaca_url.clone(),
            device_number: 0,
            cooler_targets_c: Vec::new(),
        });
    }
    if world.mount.is_none() {
        world.mount = Some(bdd_infra::rp_harness::MountConfig {
            alpaca_url,
            device_number: 0,
            settle_after_slew: None,
        });
    }
}

async fn start_polar_align_service(world: &mut PolarAlignWorld) {
    if world.polar_align.is_some() {
        return;
    }
    let mut config = PolarAlignWorld::polar_align_service_config();
    if world.omit_plugin_site {
        config
            .as_object_mut()
            .expect("service config is an object")
            .remove("site");
    }
    if let Some(adjustment) = config.get_mut("adjustment").and_then(|a| a.as_object_mut()) {
        for (key, value) in &world.adjustment_overrides {
            adjustment.insert(key.clone(), value.clone());
        }
    }
    if let Some(measurement) = config
        .get_mut("measurement")
        .and_then(|m| m.as_object_mut())
    {
        for (key, value) in &world.measurement_overrides {
            measurement.insert(key.clone(), value.clone());
        }
    }
    // The service is an MCP client of rp: its config names the endpoint
    // every `POST /runs` run connects to. The standalone scenarios run
    // without rp and never start a run.
    if let Some(rp) = world.rp.as_ref() {
        config["mcp_server_url"] = serde_json::json!(format!("{}/mcp", rp.base_url));
    }
    let config_path = write_temp_config_file("polar-align-config", &config).await;
    world.polar_align = Some(ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await);
}

async fn start_rp_service(world: &mut PolarAlignWorld) {
    if world
        .rp
        .as_ref()
        .is_some_and(bdd_infra::ServiceHandle::is_running)
    {
        return;
    }

    let config = world.build_rp_config();
    world.rp = Some(start_rp(&config).await);

    assert!(
        world.wait_for_rp_healthy().await,
        "rp did not become healthy within timeout"
    );
}
