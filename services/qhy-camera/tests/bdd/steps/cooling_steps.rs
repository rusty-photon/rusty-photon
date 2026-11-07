//! Cooling steps, and the sensor reading the suite sets on the simulated
//! camera (K2).

use cucumber::{given, then, when};

use crate::world::CameraWorld;

#[given("the qhy-camera service running with a simulated camera whose temperature reading the suite can set")]
async fn service_with_a_settable_reading(world: &mut CameraWorld) {
    world.temperature_file = Some(world.scratch_dir().join("temperature"));
    world.start().await;
}

/// Writes the reading verbatim, so a scenario can hand the sensor a number
/// no camera can report, `NaN` included.
#[when(regex = r"^the sensor of camera device (\d+) reads (\S+)$")]
async fn sensor_reads(world: &mut CameraWorld, _device: u32, reading: String) {
    let file = world
        .temperature_file
        .as_ref()
        .expect("the service was not started with a temperature file");
    std::fs::write(file, reading).expect("write the temperature file");
}

#[then(regex = r"^camera device (\d+) reports a finite CCDTemperature$")]
async fn finite_ccd_temperature(world: &mut CameraWorld, _device: u32) {
    assert!(world.camera().ccd_temperature().await.unwrap().is_finite());
}

#[then(regex = r"^camera device (\d+) reports CCDTemperature as (-?[0-9.]+)$")]
async fn reports_ccd_temperature(world: &mut CameraWorld, _device: u32, expected: f64) {
    let actual = world.camera().ccd_temperature().await.unwrap();
    assert!((actual - expected).abs() < 1e-6, "{actual} != {expected}");
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

#[when(regex = r"^I try to turn the cooler on for camera device (\d+)$")]
async fn try_turn_cooler_on(world: &mut CameraWorld, _device: u32) {
    world.last_error_code = world
        .camera()
        .set_cooler_on(true)
        .await
        .err()
        .map(|e| e.code.raw());
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
