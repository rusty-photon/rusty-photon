//! Steps of `guiding.feature`, `dither_and_pause.feature` and
//! `statistics.feature`: guiding, dither, pause and the statistics.

use std::time::{Duration, Instant};

use cucumber::{given, then, when};
use guide_simulator::{GuideDirection, PierSide, SkyOffset, SIDEREAL_ARCSEC_PER_S};
use serde_json::{json, Value};

use crate::steps::loop_steps::latest;
use crate::world::GuiderWorld;

/// When the calibration the scenario stores was made.
const STORED_AT: &str = "2026-11-20T00:00:00Z";

/// The state directory the configuration names.
fn state_dir(world: &GuiderWorld) -> std::path::PathBuf {
    std::path::PathBuf::from(
        world.config["state_dir"]
            .as_str()
            .expect("the configuration names a state directory"),
    )
}

/// `GET /api/v1/guiding/stats`, answered 200.
async fn stats(world: &GuiderWorld) -> Value {
    let (status, body) = world
        .get("/api/v1/guiding/stats")
        .await
        .expect("GET /api/v1/guiding/stats");
    assert_eq!(
        status, 200,
        "GET /api/v1/guiding/stats answered {status}: {body}"
    );
    body
}

/// A number of `body` at `pointer`.
fn number(body: &Value, pointer: &str) -> f64 {
    body.pointer(pointer)
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("{pointer} in {body}"))
}

#[given("the guider configuration settles within 1.5 px for 1 s in at most 30 s")]
fn settles_within(world: &mut GuiderWorld) {
    world.set_field(
        "/settling",
        json!({ "pixels": 1.5, "time": "1s", "timeout": "30s" }),
    );
}

#[given("the state directory holds a calibration of the simulated rig")]
fn stored_calibration(world: &mut GuiderWorld) {
    // The simulated mount reports its sidereal guide rate in degrees a
    // second as this does, so the record serves its runs.
    let rate_deg_per_s = SIDEREAL_ARCSEC_PER_S / 3_600.0;
    let rate_px_per_s = SIDEREAL_ARCSEC_PER_S / 2.0;
    let axis = |angle_deg: f64| json!({ "angle_deg": angle_deg, "rate_px_per_s": rate_px_per_s, "steps": 4, "step": "200ms" });
    let mut dec = axis(270.0);
    dec["backlash"] = json!("0s");
    let record = json!({
        "calibrated_at": STORED_AT,
        "pier_side": "east",
        "declination_deg": 0.0,
        "pixel_scale_arcsec": 2.0,
        "focal_length_mm": 206.265,
        "camera": { "pixel_size_um": 2.0, "width": 320, "height": 240, "binning": 1 },
        "guide_rates": { "ra_deg_per_s": rate_deg_per_s, "dec_deg_per_s": rate_deg_per_s },
        "ra": axis(0.0),
        "dec": dec,
        "orthogonality_error_deg": 0.0,
        "expected": { "ra_rate_px_per_s": rate_px_per_s, "dec_rate_px_per_s": rate_px_per_s },
        "warnings": []
    });
    let dir = state_dir(world);
    std::fs::create_dir_all(&dir).expect("create the state directory");
    std::fs::write(
        dir.join("calibration.json"),
        json!({ "east": record, "west": null, "unknown": null }).to_string(),
    )
    .expect("write calibration.json");
}

#[given(expr = "the simulated mount drifts {float}″ a minute east")]
fn drifts(world: &mut GuiderWorld, arcsec_per_min: f64) {
    world.sim_config().sim.mount.drift.east_arcsec_per_min = arcsec_per_min;
}

#[given(regex = r"^the simulated field jumps (\d+)″ (east|west)$")]
#[when(regex = r"^the simulated field jumps (\d+)″ (east|west)$")]
#[then(regex = r"^the simulated field jumps (\d+)″ (east|west)$")]
fn field_jumps(world: &mut GuiderWorld, arcsec: f64, direction: String) {
    let east = if direction == "east" { arcsec } else { -arcsec };
    world
        .sim()
        .slew(
            SkyOffset { east, north: 0.0 },
            Duration::ZERO,
            PierSide::East,
        )
        .expect("move the simulated field");
}

#[when("a client asks the guider to start guiding")]
async fn ask_to_start(world: &mut GuiderWorld) {
    world.post("/api/v1/guiding/start", None).await;
}

#[when(regex = r"^a client asks the guider to start guiding with the body (.+)$")]
async fn ask_to_start_with(world: &mut GuiderWorld, body: String) {
    world.post("/api/v1/guiding/start", Some(&body)).await;
}

#[given(
    regex = r"^a client asks the guider to start guiding in the background with the body (.+)$"
)]
fn ask_to_start_in_background(world: &mut GuiderWorld, body: String) {
    world.post_in_background("/api/v1/guiding/start", Some(&body));
}

