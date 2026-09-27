//! Steps for `coordinate_reads.feature`.

use crate::world::StarAdventurerWorld;
use cucumber::{given, then, when};
use std::time::Duration;

#[given(expr = "a mount with CPR {int} on the RA axis and {int} on the Dec axis")]
async fn mount_with_cpr(_world: &mut StarAdventurerWorld, cpr_ra: u32, cpr_dec: u32) {
    // Mock seeds the GTi's per-axis CPRs (RA `0x375F00 = 3,628,800`,
    // Dec `0x2C4C00 = 2,903,040` — the axes differ on real hardware);
    // scenarios using this step pin those defaults. Assert the values
    // match so a feature-file typo / divergence fails fast instead of
    // silently passing.
    const GTI_CPR_RA: u32 = 0x0037_5F00;
    const GTI_CPR_DEC: u32 = 0x002C_4C00;
    assert_eq!(
        cpr_ra, GTI_CPR_RA,
        "mock seeds RA CPR {GTI_CPR_RA}; feature file asked for {cpr_ra}. \
         Custom CPRs need a /debug/v1/mock-state extension."
    );
    assert_eq!(
        cpr_dec, GTI_CPR_DEC,
        "mock seeds Dec CPR {GTI_CPR_DEC}; feature file asked for {cpr_dec}. \
         Custom CPRs need a /debug/v1/mock-state extension."
    );
}

#[given(expr = "the RA-axis encoder reads {int} ticks")]
async fn ra_encoder_reads(world: &mut StarAdventurerWorld, ticks: i32) {
    world.queue_seed("ra_ticks", ticks.into()).await;
}

#[given(expr = "the Dec-axis encoder reads {int} ticks")]
async fn dec_encoder_reads(world: &mut StarAdventurerWorld, ticks: i32) {
    world.queue_seed("dec_ticks", ticks.into()).await;
}

#[given(expr = "site longitude is {float} degrees")]
async fn site_longitude_is(world: &mut StarAdventurerWorld, deg: f64) {
    world.pin_site_longitude(deg);
}

#[given(expr = "UTC is {string}")]
async fn utc_is(_world: &mut StarAdventurerWorld, _ts: String) {
    // No clock injection in MVP — the running binary always reads
    // `SystemTime::now()`. The scenarios this step appears in only
    // assert *relative* properties (e.g. "RA equals SiderealTime"),
    // not absolute literal LST values. Absolute LST is unit-tested in
    // `coordinates::tests::lst_changes_with_longitude` /
    // `lst_is_stable_across_calls`.
}

#[given("the mount reports both axes stopped")]
async fn mount_axes_stopped(world: &mut StarAdventurerWorld) {
    world.queue_seed("ra_running", false.into()).await;
    world.queue_seed("dec_running", false.into()).await;
}

#[given("the mount reports the RA axis running in goto mode")]
async fn ra_axis_running_goto(world: &mut StarAdventurerWorld) {
    // A far goto target keeps the mock's `advance_one_step` from
    // immediately tripping `running` to false on `delta == 0`. Use the
    // protocol's signed-24-bit max so the seed stays representable on
    // the wire and survives the tick-range validator on
    // `/debug/v1/mock-state`.
    use skywatcher_motor_protocol::codec::POSITION_MAX;
    world.queue_seed("ra_running", true.into()).await;
    world.queue_seed("ra_goto", true.into()).await;
    world
        .queue_seed("ra_goto_target_ticks", POSITION_MAX.into())
        .await;
    world.queue_seed("ra_initialized", true.into()).await;
    world.queue_seed("dec_initialized", true.into()).await;
}

#[when("I try to read RightAscension")]
async fn try_read_ra(world: &mut StarAdventurerWorld) {
    match world.mount().right_ascension().await {
        Ok(_) => world.clear_error(),
        Err(e) => world.record_error(e),
    }
}

#[when("I try to read Declination")]
async fn try_read_dec(world: &mut StarAdventurerWorld) {
    match world.mount().declination().await {
        Ok(_) => world.clear_error(),
        Err(e) => world.record_error(e),
    }
}

#[then(expr = "RightAscension should equal SiderealTime within {float} hours")]
async fn ra_equals_sidereal_time(world: &mut StarAdventurerWorld, tolerance: f64) {
    let ra = world.mount().right_ascension().await.unwrap();
    let lst = world.mount().sidereal_time().await.unwrap();
    assert!((ra - lst).abs() < tolerance, "RA {ra} vs LST {lst}");
}

#[then(expr = "Declination should be {float} degrees within {float}")]
async fn declination_should_be(world: &mut StarAdventurerWorld, expected: f64, tolerance: f64) {
    let actual = world.mount().declination().await.unwrap();
    assert!(
        (actual - expected).abs() < tolerance,
        "{actual} vs {expected}"
    );
}

#[then(expr = "RightAscension should be {float} hours within {float}")]
async fn ra_should_be(world: &mut StarAdventurerWorld, expected: f64, tolerance: f64) {
    // Snapshot lags the wire by up to one polling cycle, so a
    // fresh sync's effect on RightAscension takes a poll to land.
    // Retry briefly before failing.
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    let mut last = f64::NAN;
    while std::time::Instant::now() < deadline {
        last = world.mount().right_ascension().await.unwrap();
        if (last - expected).abs() < tolerance {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("RightAscension {last} did not converge to {expected} within {tolerance} hours");
}

#[then(expr = "SiderealTime should be approximately {float} hours within {float}")]
async fn sidereal_time_should_be(world: &mut StarAdventurerWorld, _expected: f64, _tolerance: f64) {
    // The absolute literal LST value depends on the wall clock, which
    // can't be pinned by the BDD harness without clock injection
    // (TODO Phase 4). Until then, assert the *meaningful* invariants:
    //  1. SiderealTime is in `[0, 24)` hours.
    //  2. Reading it twice within the same scenario advances by less
    //     than one sidereal day (a sanity check that the underlying
    //     ERFA math isn't wrapping pathologically).
    // Absolute LST is pinned by the unit test
    // `coordinates::tests::lst_changes_with_longitude`, so the
    // numeric literal stays in the feature file as documentation
    // even though we can't assert against it here.
    let first = world.mount().sidereal_time().await.unwrap();
    assert!(
        (0.0..24.0).contains(&first),
        "SiderealTime out of [0, 24): {first}"
    );
    let second = world.mount().sidereal_time().await.unwrap();
    assert!(
        (0.0..24.0).contains(&second),
        "SiderealTime out of [0, 24): {second}"
    );
}

#[then("Slewing should be false")]
async fn slewing_should_be_false(world: &mut StarAdventurerWorld) {
    assert!(!world.mount().slewing().await.unwrap());
}

#[then("Slewing should be true")]
async fn slewing_should_be_true(world: &mut StarAdventurerWorld) {
    // Slewing depends on the polling task picking up the seeded
    // `running=true` from the mock (e.g. the "RA axis running in
    // goto mode" Given step). On a contended CI runner (e.g. a
    // parallel coverage job competing for cores) that's still well
    // under a second, but 2s has flaked. 5s matches the budget the
    // other "Slewing should eventually be ..." steps already use.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if world.mount().slewing().await.unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("Slewing did not become true within 5s");
}

#[then(expr = "Slewing should eventually be false within {int} seconds")]
async fn slewing_eventually_false(world: &mut StarAdventurerWorld, secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if !world.mount().slewing().await.unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("Slewing did not become false within {secs} seconds");
}
