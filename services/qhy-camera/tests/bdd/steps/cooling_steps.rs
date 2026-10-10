//! Cooling steps.

use cucumber::{then, when};

use crate::world::CameraWorld;

#[then(regex = r"^camera device (\d+) reports a finite CCDTemperature$")]
async fn finite_ccd_temperature(world: &mut CameraWorld, _device: u32) {
    assert!(world.camera().ccd_temperature().await.unwrap().is_finite());
}

#[when(regex = r"^I set the target CCD temperature to (-?[0-9.]+) on camera device (\d+)$")]
async fn set_target_temperature(world: &mut CameraWorld, target: f64, _device: u32) {
    world
        .camera()
        .set_set_ccd_temperature(target)
        .await
        .unwrap();
}

#[then(regex = r"^camera device (\d+) reports SetCCDTemperature as (-?[0-9.]+)$")]
async fn reports_set_ccd_temperature(world: &mut CameraWorld, _device: u32, expected: f64) {
    let actual = world.camera().set_ccd_temperature().await.unwrap();
    assert!((actual - expected).abs() < 1e-6, "{actual} != {expected}");
}

#[when(regex = r"^I try to set the target CCD temperature to (-?[0-9.]+) on camera device (\d+)$")]
async fn try_set_target_temperature(world: &mut CameraWorld, target: f64, _device: u32) {
    world.last_error_code = world
        .camera()
        .set_set_ccd_temperature(target)
        .await
        .err()
        .map(|e| e.code.raw());
}

#[when(regex = r"^I turn the cooler on for camera device (\d+)$")]
async fn turn_cooler_on(world: &mut CameraWorld, _device: u32) {
    world.camera().set_cooler_on(true).await.unwrap();
}

/// The simulated cooler moves one step per `CCDTemperature` read, so the
/// sensor is read until the TEC reports a drive — bounded, so a cooler that
/// never engages fails here rather than hanging the scenario.
#[when(regex = r"^I wait for the cooler on camera device (\d+) to draw power$")]
async fn wait_for_cooler_power(world: &mut CameraWorld, _device: u32) {
    for _ in 0..50 {
        world.camera().ccd_temperature().await.unwrap();
        if world.camera().cooler_power().await.unwrap() > 0.0 {
            return;
        }
    }
    panic!("the cooler never drew power");
}

#[then(regex = r"^camera device (\d+) reports a CoolerPower between 0 and 100$")]
async fn cooler_power_in_range(world: &mut CameraWorld, _device: u32) {
    let power = world.camera().cooler_power().await.unwrap();
    assert!(
        (0.0..=100.0).contains(&power),
        "CoolerPower {power} out of [0, 100]"
    );
}