#[given("the guider is guiding")]
async fn is_guiding(world: &mut GuiderWorld) {
    world.post("/api/v1/guiding/start", None).await;
    let answer = latest(world);
    assert_eq!(
        (answer.status, &answer.json()["state"]),
        (200, &Value::from("guiding")),
        "guiding did not start: {answer:?}"
    );
}

#[when(regex = r"^a client asks the guider to dither with the body (.+)$")]
async fn ask_to_dither(world: &mut GuiderWorld, body: String) {
    world.post("/api/v1/dither", Some(&body)).await;
}

#[given(regex = r"^a client asks the guider to dither in the background with the body (.+)$")]
fn ask_to_dither_in_background(world: &mut GuiderWorld, body: String) {
    world.post_in_background("/api/v1/dither", Some(&body));
}

#[when("a client asks the guider to abort the dither")]
async fn ask_to_abort(world: &mut GuiderWorld) {
    world.post("/api/v1/dither/abort", None).await;
}

#[when("a client asks the guider to pause")]
async fn ask_to_pause(world: &mut GuiderWorld) {
    world.post("/api/v1/guiding/pause", None).await;
}

#[when(regex = r"^a client asks the guider to pause with the body (.+)$")]
async fn ask_to_pause_with(world: &mut GuiderWorld, body: String) {
    world.post("/api/v1/guiding/pause", Some(&body)).await;
}

#[when("a client asks the guider to resume")]
#[then("a client asks the guider to resume")]
async fn ask_to_resume(world: &mut GuiderWorld) {
    world.post("/api/v1/guiding/resume", None).await;
}

#[then("the simulated mount has received a pulse")]
fn a_pulse(world: &mut GuiderWorld) {
    assert_ne!(
        world.sim().pulses(),
        [],
        "the simulated mount has received no pulse"
    );
}

#[then(r#"the request answers 200 with the state "guiding" and a settled answer"#)]
fn settled_answer(world: &mut GuiderWorld) {
    let answer = latest(world);
    assert_eq!(answer.status, 200, "the request was refused: {answer:?}");
    let body = answer.json();
    assert_eq!(body["state"], "guiding", "{body}");
    let samples = body["sample_count"].as_u64().expect("a sample count");
    for field in ["rms_ra_px", "rms_ra_arcsec"] {
        assert_eq!(
            body[field].is_number(),
            samples > 0,
            "{field} with {samples} samples in {body}"
        );
    }
}

#[then(
    regex = r#"^the guider's stored calibration for the "(east|west|unknown)" pier side is still the one the state directory held$"#
)]
async fn still_stored(world: &mut GuiderWorld, side: String) {
    let (_, store) = world
        .get("/api/v1/calibration")
        .await
        .expect("GET /api/v1/calibration");
    assert_eq!(store[side.as_str()]["calibrated_at"], STORED_AT, "{store}");
}

#[then(
    regex = r#"^the guider's stored calibration for the "(east|west|unknown)" pier side is a record$"#
)]
async fn stored_record(world: &mut GuiderWorld, side: String) {
    let (_, store) = world
        .get("/api/v1/calibration")
        .await
        .expect("GET /api/v1/calibration");
    assert!(store[side.as_str()].is_object(), "{store}");
}

#[then(
    "the simulated mount received West pulses, then East, then North, then South, before it guided"
)]
fn calibrated_first(world: &mut GuiderWorld) {
    let mut runs: Vec<GuideDirection> = Vec::new();
    for pulse in world.sim().pulses() {
        if runs.last() != Some(&pulse.direction) {
            runs.push(pulse.direction);
        }
    }
    assert_eq!(
        runs.get(..4),
        Some(
            &[
                GuideDirection::West,
                GuideDirection::East,
                GuideDirection::North,
                GuideDirection::South
            ][..]
        ),
        "the directions of the pulses, run together: {runs:?}"
    );
}

#[then("the guider's stats report guiding true and settling false")]
async fn guiding_not_settling(world: &mut GuiderWorld) {
    let body = stats(world).await;
    assert_eq!(
        (&body["guiding"], &body["settling"]),
        (&Value::Bool(true), &Value::Bool(false)),
        "{body}"
    );
}

#[given("the guider's stats report settling true")]
async fn settling(world: &mut GuiderWorld) {
    world
        .wait_for("/api/v1/guiding/stats", "settling", |status, body| {
            status == 200 && body["settling"] == Value::Bool(true)
        })
        .await;
}

