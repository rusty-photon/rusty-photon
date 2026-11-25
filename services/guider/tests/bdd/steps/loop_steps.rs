//! Steps of `loop.feature`, and the request and answer steps every feature
//! of the camera loop shares.

use cucumber::{given, then, when};
use guide_simulator::{SkyOffset, Star, StarField};
use guide_simulator_alpaca::Device;
use serde_json::Value;

use crate::steps::config_steps::json_value;
use crate::world::{Answer, GuiderWorld};

/// The stars a scenario that lists its stars places, in arcseconds east and
/// north of the frame's centre. At the simulated 2″ a pixel, and in the
/// 160 × 120 frame of binning 2, they lie at least 30 pixels apart, twice
/// `stars.search_region_px`, and at least 45 pixels inside every edge,
/// beyond the 40 pixels of margin selection keeps
/// (`calibration.distance_px` plus `stars.search_region_px`). Each stands
/// far above the noise in a 200 ms exposure and far below the clip.
const LISTED: [(f64, f64); 4] = [(0.0, 0.0), (-120.0, -60.0), (120.0, 60.0), (120.0, -60.0)];
const LISTED_E_PER_S: f64 = 50_000.0;

#[given(expr = "the simulated sky holds {int} stars")]
fn sky_holds(world: &mut GuiderWorld, count: usize) {
    assert!(
        count <= LISTED.len(),
        "the suite lists {} stars",
        LISTED.len()
    );
    world.sim_config().sim.sky.stars = StarField::Listed(
        LISTED
            .iter()
            .take(count)
            .map(|&(east, north)| Star {
                position: SkyOffset { east, north },
                e_per_s: LISTED_E_PER_S,
            })
            .collect(),
    );
}

#[given("the guider is started")]
async fn started(world: &mut GuiderWorld) {
    world.start_guider().await;
    assert!(
        world.guider.is_some(),
        "the guider did not start: {:?}",
        world.refusal
    );
}

#[given(regex = r"^the simulated (camera|mount) is disconnected$")]
#[when(regex = r"^the simulated (camera|mount) is disconnected$")]
fn disconnect(world: &mut GuiderWorld, device: String) {
    let device = if device == "camera" {
        Device::Camera
    } else {
        Device::Mount
    };
    world.sim().set_connected(device, false);
}

#[when("a client asks the guider to loop")]
async fn ask_to_loop(world: &mut GuiderWorld) {
    world.post("/api/v1/loop", None).await;
}

#[when(regex = r"^a client asks the guider to loop with the body (.+)$")]
async fn ask_to_loop_with(world: &mut GuiderWorld, body: String) {
    world.post("/api/v1/loop", Some(&body)).await;
}

#[given("a client asks the guider to loop in the background")]
fn ask_to_loop_in_background(world: &mut GuiderWorld) {
    world.post_in_background("/api/v1/loop", None);
}

#[given("the guider is looping")]
#[when("the guider is looping")]
async fn is_looping(world: &mut GuiderWorld) {
    world.post("/api/v1/loop", None).await;
    let answer = world.answer.as_ref().expect("the loop was answered");
    assert_eq!(
        (answer.status, &answer.json()["state"]),
        (200, &Value::from("looping")),
        "the loop was refused: {answer:?}"
    );
}

#[then(regex = r"^the request answers (\d+) with the body (.+)$")]
fn answers_with_body(world: &mut GuiderWorld, status: u16, body: String) {
    let answer = latest(world);
    assert_eq!(
        (answer.status, answer.json()),
        (status, &json_value(&body)),
        "the answer"
    );
}

#[then(regex = r#"^the request answers (\d+) with the state "([a-z_]+)"$"#)]
fn answers_with_state(world: &mut GuiderWorld, status: u16, state: String) {
    let answer = latest(world);
    assert_eq!(
        (answer.status, &answer.json()["state"]),
        (status, &Value::from(state)),
        "the answer: {answer:?}"
    );
}

#[then(regex = r#"^the request answers (\d+) with the error "([a-z_]+)"$"#)]
fn answers_with_error(world: &mut GuiderWorld, status: u16, code: String) {
    let answer = latest(world).clone();
    assert_error(&answer, status, &code);
}

