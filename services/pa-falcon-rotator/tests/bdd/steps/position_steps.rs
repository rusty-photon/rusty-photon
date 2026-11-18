//! Step definitions for `position_reads.feature`

use crate::world::FalconRotatorWorld;
use cucumber::{given, then, when};

// `the rotator reports mechanical position …` and `the driver-side sync
// offset is …` appear as `And` under `When` in every scenario, so they
// must be registered as both Given and When — Gherkin resolves `And` to
// the keyword of the preceding step, which is `When` here.

#[given(expr = "the rotator reports mechanical position {float} degrees")]
#[when(expr = "the rotator reports mechanical position {float} degrees")]
async fn given_mechanical(world: &mut FalconRotatorWorld, degrees: f64) {
    world.mock().set_mech_position_deg(degrees).await;
}

#[given(expr = "the driver-side sync offset is {float} degrees")]
#[when(expr = "the driver-side sync offset is {float} degrees")]
async fn given_sync_offset(world: &mut FalconRotatorWorld, degrees: f64) {
    // `FalconManager::sync` stores `(sky - mech) mod 360`. Driving it via
    // the public Alpaca `sync(target)` keeps the test honest about how the
    // offset actually gets set in production — there's no test-only setter.
    //
    // Read the current mech position over Alpaca and call sync at
    // (mech + degrees). The resulting offset is exactly `degrees mod 360`.
    let mech_now = world.rotator().mechanical_position().await.unwrap();
    world
        .rotator()
        .sync(mech_now + degrees)
        .await
        .expect("sync to set the offset should succeed");
}

#[when("I read MechanicalPosition")]
async fn read_mechanical_position(world: &mut FalconRotatorWorld) {
    match world.rotator().mechanical_position().await {
        Ok(v) => {
            world.mechanical_position_result = Some(v);
            world.last_error_code = None;
        }
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[when("I read Position")]
async fn read_position(world: &mut FalconRotatorWorld) {
    match world.rotator().position().await {
        Ok(v) => {
            world.position_result = Some(v);
            world.last_error_code = None;
        }
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[when("I read TargetPosition")]
async fn read_target_position(world: &mut FalconRotatorWorld) {
    match world.rotator().target_position().await {
        Ok(v) => {
            world.target_position_result = Some(v);
            world.last_error_code = None;
        }
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[then(expr = "MechanicalPosition should be {float} degrees")]
async fn mechanical_position_should_be(world: &mut FalconRotatorWorld, expected: f64) {
    let actual = world
        .mechanical_position_result
        .expect("no MechanicalPosition captured");
    assert!(
        (actual - expected).abs() < 1e-6,
        "expected {expected}°, got {actual}°"
    );
}

#[then(expr = "Position should be {float} degrees")]
async fn position_should_be(world: &mut FalconRotatorWorld, expected: f64) {
    let actual = world.position_result.expect("no Position captured");
    assert!(
        (actual - expected).abs() < 1e-6,
        "expected {expected}°, got {actual}°"
    );
}

#[then(expr = "TargetPosition should be {float} degrees")]
async fn target_position_should_be(world: &mut FalconRotatorWorld, expected: f64) {
    let actual = world
        .target_position_result
        .expect("no TargetPosition captured");
    assert!(
        (actual - expected).abs() < 1e-6,
        "expected {expected}°, got {actual}°"
    );
}

#[then("an FA command should have been issued")]
async fn fa_command_issued(world: &mut FalconRotatorWorld) {
    let log = world.mock().command_log().await;
    assert!(
        log.iter().any(|c| c == "FA"),
        "no FA in command log: {log:?}"
    );
}