#[given(regex = r"^the guider's totals count at least (\d+) guide steps$")]
#[when(regex = r"^the guider's totals count at least (\d+) guide steps$")]
async fn guide_steps(world: &mut GuiderWorld, count: u64) {
    let (_, body) = world
        .wait_for(
            "/api/v1/guiding/stats",
            &format!("{count} guide steps"),
            |status, body| {
                status == 200
                    && body["totals"]["guide_steps"]
                        .as_u64()
                        .is_some_and(|steps| steps >= count)
            },
        )
        .await;
    world.noted_guide_steps = body["totals"]["guide_steps"].as_u64().unwrap_or(0);
    let (_, metrics) = world
        .get("/api/v1/guiding/metrics")
        .await
        .expect("GET /api/v1/guiding/metrics");
    world.noted_frames = metrics["frames"]
        .as_array()
        .map_or_else(Vec::new, |frames| {
            frames.iter().filter_map(|f| f["frame"].as_u64()).collect()
        });
}

#[then(regex = r"^the guider's totals count at least (\d+) (star-lost frames?|interruptions?)$")]
async fn totals_count(world: &mut GuiderWorld, count: u64, what: String) {
    let field = if what.starts_with("star-lost") {
        "star_lost"
    } else {
        "interruptions"
    };
    world
        .wait_for(
            "/api/v1/guiding/stats",
            &format!("{count} {what}"),
            |status, body| {
                status == 200
                    && body["totals"][field]
                        .as_u64()
                        .is_some_and(|counted| counted >= count)
            },
        )
        .await;
}

#[then("the guider's stats report an RA RMS below 1.5 px")]
async fn ra_rms_below(world: &mut GuiderWorld) {
    let body = stats(world).await;
    assert!(number(&body, "/rms_ra_px") < 1.5, "{body}");
}

#[then("the guider's stats report RMS fields in pixels and in arcseconds at 2″ a pixel")]
async fn rms_fields(world: &mut GuiderWorld) {
    let body = stats(world).await;
    assert!(body["sample_count"].as_u64().unwrap_or(0) > 0, "{body}");
    for (px, arcsec) in [
        ("/rms_ra_px", "/rms_ra_arcsec"),
        ("/rms_dec_px", "/rms_dec_arcsec"),
        ("/total_rms_px", "/total_rms_arcsec"),
    ] {
        let (px, arcsec) = (number(&body, px), number(&body, arcsec));
        assert!(
            (arcsec - 2.0 * px).abs() < 1e-9,
            "{px} px against {arcsec}″ in {body}"
        );
    }
    let (ra, dec, total) = (
        number(&body, "/rms_ra_px"),
        number(&body, "/rms_dec_px"),
        number(&body, "/total_rms_px"),
    );
    assert!((total - ra.hypot(dec)).abs() < 1e-9, "{body}");
}

#[then("the guider's totals count as many RA and Dec samples as guide steps")]
async fn samples_as_steps(world: &mut GuiderWorld) {
    let totals = stats(world).await["totals"].clone();
    let steps = &totals["guide_steps"];
    assert_eq!(&totals["ra_samples"], steps, "{totals}");
    assert_eq!(&totals["dec_samples"], steps, "{totals}");
    assert!(number(&totals, "/ra_sum_sq_px2") >= 0.0, "{totals}");
}

#[then("the guider's totals count at least the guide steps they counted before the request")]
async fn totals_go_on(world: &mut GuiderWorld) {
    let totals = stats(world).await["totals"].clone();
    let steps = totals["guide_steps"].as_u64().unwrap_or(0);
    assert!(
        steps >= world.noted_guide_steps,
        "the totals went back from {} to {steps}: {totals}",
        world.noted_guide_steps
    );
}

#[then("the guider's stats report no Dec or total RMS")]
async fn no_dec_rms(world: &mut GuiderWorld) {
    let body = stats(world).await;
    for field in [
        "rms_dec_px",
        "total_rms_px",
        "rms_dec_arcsec",
        "total_rms_arcsec",
    ] {
        assert_eq!(body[field], Value::Null, "{field} in {body}");
    }
    assert!(body["rms_ra_px"].is_number(), "{body}");
}

#[then("the guider's totals count no Dec sample")]
async fn no_dec_sample(world: &mut GuiderWorld) {
    let totals = stats(world).await["totals"].clone();
    assert_eq!(totals["dec_samples"], json!(0), "{totals}");
    assert_eq!(totals["dec_sum_sq_px2"], json!(0.0), "{totals}");
}

#[then(regex = r"^the simulated camera starts no exposure for (\d+) seconds$")]
async fn no_exposure_for(world: &mut GuiderWorld, seconds: u64) {
    let before = world.sim().exposures().len();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        assert_eq!(
            world.sim().exposures().len(),
            before,
            "the camera started an exposure in a full pause"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    world.noted_exposures = before;
}

#[then("the simulated camera starts exposures again")]
async fn exposures_again(world: &mut GuiderWorld) {
    let before = world.noted_exposures;
    world
        .wait_until("an exposure after the pause", || {
            world.sim().exposures().len() > before
        })
        .await;
}
