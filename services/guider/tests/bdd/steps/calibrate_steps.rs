//! Steps of `calibrate.feature`: the simulated mount a calibration pulses,
//! `calibrate`, and the record it answers.

use std::time::Duration;

use cucumber::{given, then, when};
use guide_simulator::{GuideDirection, PierSide, SkyOffset};
use serde_json::{json, Value};

use crate::steps::config_steps::json_value;
use crate::steps::loop_steps::{assert_error, latest};
use crate::world::GuiderWorld;

/// How long a slew that outlasts a scenario takes.
const LONG_SLEW: Duration = Duration::from_secs(3_600);

/// The furthest an axis's angle may lie from the one the simulated sensor
/// gives it, in degrees.
const ANGLE_TOLERANCE_DEG: f64 = 5.0;

/// The furthest a calibrated rate may lie from the one the guide rates give,
/// as a fraction.
const RATE_TOLERANCE: f64 = 0.2;

#[given("the simulated mount guides at the sidereal rate")]
fn sidereal_guide_rate(world: &mut GuiderWorld) {
    let mount = &mut world.sim_config().sim.mount;
    mount.guide_rate_ra = 1.0;
    mount.guide_rate_dec = 1.0;
}

#[given(expr = "the simulated mount points at declination {float}°")]
fn points_at_declination(world: &mut GuiderWorld, declination_deg: f64) {
    world.sim_config().sim.mount.declination_deg = declination_deg;
}

#[given("the simulated mount reports no guide rate")]
fn no_guide_rate(world: &mut GuiderWorld) {
    world.sim_config().mount.reports_guide_rates = false;
}

#[given("the simulated mount reports no pier side")]
fn no_pier_side(world: &mut GuiderWorld) {
    world.sim_config().mount.reports_pier_side = false;
}

#[given("the guider configuration calibrates over 6 px in 4 steps")]
fn short_calibration(world: &mut GuiderWorld) {
    world.set_field("/calibration/distance_px", json!(6));
    world.set_field("/calibration/steps", json!(4));
    world.set_field("/calibration/max_steps", json!(8));
    world.set_field("/calibration/backlash_clear_px", json!(1));
}

#[given("the simulated mount is parked")]
#[when("the simulated mount is parked")]
fn parked(world: &mut GuiderWorld) {
    world.sim().park();
}

#[given("the simulated mount stops tracking")]
#[when("the simulated mount stops tracking")]
fn stops_tracking(world: &mut GuiderWorld) {
    world
        .sim()
        .set_tracking(false)
        .expect("turn the simulated mount's tracking off");
}

#[given("the simulated mount is slewing")]
#[when("the simulated mount starts a slew of an hour")]
fn slewing(world: &mut GuiderWorld) {
    world
        .sim()
        .slew(
            SkyOffset {
                east: 3_600.0,
                north: 0.0,
            },
            LONG_SLEW,
            PierSide::East,
        )
        .expect("slew the simulated mount");
}

#[when("the simulated mount turns to the West side")]
fn turns_west(world: &mut GuiderWorld) {
    world
        .sim()
        .slew(
            SkyOffset {
                east: 0.0,
                north: 0.0,
            },
            Duration::ZERO,
            PierSide::West,
        )
        .expect("turn the simulated mount to the West side");
}

#[given("the simulated mount cannot pulse guide now")]
fn cannot_pulse_guide_now(world: &mut GuiderWorld) {
    world.sim().set_can_pulse_guide(false);
}

#[given(expr = "the simulated mount's pulses move it at {float} of their rate")]
#[when(expr = "the simulated mount's pulses move it at {float} of their rate")]
fn rate_factor(world: &mut GuiderWorld, factor: f64) {
    world
        .sim()
        .set_rate_factors(factor, factor)
        .expect("set the simulated mount's rate factors");
}

#[given("the simulated mount's next pulse sticks")]
fn next_pulse_sticks(world: &mut GuiderWorld) {
    world.sim().set_pulses_stuck(true);
}

#[when("a client asks the guider to calibrate")]
#[given("a client asks the guider to calibrate")]
async fn ask_to_calibrate(world: &mut GuiderWorld) {
    world.post("/api/v1/calibrate", None).await;
}

