//! Gain, offset, and readout-mode steps.

use cucumber::{then, when};

use crate::world::CameraWorld;

#[then(regex = r"^camera device (\d+) reports GainMin not greater than GainMax$")]
async fn gain_min_le_max(world: &mut CameraWorld, _device: u32) {
    let camera = world.camera();
    assert!(camera.gain_min().await.unwrap() <= camera.gain_max().await.unwrap());
}

#[then(regex = r"^camera device (\d+) reports a Gain within GainMin and GainMax$")]
async fn gain_within(world: &mut CameraWorld, _device: u32) {
    let camera = world.camera();
    let gain = camera.gain().await.unwrap();
    let min = camera.gain_min().await.unwrap();
    let max = camera.gain_max().await.unwrap();
    assert!(min <= gain && gain <= max, "{min} <= {gain} <= {max}");
}

#[when(regex = r"^I set Gain to GainMax on camera device (\d+)$")]
async fn set_gain_to_max(world: &mut CameraWorld, _device: u32) {
    let camera = world.camera();
    let max = camera.gain_max().await.unwrap();
    camera.set_gain(max).await.unwrap();
}

#[then(regex = r"^camera device (\d+) reports Gain equal to GainMax$")]
async fn gain_equals_max(world: &mut CameraWorld, _device: u32) {
    let camera = world.camera();
    assert_eq!(
        camera.gain().await.unwrap(),
        camera.gain_max().await.unwrap()
    );
}

#[when(regex = r"^I try to set Gain to one above GainMax on camera device (\d+)$")]
async fn try_gain_above_max(world: &mut CameraWorld, _device: u32) {
    let camera = world.camera();
    let max = camera.gain_max().await.unwrap();
    world.last_error_code = camera.set_gain(max + 1).await.err().map(|e| e.code.raw());
}

#[then(regex = r"^camera device (\d+) reports OffsetMin not greater than OffsetMax$")]
async fn offset_min_le_max(world: &mut CameraWorld, _device: u32) {
    let camera = world.camera();
    assert!(camera.offset_min().await.unwrap() <= camera.offset_max().await.unwrap());
}

#[then(regex = r"^camera device (\d+) reports an Offset within OffsetMin and OffsetMax$")]
async fn offset_within(world: &mut CameraWorld, _device: u32) {
    let camera = world.camera();
    let offset = camera.offset().await.unwrap();
    let min = camera.offset_min().await.unwrap();
    let max = camera.offset_max().await.unwrap();
    assert!(min <= offset && offset <= max, "{min} <= {offset} <= {max}");
}

#[when(regex = r"^I try to set Offset to one below OffsetMin on camera device (\d+)$")]
async fn try_offset_below_min(world: &mut CameraWorld, _device: u32) {
    let camera = world.camera();
    let min = camera.offset_min().await.unwrap();
    world.last_error_code = camera.set_offset(min - 1).await.err().map(|e| e.code.raw());
}

#[then(regex = r"^camera device (\d+) reports at least one ReadoutMode$")]
async fn at_least_one_readout_mode(world: &mut CameraWorld, _device: u32) {
    assert_ne!(
        world.camera().readout_modes().await.unwrap(),
        Vec::<String>::new()
    );
}

#[then(regex = r"^camera device (\d+) reports a ReadoutMode index within the modes list$")]
async fn readout_mode_within_list(world: &mut CameraWorld, _device: u32) {
    let camera = world.camera();
    let current = camera.readout_mode().await.unwrap();
    let count = camera.readout_modes().await.unwrap().len();
    assert!(current < count, "current mode {current} not in 0..{count}");
}

#[when(regex = r"^I try to set ReadoutMode to (\d+) on camera device (\d+)$")]
async fn try_set_readout_mode(world: &mut CameraWorld, mode: usize, _device: u32) {
    world.last_error_code = world
        .camera()
        .set_readout_mode(mode)
        .await
        .err()
        .map(|e| e.code.raw());
}