#[then(
    regex = r#"^the request answers (\d+) with the error "([a-z_]+)" and the stop reason "([a-z_]+)"$"#
)]
fn answers_with_stop_reason(world: &mut GuiderWorld, status: u16, code: String, reason: String) {
    let answer = latest(world).clone();
    assert_error(&answer, status, &code);
    assert_eq!(
        answer.json()["details"]["stop_reason"],
        Value::from(reason),
        "the stop reason in {answer:?}"
    );
}

#[then(regex = r#"^the background request answers (\d+) with the error "([a-z_]+)"$"#)]
async fn background_answers_with_error(world: &mut GuiderWorld, status: u16, code: String) {
    let answer = background_answer(world).await;
    assert_error(&answer, status, &code);
}

#[then(
    regex = r#"^the background request answers (\d+) with the error "([a-z_]+)" and the stop reason "([a-z_]+)"$"#
)]
async fn background_answers_with_stop_reason(
    world: &mut GuiderWorld,
    status: u16,
    code: String,
    reason: String,
) {
    let answer = background_answer(world).await;
    assert_error(&answer, status, &code);
    assert_eq!(
        answer.json()["details"]["stop_reason"],
        Value::from(reason),
        "the stop reason in {answer:?}"
    );
}

#[then(regex = r#"^the guider's stats report the state "([a-z_]+)"$"#)]
async fn stats_report_state(world: &mut GuiderWorld, state: String) {
    world
        .wait_for(
            "/api/v1/guiding/stats",
            &format!("the state {state}"),
            |status, body| status == 200 && body["state"] == state.as_str(),
        )
        .await;
}

#[then("the simulated camera starts exposure after exposure")]
async fn exposure_after_exposure(world: &mut GuiderWorld) {
    world
        .wait_until("three exposures", || world.sim().exposures().len() >= 3)
        .await;
}

#[given("the simulated camera has started an exposure")]
#[then("the simulated camera has started an exposure")]
async fn started_an_exposure(world: &mut GuiderWorld) {
    world
        .wait_until("an exposure", || !world.sim().exposures().is_empty())
        .await;
}

#[then(
    regex = r"^every exposure of the simulated camera is at bin (\d+) of the region (\d+) × (\d+) at 0, 0$"
)]
fn every_exposure_at(world: &mut GuiderWorld, bin: u8, width: u32, height: u32) {
    let exposures = world.sim().exposures();
    assert!(!exposures.is_empty(), "the camera has started no exposure");
    for exposure in exposures {
        assert_eq!(
            (
                exposure.bin,
                exposure.region.x,
                exposure.region.y,
                exposure.region.width,
                exposure.region.height
            ),
            (bin, 0, 0, width, height),
            "an exposure's binning and region"
        );
    }
}

#[then(regex = r"^every exposure of the simulated camera is at gain (\d+) and offset (\d+)$")]
fn every_exposure_at_gain(world: &mut GuiderWorld, gain: i32, offset: i32) {
    let exposures = world.sim().exposures();
    assert!(!exposures.is_empty(), "the camera has started no exposure");
    for exposure in exposures {
        assert_eq!(
            (exposure.gain, exposure.offset),
            (gain, offset),
            "an exposure's gain and offset"
        );
    }
}

#[given(
    regex = r"^another client sets the simulated camera's gain to (\d+) and its offset to (\d+)$"
)]
#[when(
    regex = r"^another client sets the simulated camera's gain to (\d+) and its offset to (\d+)$"
)]
async fn another_client_sets_gain(world: &mut GuiderWorld, gain: i32, offset: i32) {
    let camera = world.sim_camera().await;
    camera.set_gain(gain).await.expect("set the camera's gain");
    camera
        .set_offset(offset)
        .await
        .expect("set the camera's offset");
}

/// The scenario's latest answer.
pub const fn latest(world: &GuiderWorld) -> &Answer {
    world.answer.as_ref().expect("the scenario sent no request")
}

/// The answer to the request sent in the background.
async fn background_answer(world: &mut GuiderWorld) -> Answer {
    let request = world
        .background
        .take()
        .expect("the scenario sent no request in the background");
    request
        .await
        .expect("the background request's task")
        .unwrap_or_else(|e| panic!("the background request failed: {e}"))
}

/// `answer` is the error envelope with `code` at `status`.
pub fn assert_error(answer: &Answer, status: u16, code: &str) {
    assert_eq!(
        (answer.status, &answer.json()["error"]),
        (status, &Value::from(code)),
        "the answer: {answer:?}"
    );
    assert!(
        answer.json()["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "the error names no message: {answer:?}"
    );
}
