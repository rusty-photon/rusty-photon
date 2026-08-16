//! Step definitions for `follow_mode.feature` (contracts F1, F2, F5,
//! F6, F8).

use crate::world::{MountStubBehavior, RotatorStubBehavior, SkySurveyCameraWorld};
use cucumber::{given, then};
use serde_json::Value;

#[given(expr = "a mount reports RA {float} hours and Dec {float} degrees")]
async fn mount_reports(world: &mut SkySurveyCameraWorld, ra_hours: f64, dec_deg: f64) {
    world
        .spawn_mount_stub(MountStubBehavior::Ok { ra_hours, dec_deg })
        .await;
}

#[given("a mount that errors on every read")]
async fn mount_errors(world: &mut SkySurveyCameraWorld) {
    world.spawn_mount_stub(MountStubBehavior::AscomError).await;
}

#[given(expr = "a rotator reports position angle {float} degrees")]
async fn rotator_reports(world: &mut SkySurveyCameraWorld, position_angle: f64) {
    world
        .spawn_rotator_stub(RotatorStubBehavior::Ok { position_angle })
        .await;
}

#[given("a rotator that errors on every read")]
async fn rotator_errors(world: &mut SkySurveyCameraWorld) {
    world
        .spawn_rotator_stub(RotatorStubBehavior::AscomError)
        .await;
}

#[cucumber::when(expr = "the rotator is updated to position angle {float} degrees")]
fn rotator_updated(world: &mut SkySurveyCameraWorld, position_angle: f64) {
    world.set_rotator_stub_behavior(RotatorStubBehavior::Ok { position_angle });
}

#[given("the camera is configured to follow that mount")]
const fn follow_with_zero_offset(_world: &mut SkySurveyCameraWorld) {
    // `spawn_mount_stub` already set `telescope_endpoint_override`,
    // and the world's offset fields default to 0.0. This step exists
    // to make follow-mode wiring explicit in the feature file.
}

#[given(
    expr = "the camera is configured to follow that mount with offset RA {float} arcsec and Dec {float} arcsec"
)]
const fn follow_with_offset(world: &mut SkySurveyCameraWorld, ra_arcsec: f64, dec_arcsec: f64) {
    world.telescope_offset_ra_arcsec = ra_arcsec;
    world.telescope_offset_dec_arcsec = dec_arcsec;
}

#[given("the camera is started and connected in follow mode")]
async fn started_connected_follow(world: &mut SkySurveyCameraWorld) {
    world.spawn_skyview_stub_ok().await;
    let fits = crate::world::make_zero_fits(640, 480);
    world.set_stub_behavior(crate::world::StubBehavior::ServingFits(fits));
    world.start_service().await;
    world.set_camera_connected(true).await;
    if let Some(code) = world.last_ascom_error {
        panic!("expected connect to succeed in follow-mode setup, got ASCOM {code:#X}");
    }
}

#[then(
    expr = "after a successful exposure, the position endpoint reports RA approximately {float} Dec approximately {float}"
)]
async fn position_after_exposure(world: &mut SkySurveyCameraWorld, ra: f64, dec: f64) {
    drive_exposure_then_check_position(world, ra, dec).await;
}

#[cucumber::when(expr = "the mount is updated to RA {float} hours and Dec {float} degrees")]
fn mount_updated(world: &mut SkySurveyCameraWorld, ra_hours: f64, dec_deg: f64) {
    world.set_mount_stub_behavior(MountStubBehavior::Ok { ra_hours, dec_deg });
}

#[then(
    expr = "after another successful exposure, the position endpoint reports RA approximately {float} Dec approximately {float}"
)]
async fn position_after_another_exposure(world: &mut SkySurveyCameraWorld, ra: f64, dec: f64) {
    drive_exposure_then_check_position(world, ra, dec).await;
}

#[then(
    expr = "after a successful exposure, the position endpoint reports RA approximately {float} Dec approximately {float} and rotation approximately {float}"
)]
async fn position_after_exposure_with_rotation(
    world: &mut SkySurveyCameraWorld,
    ra: f64,
    dec: f64,
    rotation: f64,
) {
    drive_exposure_then_check_position_rotation(world, ra, dec, rotation).await;
}

#[then(
    expr = "after another successful exposure, the position endpoint reports RA approximately {float} Dec approximately {float} and rotation approximately {float}"
)]
async fn position_after_another_exposure_with_rotation(
    world: &mut SkySurveyCameraWorld,
    ra: f64,
    dec: f64,
    rotation: f64,
) {
    drive_exposure_then_check_position_rotation(world, ra, dec, rotation).await;
}

async fn drive_exposure_then_check_position_rotation(
    world: &mut SkySurveyCameraWorld,
    ra: f64,
    dec: f64,
    rotation: f64,
) {
    let body = drive_exposure_then_get_position(world).await;
    assert_position(&body, ra, dec);
    let actual_rotation = body["rotation"].as_f64().expect("missing rotation");
    assert!(
        (actual_rotation - rotation).abs() < 1e-4,
        "expected rotation ≈ {rotation}, got {actual_rotation}"
    );
}

async fn drive_exposure_then_check_position(world: &mut SkySurveyCameraWorld, ra: f64, dec: f64) {
    let body = drive_exposure_then_get_position(world).await;
    assert_position(&body, ra, dec);
}

/// Drive one light exposure, wait for it to complete, then GET the
/// position endpoint and return the parsed JSON body. Shared by the
/// RA/Dec-only and the RA/Dec/rotation Then steps.
async fn drive_exposure_then_get_position(world: &mut SkySurveyCameraWorld) -> Value {
    world.drive_start_exposure_default().await;
    if let Some(code) = world.last_ascom_error {
        panic!("StartExposure rejected with ASCOM {code:#X}");
    }
    if !world
        .wait_for_image_ready(std::time::Duration::from_secs(10))
        .await
    {
        panic!("image never became ready within 10s");
    }
    let url = format!("{}/sky-survey/position", world.base_url());
    let client = world.http();
    let response = client.get(&url).send().await.expect("GET position failed");
    let status = response.status();
    let body_text = response
        .text()
        .await
        .expect("failed to read GET position body");
    assert!(
        status.is_success(),
        "GET /sky-survey/position returned {status} (body: {body_text})"
    );
    serde_json::from_str(&body_text)
        .unwrap_or_else(|e| panic!("position body not JSON: {e} (body: {body_text})"))
}

fn assert_position(body: &Value, ra: f64, dec: f64) {
    let actual_ra = body["ra"].as_f64().expect("missing ra");
    let actual_dec = body["dec"].as_f64().expect("missing dec");
    assert!(
        (actual_ra - ra).abs() < 1e-4,
        "expected RA ≈ {ra}, got {actual_ra}"
    );
    assert!(
        (actual_dec - dec).abs() < 1e-4,
        "expected Dec ≈ {dec}, got {actual_dec}"
    );
}