#[given("a client asks the guider to calibrate in the background")]
fn ask_to_calibrate_in_background(world: &mut GuiderWorld) {
    world.post_in_background("/api/v1/calibrate", None);
}

#[given("the client gives up waiting")]
fn gives_up(world: &mut GuiderWorld) {
    world
        .background
        .take()
        .expect("the scenario sent no request in the background")
        .abort();
}

#[given("a client asks the guider to clear the calibration store")]
#[when("a client asks the guider to clear the calibration store")]
#[then("a client asks the guider to clear the calibration store")]
async fn ask_to_clear(world: &mut GuiderWorld) {
    world.post("/api/v1/calibration/clear", None).await;
}

#[given("the simulated mount has received a pulse")]
async fn has_received_a_pulse(world: &mut GuiderWorld) {
    world
        .wait_until("a pulse", || !world.sim().pulses().is_empty())
        .await;
}

/// The record a successful calibration answered.
fn answered(world: &GuiderWorld) -> Value {
    let answer = latest(world);
    assert_eq!(
        answer.status, 200,
        "the calibration was refused: {answer:?}"
    );
    answer.json()["calibration"].clone()
}

/// Whether `actual` lies within `tolerance` of `expected`, both angles in
/// degrees.
fn angle_near(actual: f64, expected: f64, tolerance: f64) -> bool {
    let difference = (actual - expected).rem_euclid(360.0);
    difference.min(360.0 - difference) <= tolerance
}

#[then(
    regex = r#"^the calibration answered was made on the "(east|west|unknown)" pier side at declination (-?\d+)$"#
)]
fn made_on(world: &mut GuiderWorld, side: String, declination: f64) {
    let record = answered(world);
    assert_eq!(record["pier_side"], Value::from(side), "{record}");
    let declination_deg = record["declination_deg"].as_f64().unwrap_or(f64::NAN);
    assert!(
        (declination_deg - declination).abs() < 1e-6,
        "the declination in {record}"
    );
}

#[then(
    "the calibration answered is of a 2 µm sensor of 320 × 240 at binning 1, a pixel scale of 2″ and a focal length of 206.265 mm"
)]
fn made_with(world: &mut GuiderWorld) {
    let record = answered(world);
    assert_eq!(
        record["camera"],
        json!({ "pixel_size_um": 2.0, "width": 320, "height": 240, "binning": 1 }),
        "{record}"
    );
    assert_eq!(record["pixel_scale_arcsec"], json!(2.0), "{record}");
    assert_eq!(record["focal_length_mm"], json!(206.265), "{record}");
}

#[then(
    "the calibration answered moves the stars at about 0° for West and 270° for North, within 20% of the rates the guide rates give"
)]
fn axes(world: &mut GuiderWorld) {
    let record = answered(world);
    let number = |pointer: &str| {
        record
            .pointer(pointer)
            .and_then(Value::as_f64)
            .unwrap_or_else(|| panic!("{pointer} in {record}"))
    };
    assert!(
        angle_near(number("/ra/angle_deg"), 0.0, ANGLE_TOLERANCE_DEG),
        "the RA axis's angle in {record}"
    );
    assert!(
        angle_near(number("/dec/angle_deg"), 270.0, ANGLE_TOLERANCE_DEG),
        "the Dec axis's angle in {record}"
    );
    for (rate, expected) in [
        ("/ra/rate_px_per_s", "/expected/ra_rate_px_per_s"),
        ("/dec/rate_px_per_s", "/expected/dec_rate_px_per_s"),
    ] {
        let (rate, expected) = (number(rate), number(expected));
        assert!(
            (rate / expected - 1.0).abs() <= RATE_TOLERANCE,
            "a rate of {rate} against {expected} in {record}"
        );
    }
}

#[then("the calibration answered has no warning")]
fn no_warning(world: &mut GuiderWorld) {
    let record = answered(world);
    assert_eq!(record["warnings"], json!([]), "{record}");
}

