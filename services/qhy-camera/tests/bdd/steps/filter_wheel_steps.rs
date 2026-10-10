//! Filter-wheel steps.

use std::time::Duration;

use cucumber::{given, then, when};

use crate::world::CameraWorld;

#[given(regex = r"^filterwheel device (\d+) is connected$")]
async fn filter_wheel_connected(world: &mut CameraWorld, _device: u32) {
    world.filter_wheel().set_connected(true).await.unwrap();
}

#[then(regex = r"^filterwheel device (\d+) reports (\d+) filter names$")]
async fn reports_filter_count(world: &mut CameraWorld, _device: u32, count: usize) {
    assert_eq!(world.filter_wheel().names().await.unwrap().len(), count);
}

#[then(
    regex = r"^filterwheel device (\d+) reports the generated names Filter0 through Filter(\d+)$"
)]
async fn reports_generated_names(world: &mut CameraWorld, _device: u32, last: u32) {
    let names = world.filter_wheel().names().await.unwrap();
    let expected: Vec<String> = (0..=last).map(|i| format!("Filter{i}")).collect();
    assert_eq!(names, expected);
}

#[when(regex = r"^I set filterwheel device (\d+) to position (\d+)$")]
async fn set_position(world: &mut CameraWorld, _device: u32, position: usize) {
    world.filter_wheel().set_position(position).await.unwrap();
}

// `And the filter wheel move ... completes` follows a `When` (When-typed); also
// registered as Then for robustness.
#[when(regex = r"^the filter wheel move on device (\d+) completes$")]
#[then(regex = r"^the filter wheel move on device (\d+) completes$")]
async fn move_completes(world: &mut CameraWorld, _device: u32) {
    // The simulated CFW settles over a few reads; poll until it is no longer "moving".
    let filter_wheel = world.filter_wheel();
    for _ in 0..40 {
        if filter_wheel.position().await.unwrap().is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("filter wheel move did not complete");
}

#[then(regex = r"^filterwheel device (\d+) reports Position as moving$")]
async fn reports_moving(world: &mut CameraWorld, _device: u32) {
    assert_eq!(world.filter_wheel().position().await.unwrap(), None);
}

#[then(regex = r"^filterwheel device (\d+) reports Position as (\d+)$")]
async fn reports_position(world: &mut CameraWorld, _device: u32, position: usize) {
    assert_eq!(
        world.filter_wheel().position().await.unwrap(),
        Some(position)
    );
}

#[when(regex = r"^I try to set filterwheel device (\d+) to position (\d+)$")]
async fn try_set_position(world: &mut CameraWorld, _device: u32, position: usize) {
    world.last_error_code = world
        .filter_wheel()
        .set_position(position)
        .await
        .err()
        .map(|e| e.code.raw());
}

#[then(regex = r"^filterwheel device (\d+) reports FocusOffsets of (\d+) zeros$")]
async fn reports_focus_offsets(world: &mut CameraWorld, _device: u32, count: usize) {
    assert_eq!(
        world.filter_wheel().focus_offsets().await.unwrap(),
        vec![0; count]
    );
}

#[then(regex = r"^filterwheel device (\d+) reports the filter names (.+)$")]
async fn reports_filter_names(world: &mut CameraWorld, _device: u32, names: String) {
    let expected: Vec<String> = names.split(", ").map(str::to_string).collect();
    assert_eq!(world.filter_wheel().names().await.unwrap(), expected);
}
