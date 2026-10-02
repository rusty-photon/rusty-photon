//! Steps for `pulse_guide.feature`.

use crate::world::StarAdventurerWorld;
use cucumber::{given, then, when};
use std::time::{Duration, Instant};

fn parse_direction(s: &str) -> ascom_alpaca::api::telescope::GuideDirection {
    use ascom_alpaca::api::telescope::GuideDirection;
    match s {
        "North" => GuideDirection::North,
        "South" => GuideDirection::South,
        "East" => GuideDirection::East,
        "West" => GuideDirection::West,
        other => panic!("unknown guide direction {other:?}"),
    }
}

#[when(expr = "I pulse guide {word} for {int} ms")]
async fn pulse_guide(world: &mut StarAdventurerWorld, direction: String, ms: u64) {
    let dir = parse_direction(&direction);
    world
        .mount()
        .pulse_guide(dir, Duration::from_millis(ms))
        .await
        .unwrap();
}

#[when(expr = "I try to pulse guide {word} for {int} ms")]
async fn try_pulse_guide(world: &mut StarAdventurerWorld, direction: String, ms: u64) {
    let dir = parse_direction(&direction);
    match world
        .mount()
        .pulse_guide(dir, Duration::from_millis(ms))
        .await
    {
        Ok(()) => world.clear_error(),
        Err(e) => world.record_error(e),
    }
}

#[when(expr = "I set GuideRateRightAscension to {float}")]
async fn set_guide_rate_ra(world: &mut StarAdventurerWorld, value: f64) {
    world
        .mount()
        .set_guide_rate_right_ascension(value)
        .await
        .unwrap();
}

#[when(expr = "I set GuideRateDeclination to {float}")]
async fn set_guide_rate_dec(world: &mut StarAdventurerWorld, value: f64) {
    world
        .mount()
        .set_guide_rate_declination(value)
        .await
        .unwrap();
}

#[when(expr = "I try to set GuideRateRightAscension to {float}")]
async fn try_set_guide_rate_ra(world: &mut StarAdventurerWorld, value: f64) {
    match world.mount().set_guide_rate_right_ascension(value).await {
        Ok(()) => world.clear_error(),
        Err(e) => world.record_error(e),
    }
}

#[when(expr = "I try to set GuideRateDeclination to {float}")]
async fn try_set_guide_rate_dec(world: &mut StarAdventurerWorld, value: f64) {
    match world.mount().set_guide_rate_declination(value).await {
        Ok(()) => world.clear_error(),
        Err(e) => world.record_error(e),
    }
}

#[then("CanPulseGuide should be true")]
async fn can_pulse_guide_true(world: &mut StarAdventurerWorld) {
    assert!(world.mount().can_pulse_guide().await.unwrap());
}

#[then("CanSetGuideRates should be true")]
async fn can_set_guide_rates_true(world: &mut StarAdventurerWorld) {
    assert!(world.mount().can_set_guide_rates().await.unwrap());
}

#[then("IsPulseGuiding should be true")]
async fn is_pulse_guiding_true(world: &mut StarAdventurerWorld) {
    assert!(world.mount().is_pulse_guiding().await.unwrap());
}

#[then("IsPulseGuiding should be false")]
async fn is_pulse_guiding_false(world: &mut StarAdventurerWorld) {
    assert!(!world.mount().is_pulse_guiding().await.unwrap());
}

#[then(expr = "IsPulseGuiding should become false within {int} ms")]
async fn is_pulse_guiding_becomes_false(world: &mut StarAdventurerWorld, ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if !world.mount().is_pulse_guiding().await.unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("IsPulseGuiding did not clear within {ms} ms");
}

#[then(expr = "GuideRateRightAscension should be approximately {float} within {float}")]
async fn guide_rate_ra_approx(world: &mut StarAdventurerWorld, expected: f64, tolerance: f64) {
    let actual = world.mount().guide_rate_right_ascension().await.unwrap();
    assert!(
        (actual - expected).abs() < tolerance,
        "GuideRateRightAscension: got {actual}, expected {expected} ± {tolerance}"
    );
}

#[then(expr = "GuideRateDeclination should be approximately {float} within {float}")]
async fn guide_rate_dec_approx(world: &mut StarAdventurerWorld, expected: f64, tolerance: f64) {
    let actual = world.mount().guide_rate_declination().await.unwrap();
    assert!(
        (actual - expected).abs() < tolerance,
        "GuideRateDeclination: got {actual}, expected {expected} ± {tolerance}"
    );
}

#[then(expr = "the mount should have received exactly {int} {word} frame(s)")]
async fn frame_count(world: &mut StarAdventurerWorld, expected: usize, frame: String) {
    // Counting one exact frame across the whole post-startup log lets a
    // scenario pin how often a wire command went out without having to
    // say which call site sent each copy: `:K1` exactly once means the
    // pulse itself never stopped RA, a second `:I108CC05` means a
    // restore fired.
    let want = format!("{frame}\r");
    let log = world.command_log().await;
    let count = log.iter().filter(|c| **c == want).count();
    assert_eq!(
        count, expected,
        "expected exactly {expected} {frame} frames, saw {count} in log {log:?}"
    );
}

#[given("the mount stops the RA axis on its own")]
#[when("the mount stops the RA axis on its own")]
async fn mount_stops_ra(world: &mut StarAdventurerWorld) {
    // A stop the driver did not issue — a hand controller, a power
    // glitch — leaves `Tracking` reading true over a stopped motor.
    world.queue_seed("ra_running", false.into()).await;
}