#[then("the calibration answered has no Dec axis")]
fn no_dec_axis(world: &mut GuiderWorld) {
    let record = answered(world);
    assert_eq!(record["dec"], Value::Null, "{record}");
    assert_eq!(record["orthogonality_error_deg"], Value::Null, "{record}");
}

#[then("the calibration answered has no guide rates and no expected rates")]
fn no_guide_rates(world: &mut GuiderWorld) {
    let record = answered(world);
    assert_eq!(record["guide_rates"], Value::Null, "{record}");
    assert_eq!(record["expected"], Value::Null, "{record}");
}

#[then(regex = r#"^the calibration answered steps each axis by "([^"]+)"$"#)]
fn steps_by(world: &mut GuiderWorld, pulse: String) {
    let record = answered(world);
    assert_eq!(
        record["ra"]["step"],
        Value::from(pulse.as_str()),
        "{record}"
    );
    assert_eq!(
        record["dec"]["step"],
        Value::from(pulse.as_str()),
        "{record}"
    );
}

/// The simulated mount's pulses, their directions run together.
fn directions(world: &GuiderWorld) -> Vec<GuideDirection> {
    let mut runs: Vec<GuideDirection> = Vec::new();
    for pulse in world.sim().pulses() {
        if runs.last() != Some(&pulse.direction) {
            runs.push(pulse.direction);
        }
    }
    runs
}

#[then("the simulated mount received West pulses, then East, then North, then South")]
fn pulsed_in_order(world: &mut GuiderWorld) {
    assert_eq!(
        directions(world),
        [
            GuideDirection::West,
            GuideDirection::East,
            GuideDirection::North,
            GuideDirection::South
        ],
        "the directions of the pulses, run together"
    );
}

#[then("the simulated mount received West pulses, then East, and no other")]
fn pulsed_ra_only(world: &mut GuiderWorld) {
    assert_eq!(
        directions(world),
        [GuideDirection::West, GuideDirection::East],
        "the directions of the pulses, run together"
    );
}

#[then(
    regex = r#"^the request answers (\d+) with the error "([a-z_]+)" and a message naming "(.+)"$"#
)]
fn answers_with_message(world: &mut GuiderWorld, status: u16, code: String, text: String) {
    let answer = latest(world).clone();
    assert_error(&answer, status, &code);
    let message = answer.json()["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(&text),
        "the message does not name {text:?}: {answer:?}"
    );
}

#[then(regex = r#"^the request answers (\d+) with the error "([a-z_]+)" and the details (.+)$"#)]
fn answers_with_details(world: &mut GuiderWorld, status: u16, code: String, details: String) {
    let answer = latest(world).clone();
    assert_error(&answer, status, &code);
    assert_eq!(answer.json()["details"], json_value(&details), "{answer:?}");
}

#[then(
    regex = r#"^the request answers (\d+) with the error "([a-z_]+)" and the cross-check "([a-z_]+)" failing above (.+)$"#
)]
fn answers_with_check(
    world: &mut GuiderWorld,
    status: u16,
    code: String,
    check: String,
    threshold: f64,
) {
    let answer = latest(world).clone();
    assert_error(&answer, status, &code);
    let details = &answer.json()["details"];
    assert_eq!(details["check"], Value::from(check), "{answer:?}");
    assert_eq!(details["threshold"], json!(threshold), "{answer:?}");
    assert!(
        details["value"]
            .as_f64()
            .is_some_and(|value| value > threshold),
        "{answer:?}"
    );
}

#[then(regex = r"^the guider's metrics gain at least (\d+) more frames$")]
async fn gain_after_calibration(world: &mut GuiderWorld, count: u64) {
    let after = crate::steps::stats_steps::last_frame(world)
        .await
        .unwrap_or(0);
    let target = after + count;
    world
        .wait_for(
            "/api/v1/guiding/metrics",
            &format!("a frame numbered {target} or above"),
            |status, body| {
                status == 200
                    && body["frames"]
                        .as_array()
                        .and_then(|frames| frames.last())
                        .and_then(|frame| frame["frame"].as_u64())
                        .is_some_and(|frame| frame >= target)
            },
        )
        .await;
}
