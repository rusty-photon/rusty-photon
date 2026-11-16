//! BDD step definitions for a training run on a guiding train
//! (`guide_training.feature`).
//!
//! The guiding train's camera is the simulator's camera, a roster
//! camera of rp's like any other, so rp's captures under the train
//! lease take the simulator's frames. The guider is `bdd-infra`'s stub
//! guider: the steps here name the camera its device report gives, which
//! the check rp makes before a start of guiding compares with the
//! train's, and assert on the requests it recorded. The stub guider's
//! Given and its pause assertions are in `focus_steps.rs`.

use bdd_infra::rp_harness::{
    CameraConfig, FilterWheelConfig, FocuserConfig, MountConfig, OpticalTrainConfig,
};
use cucumber::{given, then};

use crate::world::FocusModelWorld;

use super::focus_steps::{read_focuser_position, start_provider_then_rp};

/// The guiding train's optics: the reference train's, so the derived
/// sweep is the one the imaging scenarios walk.
const FOCAL_LENGTH_MM: f64 = 500.0;
const APERTURE_MM: f64 = 100.0;

// ---------------------------------------------------------------------------
// Given steps
// ---------------------------------------------------------------------------

/// The simulator's camera 0 is the guiding train's camera.
#[given("the stub guider's camera is the simulator's camera")]
async fn stub_camera_is_simulators(world: &mut FocusModelWorld) {
    let camera_id = world.simulator_unique_id("Camera").await;
    world.guider().set_camera_unique_id(Some(&camera_id));
}

#[given(
    "rp is running with a guiding train on its own focuser behind the simulator's camera and focus-model registered as a tool provider"
)]
async fn rp_with_guiding_train(world: &mut FocusModelWorld) {
    guiding_rig(world, false);
    start_provider_then_rp(world).await;
}

#[given(
    "rp is running with a guiding train on its own focuser and filter wheel behind the simulator's camera and focus-model registered as a tool provider"
)]
async fn rp_with_guiding_train_and_wheel(world: &mut FocusModelWorld) {
    guiding_rig(world, true);
    start_provider_then_rp(world).await;
}

// ---------------------------------------------------------------------------
// Then steps
// ---------------------------------------------------------------------------

#[then(expr = "the focuser should still be at position {int}")]
async fn focuser_still_at(world: &mut FocusModelWorld, expected: i64) {
    let position = read_focuser_position(world).await;
    assert_eq!(position, expected, "the focuser moved");
}

/// The guider was read, and every request it received was a read of
/// its state: rp's guide camera guard reads `guiding/stats` as a
/// guiding train's lease is acquired and before each capture through
/// the guide camera, and the provider's handshake reads it through rp.
/// Nothing paused, started or stopped the guider.
#[then("the stub guider should have received state reads and no other request")]
fn stub_received_only_state_reads(world: &mut FocusModelWorld) {
    let requests = world.guider().requests();
    assert!(
        !requests.is_empty(),
        "the stub guider received no request: nothing read its state"
    );
    let others: Vec<_> = requests
        .iter()
        .filter(|request| request.method != "GET" || request.path != "/guiding/stats")
        .collect();
    assert!(
        others.is_empty(),
        "expected only reads of the guider's state, got {others:?}; all: {requests:?}"
    );
}

/// Not even a read: rp reads the guider's state as it acquires a
/// guiding train's lease, so no lease was asked for.
#[then("the stub guider should have received no request")]
fn stub_received_no_request(world: &mut FocusModelWorld) {
    let requests = world.guider().requests();
    assert!(requests.is_empty(), "the stub guider received {requests:?}");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A rig whose only train is the guiding train: the simulator's
/// camera and focuser, and its wheel when `wheel` says so. Guiding is
/// mount-scoped, so the guider block needs a mount: the simulator's,
/// which the stub guider's device report names and nothing here
/// actuates.
fn guiding_rig(world: &mut FocusModelWorld, wheel: bool) {
    let alpaca_url = world.omnisim_url();
    world.cameras.push(CameraConfig {
        id: "guide-cam".to_string(),
        alpaca_url: alpaca_url.clone(),
        device_number: 0,
        cooler_targets_c: Vec::new(),
    });
    world.focusers.push(FocuserConfig {
        id: "main-focuser".to_string(),
        alpaca_url: alpaca_url.clone(),
        device_number: 0,
        min_position: None,
        max_position: None,
        backlash: None,
        microns_per_step: world.focuser_microns_per_step,
    });
    let mut devices = vec!["main-focuser".to_string(), "guide-cam".to_string()];
    if wheel {
        world.filter_wheels.push(FilterWheelConfig {
            id: "main-fw".to_string(),
            alpaca_url: alpaca_url.clone(),
            device_number: 0,
            filters: vec!["Luminance".to_string(), "Ha".to_string()],
            wavelengths_nm: world.filter_wavelengths_nm.clone(),
        });
        devices.insert(0, "main-fw".to_string());
    }
    world.optical_trains.push(OpticalTrainConfig {
        telescope: None,
        aperture_mm: Some(APERTURE_MM),
        obstruction_mm: None,
        id: "guide".to_string(),
        purpose: Some("guiding".to_string()),
        focal_length_mm: Some(FOCAL_LENGTH_MM),
        default_position_angle_degrees: None,
        devices,
        auto_focus: None,
    });
    world.mount = Some(MountConfig {
        alpaca_url,
        device_number: 0,
        settle_after_slew: None,
    });
}
