//! Binning and ROI steps.

use cucumber::{then, when};

use crate::world::CameraWorld;

#[when(regex = r"^I set BinX (\d+) and BinY (\d+) on camera device (\d+)$")]
async fn set_bin(world: &mut CameraWorld, bin_x: u8, bin_y: u8, _device: u32) {
    let camera = world.camera();
    camera.set_bin_x(bin_x).await.unwrap();
    camera.set_bin_y(bin_y).await.unwrap();
}

#[when(regex = r"^I try to set BinX (\d+) and BinY (\d+) on camera device (\d+)$")]
async fn try_set_bin(world: &mut CameraWorld, bin_x: u8, bin_y: u8, _device: u32) {
    let camera = world.camera();
    let result = match camera.set_bin_x(bin_x).await {
        Ok(()) => camera.set_bin_y(bin_y).await,
        Err(e) => Err(e),
    };
    world.last_error_code = result.err().map(|e| e.code.raw());
}

#[then(regex = r"^camera device (\d+) reports BinX as (\d+) and BinY as (\d+)$")]
async fn reports_bin(world: &mut CameraWorld, _device: u32, bin_x: u8, bin_y: u8) {
    let camera = world.camera();
    assert_eq!(camera.bin_x().await.unwrap(), bin_x);
    assert_eq!(camera.bin_y().await.unwrap(), bin_y);
}

#[when(regex = r"^I set StartX (\d+) NumX (\d+) StartY (\d+) NumY (\d+) on camera device (\d+)$")]
async fn set_roi(
    world: &mut CameraWorld,
    start_x: u32,
    num_x: u32,
    start_y: u32,
    num_y: u32,
    _device: u32,
) {
    let camera = world.camera();
    camera.set_start_x(start_x).await.unwrap();
    camera.set_num_x(num_x).await.unwrap();
    camera.set_start_y(start_y).await.unwrap();
    camera.set_num_y(num_y).await.unwrap();
}

#[then(regex = r"^camera device (\d+) reports StartX (\d+) NumX (\d+) StartY (\d+) NumY (\d+)$")]
async fn reports_roi(
    world: &mut CameraWorld,
    _device: u32,
    start_x: u32,
    num_x: u32,
    start_y: u32,
    num_y: u32,
) {
    let camera = world.camera();
    assert_eq!(camera.start_x().await.unwrap(), start_x, "StartX");
    assert_eq!(camera.num_x().await.unwrap(), num_x, "NumX");
    assert_eq!(camera.start_y().await.unwrap(), start_y, "StartY");
    assert_eq!(camera.num_y().await.unwrap(), num_y, "NumY");
}

#[then(regex = r"^camera device (\d+) accepts the ROI without error$")]
async fn roi_accepted(world: &mut CameraWorld, _device: u32) {
    // The relaxed setters succeeded in the When step; confirm they read back.
    let camera = world.camera();
    camera.num_x().await.unwrap();
    camera.num_y().await.unwrap();
}
