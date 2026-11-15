//! BDD step definitions for a training run on a guiding train
//! (`guide_training.feature`).
//!
//! The guiding train's camera is the simulator's camera, a roster
//! camera of rp's like any other, so rp's captures under the train
//! lease take the simulator's frames. The guider is `bdd-infra`'s stub
//! guider: the steps here name the camera its device report gives and
//! assert on the camera-lease calls it recorded. The stub guider's Given
//! and its pause assertions are in `focus_steps.rs`.

use std::time::Duration;

use bdd_infra::rp_harness::{
    CameraConfig, FilterWheelConfig, FocuserConfig, LeaseCall, MountConfig, OpticalTrainConfig,
};
use cucumber::{given, then};

use crate::world::FocusModelWorld;

use super::focus_steps::{read_focuser_position, start_provider_then_rp};

/// The guiding train's optics: the reference train's, so the derived
/// sweep is the one the imaging scenarios walk.
const FOCAL_LENGTH_MM: f64 = 500.0;
const APERTURE_MM: f64 = 100.0;

/// How long a step waits for rp to give the guider's camera lease back:
/// rp releases it once the train lease has given everything back, a
/// moment after `release_train_lease` answered.
const CAMERA_LEASE_RELEASE_WAIT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Given steps
// ---------------------------------------------------------------------------

/// The simulator's camera 0 is the guiding train's camera.
#[given("the stub guider's camera is the simulator's camera")]
async fn stub_camera_is_simulators(world: &mut FocusModelWorld) {
    let camera_id = world.simulator_unique_id("Camera").await;
    world.guider().set_camera_unique_id(Some(&camera_id));
}

#[given(expr = "the stub guider reports the camera unique_id {string}")]
fn stub_reports_camera_unique_id(world: &mut FocusModelWorld, unique_id: String) {
    world.guider().set_camera_unique_id(Some(&unique_id));
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

#[then("the error message should name the simulator camera's unique id")]
async fn error_names_simulator_camera(world: &mut FocusModelWorld) {
    let unique_id = world.simulator_unique_id("Camera").await;
    let message = world
        .last_result()
        .as_ref()
        .expect_err("the tool call succeeded, so there is no error message");
    assert!(
        message.contains(&unique_id),
        "error message {message:?} does not name the simulator camera's unique id {unique_id:?}"
    );
}

/// Every `POST /camera/lease` counts, granted or refused.
#[then(expr = "the stub guider should have been asked for {int} camera lease(s)")]
fn stub_asked_for_camera_leases(world: &mut FocusModelWorld, count: usize) {
    let acquires = world.guider().requests_to("/camera/lease");
    assert_eq!(
        acquires.len(),
        count,
        "the stub guider's camera lease acquires: {acquires:?}"
    );
}

/// Every token rp named in an acquire is named in a release after it.
#[then("every camera lease the stub guider was asked for should be released by its token")]
async fn every_camera_lease_released(world: &mut FocusModelWorld) {
    let deadline = tokio::time::Instant::now() + CAMERA_LEASE_RELEASE_WAIT;
    loop {
        let unreleased = unreleased_tokens(&world.guider().lease_calls());
        if unreleased.is_empty() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no release named {unreleased:?} within {CAMERA_LEASE_RELEASE_WAIT:?}; calls: {:?}",
            world.guider().lease_calls()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[then("the stub guider should hold no camera lease")]
async fn stub_holds_no_camera_lease(world: &mut FocusModelWorld) {
    let deadline = tokio::time::Instant::now() + CAMERA_LEASE_RELEASE_WAIT;
    while let Some(lease) = world.guider().lease() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the stub guider still holds a camera lease under {:?} after \
             {CAMERA_LEASE_RELEASE_WAIT:?}; calls: {:?}",
            lease.token,
            world.guider().lease_calls()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The acquired tokens no later release names.
fn unreleased_tokens(calls: &[LeaseCall]) -> Vec<String> {
    calls
        .iter()
        .enumerate()
        .filter_map(|(at, call)| match call {
            LeaseCall::Acquire { token, .. } => {
                let released = calls[at + 1..].iter().any(
                    |later| matches!(later, LeaseCall::Release { token: t, .. } if t == token),
                );
                (!released).then(|| token.clone())
            }
            _ => None,
        })
        .collect()
}

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
