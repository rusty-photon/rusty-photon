//! Step definitions for `exposure_validation.feature` (contracts E1-E6)
//! and the supporting Givens used by `exposure_survey.feature` and
//! `cancellation.feature`.

use crate::world::SkySurveyCameraWorld;
use cucumber::{given, then, when};

#[given("the camera is connected with the survey backend stubbed")]
async fn connected_stubbed(world: &mut SkySurveyCameraWorld) {
    world.spawn_skyview_stub_ok().await;
    world.start_service().await;
    world.set_camera_connected(true).await;
    if let Some(code) = world.last_ascom_error {
        panic!("expected connect to succeed, got ASCOM {code:#X}");
    }
}

#[given("an exposure is already in flight")]
async fn exposure_in_flight(world: &mut SkySurveyCameraWorld) {
    // Set the stub to Hold so the spawned fetch hangs indefinitely;
    // that keeps `exposure_in_flight` true for E2 / A1.
    world.set_stub_behavior(crate::world::StubBehavior::Hold);
    world.drive_start_exposure(1, 1, 100, 100, 0, 0, 1.0).await;
    if let Some(code) = world.last_ascom_error {
        panic!("expected initial StartExposure to succeed, got ASCOM {code:#X}");
    }
}

#[given("an exposure has completed")]
async fn exposure_completed(world: &mut SkySurveyCameraWorld) {
    // The opposite setup to the Given above: let the fetch finish, so the
    // camera is idle with a frame still readable. A3 asserts a cancel
    // issued here leaves that frame alone.
    world.set_stub_behavior(crate::world::StubBehavior::ServingFits(
        crate::world::make_zero_fits(640, 480),
    ));
    world.drive_start_exposure_default().await;
    if let Some(code) = world.last_ascom_error {
        panic!("expected StartExposure to succeed, got ASCOM {code:#X}");
    }
    assert!(
        world
            .wait_for_image_ready(std::time::Duration::from_secs(10))
            .await,
        "image never became ready within 10s"
    );
}

#[when(
    expr = "I StartExposure with BinX {int} BinY {int} NumX {int} NumY {int} StartX {int} StartY {int} Duration {float}"
)]
#[allow(clippy::too_many_arguments)]
async fn start_exposure(
    world: &mut SkySurveyCameraWorld,
    bin_x: i32,
    bin_y: i32,
    num_x: i32,
    num_y: i32,
    start_x: i32,
    start_y: i32,
    duration_s: f64,
) {
    world
        .drive_start_exposure(bin_x, bin_y, num_x, num_y, start_x, start_y, duration_s)
        .await;
}

#[then("the exposure is rejected with ASCOM INVALID_VALUE")]
fn rejected_invalid_value(world: &mut SkySurveyCameraWorld) {
    assert_ascom_error(world, 0x401);
}

#[then("the exposure is rejected with ASCOM INVALID_OPERATION")]
fn rejected_invalid_operation(world: &mut SkySurveyCameraWorld) {
    assert_ascom_error(world, 0x40B);
}

#[then("the exposure is rejected with ASCOM NOT_CONNECTED")]
fn rejected_not_connected(world: &mut SkySurveyCameraWorld) {
    assert_ascom_error(world, 0x407);
}

fn assert_ascom_error(world: &SkySurveyCameraWorld, expected: u32) {
    let actual = world
        .last_ascom_error
        .expect("no ASCOM error captured — did the When step run?");
    assert_eq!(
        actual, expected,
        "expected ASCOM error {expected:#X}, got {actual:#X} (body: {:?})",
        world.last_http_body
    );
}
