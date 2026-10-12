//! A camera that leaves the bus (C6): starting the service over a simulated
//! camera that can, taking the camera away and giving it back, and the
//! reconnect a client makes once it has gone.

use cucumber::{given, then, when};

use crate::world::CameraWorld;

#[given("the svbony-camera service running with a simulated camera that can leave the bus")]
async fn service_with_a_departing_camera(world: &mut CameraWorld) {
    world.departure_file = Some(world.scratch_dir().join("departed"));
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

#[when(regex = r"^I try to connect camera device (\d+)$")]
async fn try_connect_camera(world: &mut CameraWorld, device: u32) {
    let error = world.device(device).set_connected(true).await.err();
    world.last_error_code = error.as_ref().map(|e| e.code.raw());
    world.last_error_message = error.map(|e| e.message.to_string());
}

/// A departed camera is found out by its exposure's read timing out at its
/// deadline, in the background, so the scenario waits for the effect rather
/// than sampling once (testing.md §6.9).
#[then(regex = r"^camera device (\d+) eventually reports Connected as false$")]
async fn eventually_disconnected(world: &mut CameraWorld, _device: u32) {
    world.wait_disconnected().await;
}
