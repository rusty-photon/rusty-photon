//! Steps of `frames.feature`: the simulated camera's scripted faults and
//! sensor changes, and what the guider's loop makes of them.

use std::time::{Duration, Instant};

use cucumber::{given, then, when};
use guide_simulator_alpaca::FrameFault;
use serde_json::Value;

use crate::steps::stats_steps::last_frame;
use crate::world::{GuiderWorld, WAIT_BUDGET};

/// A fault as the feature names it.
fn fault_named(name: &str) -> FrameFault {
    match name {
        "a start that fails" => FrameFault::StartFails,
        "a frame never ready" => FrameFault::NeverReady,
        "a frame of the wrong size" => FrameFault::WrongSize,
        other => panic!("no fault is named {other:?}"),
    }
}

#[given(
    regex = r"^every exposure of the simulated camera fails as (a start that fails|a frame never ready|a frame of the wrong size)$"
)]
#[when(
    regex = r"^every exposure of the simulated camera fails as (a start that fails|a frame never ready|a frame of the wrong size)$"
)]
fn every_exposure_fails(world: &mut GuiderWorld, fault: String) {
    let fault = fault_named(&fault);
    match world.sim.as_ref() {
        Some(sim) => sim.fail_frames(fault, None),
        None => world.frame_faults = Some((fault, None)),
    }
}

#[when(
    regex = r"^the next (\d+) exposures of the simulated camera fail as (a start that fails|a frame never ready|a frame of the wrong size)$"
)]
async fn next_exposures_fail(world: &mut GuiderWorld, count: usize, fault: String) {
    world.noted_frames = vec![last_frame(world).await.unwrap_or(0)];
    world.noted_exposure = world.sim().exposures().last().map(|exposure| exposure.at);
    world.faults_at = Some(world.sim().now());
    world.sim().fail_frames(fault_named(&fault), Some(count));
}

#[then(
    regex = r"^the simulated camera's next exposure starts no sooner than (\d+) s after the last one before the failures$"
)]
async fn next_exposure_no_sooner(world: &mut GuiderWorld, seconds: u64) {
    let before = world
        .noted_exposure
        .expect("an exposure was noted before the failures");
    world
        .wait_until("an exposure after the failures", || {
            world
                .sim()
                .exposures()
                .last()
                .is_some_and(|exposure| exposure.at > before)
        })
        .await;
    let next = world
        .sim()
        .exposures()
        .into_iter()
        .find(|exposure| exposure.at > before)
        .expect("the exposure after the failures");
    // A few milliseconds of slack for the two processes' clocks.
    let earliest =
        before.after(Duration::from_secs(seconds).saturating_sub(Duration::from_millis(20)));
    assert!(
        next.at >= earliest,
        "the next exposure started at {:?}, {seconds} s after {before:?} is {:?}",
        next.at,
        before.after(Duration::from_secs(seconds))
    );
}

#[when(regex = r"^the guider's metrics gain at least (\d+) frames after the failures$")]
#[then(regex = r"^the guider's metrics gain at least (\d+) frames after the failures$")]
async fn gain_frames_after_failures(world: &mut GuiderWorld, count: u64) {
    // Every start after the faults were scripted fails until they are spent,
    // so an exposure the camera recorded after them comes after the
    // failures; frames measured from then on are frames after them.
    let faults_at = world.faults_at.expect("the scenario scripted failures");
    world
        .wait_until("an exposure after the failures", || {
            world
                .sim()
                .exposures()
                .last()
                .is_some_and(|exposure| exposure.at > faults_at)
        })
        .await;
    let before = last_frame(world).await.unwrap_or(0);
    let target = before + count;
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

#[when(expr = "the simulated camera's pixel size becomes {float} µm")]
fn pixel_size_becomes(world: &mut GuiderWorld, pixel_size_um: f64) {
    world.sim().set_pixel_size(pixel_size_um);
}

#[then(regex = r"^the simulated camera's gain reads (\d+) again$")]
async fn gain_reads_again(world: &mut GuiderWorld, gain: i32) {
    let camera = world.sim_camera().await;
    let start = Instant::now();
    let mut last = None;
    while start.elapsed() < WAIT_BUDGET {
        last = camera.gain().await.ok();
        if last == Some(gain) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the camera's gain did not read {gain} within {WAIT_BUDGET:?}; last read {last:?}");
}

#[when(regex = r#"^the guider's last stop has the reason "([a-z_]+)"$"#)]
#[then(regex = r#"^the guider's last stop has the reason "([a-z_]+)"$"#)]
async fn last_stop_reason(world: &mut GuiderWorld, reason: String) {
    last_stop(world, &reason).await;
}

#[then(
    regex = r#"^the guider's last stop has the reason "([a-z_]+)" and a message naming "(.+)"$"#
)]
async fn last_stop_reason_and_message(world: &mut GuiderWorld, reason: String, text: String) {
    let stop = last_stop(world, &reason).await;
    let message = stop["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(&text),
        "the last stop's message does not name {text:?}: {stop}"
    );
}

#[then(regex = r#"^the guider's last stop has the reason "([a-z_]+)" and a time in RFC 3339$"#)]
async fn last_stop_reason_and_time(world: &mut GuiderWorld, reason: String) {
    let stop = last_stop(world, &reason).await;
    let at = stop["at"].as_str().unwrap_or_default();
    humantime::parse_rfc3339(at)
        .unwrap_or_else(|e| panic!("the last stop's time {at:?} is not RFC 3339: {e}"));
}

#[then("the guider's last stop is null")]
async fn last_stop_is_null(world: &mut GuiderWorld) {
    let (status, body) = world
        .get("/api/v1/guiding/stats")
        .await
        .expect("GET /api/v1/guiding/stats");
    assert_eq!(
        status, 200,
        "GET /api/v1/guiding/stats answered {status}: {body}"
    );
    assert_eq!(body["last_stop"], Value::Null, "the last stop in {body}");
}

#[then(
    regex = r#"^the guider's stats report the state "([a-z_]+)" at every read for (\d+) seconds$"#
)]
async fn state_throughout(world: &mut GuiderWorld, state: String, seconds: u64) {
    let window = Duration::from_secs(seconds);
    let start = Instant::now();
    while start.elapsed() < window {
        let (status, body) = world
            .get("/api/v1/guiding/stats")
            .await
            .expect("GET /api/v1/guiding/stats");
        assert_eq!(
            (status, &body["state"]),
            (200, &Value::from(state.as_str())),
            "the guider left {state} within the window: {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The guider's last stop, once its reason is `reason`.
async fn last_stop(world: &GuiderWorld, reason: &str) -> Value {
    let (_, body) = world
        .wait_for(
            "/api/v1/guiding/stats",
            &format!("a last stop with the reason {reason}"),
            |status, body| status == 200 && body["last_stop"]["reason"] == reason,
        )
        .await;
    body["last_stop"].clone()
}
