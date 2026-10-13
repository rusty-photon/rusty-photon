//! A camera that leaves the bus (C9): starting the service over a simulated
//! camera that can, taking the camera away and giving it back, and the
//! reconnect a client makes once it has gone.

use cucumber::{given, then, when};

use crate::world::CameraWorld;

#[given("the qhy-camera service running with a simulated camera that can leave the bus")]
async fn service_with_a_departing_camera(world: &mut CameraWorld) {
    world.departure_file = Some(world.scratch_dir().join("departed"));
    world.start().await;
}

#[given("the qhy-camera service running with a simulated camera that cannot be opened")]
async fn service_with_a_camera_that_cannot_be_opened(world: &mut CameraWorld) {
    // A camera already off the bus when the service starts: the scan lists it,
    // and the CFW probe's open fails, as on a wedged camera or one unplugged
    // between the scan and the probe.
    let departure = world.scratch_dir().join("departed");
    std::fs::write(&departure, b"").expect("write the departure file");
    world.departure_file = Some(departure);
    world.expect_no_camera = true;
    world.start().await;
}

#[when(regex = r"^camera device (\d+) leaves the bus$")]
async fn camera_leaves_the_bus(world: &mut CameraWorld, _device: u32) {
    let departure = world
        .departure_file
        .as_ref()
        .expect("the service was not started with a departure file");
    std::fs::write(departure, b"").expect("write the departure file");
}

#[when(regex = r"^camera device (\d+) returns to the bus$")]
async fn camera_returns_to_the_bus(world: &mut CameraWorld, _device: u32) {
    let departure = world
        .departure_file
        .as_ref()
        .expect("the service was not started with a departure file");
    std::fs::remove_file(departure).expect("remove the departure file");
}

/// Stashes the rejection's code and message alike: a placeholder's reason is
/// in its message (U4).
#[when(regex = r"^I try to connect camera device (\d+)$")]
async fn try_connect_camera(world: &mut CameraWorld, device: u32) {
    let error = world.device(device).set_connected(true).await.err();
    world.last_error_code = error.as_ref().map(|e| e.code.raw());
    world.last_error_message = error.map(|e| e.message.to_string());
}

#[then(regex = r"^filterwheel device (\d+) reports Connected as (true|false)$")]
async fn filter_wheel_reports_connected(world: &mut CameraWorld, device: u32, expected: bool) {
    let actual = world.wheel(device).connected().await.unwrap();
    assert_eq!(
        actual, expected,
        "filter wheel Connected expected {expected}, got {actual}"
    );
}
