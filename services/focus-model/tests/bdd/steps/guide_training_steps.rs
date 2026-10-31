//! BDD step definitions for a training run on a guiding train
//! (`guide_training.feature`).
//!
//! The guiding train's camera is a stub of `phd2-guider`'s camera
//! facade: it answers the sweep-lease Actions itself, recording each,
//! and passes every other Camera call through to the simulator's camera,
//! so rp's captures under the lease take the simulator's frames.

use std::time::Duration;

use bdd_infra::rp_harness::{
    CameraConfig, FacadeCall, FacadeCameraStub, FilterWheelConfig, FocuserConfig, MountConfig,
    OpticalTrainConfig,
};
use cucumber::{given, then};

use crate::world::FocusModelWorld;

use super::focus_steps::{read_focuser_position, start_provider_then_rp};

/// The guiding train's optics: the reference train's, so the derived
/// sweep is the one the imaging scenarios walk.
const FOCAL_LENGTH_MM: f64 = 500.0;
const APERTURE_MM: f64 = 100.0;

const ACQUIRE: &str = "rustyphoton:sweep-lease-acquire";
const RELEASE: &str = "rustyphoton:sweep-lease-release";

/// How long a step waits for rp's facade release: rp sends it from the
/// lease's keeper once the lease has given everything back, a moment
/// after `release_train_lease` answered.
const FACADE_RELEASE_WAIT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Given steps
// ---------------------------------------------------------------------------

#[given("a stub camera facade over the simulator's camera")]
async fn stub_facade_over_simulator(world: &mut FocusModelWorld) {
    world.facade_stub = Some(FacadeCameraStub::start_over(&world.omnisim_url(), 0).await);
}

#[given(expr = "the stub camera facade reports PHD2 as {string}")]
fn facade_reports_phd2(world: &mut FocusModelWorld, phd2_state: String) {
    facade(world).set_phd2_state(&phd2_state);
}

#[given(
    "rp is running with a guiding train on its own focuser behind the stub camera facade and focus-model registered as a tool provider"
)]
async fn rp_with_guiding_train_behind_facade(world: &mut FocusModelWorld) {
    let camera_url = facade(world).url();
    guiding_rig(world, &camera_url, false);
    start_provider_then_rp(world).await;
}

#[given(
    "rp is running with a guiding train on its own focuser and filter wheel behind the stub camera facade and focus-model registered as a tool provider"
)]
async fn rp_with_guiding_train_and_wheel_behind_facade(world: &mut FocusModelWorld) {
    let camera_url = facade(world).url();
    guiding_rig(world, &camera_url, true);
    start_provider_then_rp(world).await;
}

#[given(
    "rp is running with a guiding train on its own focuser behind the simulator's camera and focus-model registered as a tool provider"
)]
async fn rp_with_guiding_train_behind_simulator(world: &mut FocusModelWorld) {
    let camera_url = world.omnisim_url();
    guiding_rig(world, &camera_url, false);
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

#[then(regex = r"^the stub camera facade should have been asked for (\d+) sweep leases?$")]
fn facade_asked_for(world: &mut FocusModelWorld, count: usize) {
    let acquires = calls_to(world, ACQUIRE);
    assert_eq!(acquires.len(), count, "the facade's acquires: {acquires:?}");
}

/// Every token rp named in an acquire is named in a release after it.
#[then("every sweep lease the stub camera facade was asked for should be released by its token")]
async fn every_lease_released(world: &mut FocusModelWorld) {
    let deadline = tokio::time::Instant::now() + FACADE_RELEASE_WAIT;
    loop {
        let unreleased = unreleased_tokens(world);
        if unreleased.is_empty() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no release named {unreleased:?} within {FACADE_RELEASE_WAIT:?}: {:?}",
            facade(world).calls()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[then("the stub camera facade should hold no sweep lease")]
async fn facade_holds_no_lease(world: &mut FocusModelWorld) {
    let deadline = tokio::time::Instant::now() + FACADE_RELEASE_WAIT;
    while facade(world).held_token().is_some() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let held = facade(world).held_token();
    assert!(
        held.is_none(),
        "the stub camera facade still holds {held:?} after {FACADE_RELEASE_WAIT:?}"
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const fn facade(world: &FocusModelWorld) -> &FacadeCameraStub {
    world
        .facade_stub
        .as_ref()
        .expect("no stub camera facade: add 'a stub camera facade over the simulator's camera'")
}

fn calls_to(world: &FocusModelWorld, action: &str) -> Vec<FacadeCall> {
    facade(world)
        .calls()
        .into_iter()
        .filter(|call| call.action == action)
        .collect()
}

/// The acquired tokens no later release names.
fn unreleased_tokens(world: &FocusModelWorld) -> Vec<String> {
    let calls = facade(world).calls();
    calls
        .iter()
        .enumerate()
        .filter(|(_, call)| call.action == ACQUIRE)
        .filter_map(|(at, call)| {
            let token = call.parameters["token"].as_str()?.to_string();
            let released = calls[at + 1..]
                .iter()
                .any(|later| later.action == RELEASE && later.parameters["token"] == token);
            (!released).then_some(token)
        })
        .collect()
}

/// A rig whose only train is the guiding train: the simulator's
/// focuser, the wheel when `wheel` says so, and the camera at
/// `camera_url`. Guiding is mount-scoped, so the guider block needs a
/// mount, which nothing here actuates.
fn guiding_rig(world: &mut FocusModelWorld, camera_url: &str, wheel: bool) {
    let alpaca_url = world.omnisim_url();
    world.cameras.push(CameraConfig {
        id: "guide-cam".to_string(),
        alpaca_url: camera_url.to_string(),
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
            alpaca_url,
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
        alpaca_url: "not-a-url".to_string(),
        device_number: 0,
        settle_after_slew: None,
    });
}
