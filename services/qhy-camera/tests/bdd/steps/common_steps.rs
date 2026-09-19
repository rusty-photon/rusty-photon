//! Shared steps: service startup, connection lifecycle, boolean property
//! reports, and the generic rejection assertions.

use cucumber::gherkin::Step;
use cucumber::{given, then, when};

use crate::world::{ascom_code, CameraWorld};

// --- service startup --------------------------------------------------------

#[given("the qhy-camera service running with the simulation backend")]
#[given("a running qhy-camera service with the simulation backend")]
async fn service_running(world: &mut CameraWorld) {
    world.start().await;
}

#[given("the qhy-camera service running with an empty simulation backend")]
async fn service_running_empty(world: &mut CameraWorld) {
    world.empty_backend = true;
    world.start().await;
}

#[given(
    regex = r"^the qhy-camera service running with the simulation backend and filter names (.+)$"
)]
async fn service_running_with_filter_names(world: &mut CameraWorld, names: String) {
    world.filter_names = Some(names.split(", ").map(str::to_string).collect());
    world.start().await;
}

// --- connection lifecycle ---------------------------------------------------

#[given(regex = r"^camera device (\d+) is connected$")]
async fn camera_is_connected(world: &mut CameraWorld, _device: u32) {
    world.camera().set_connected(true).await.unwrap();
}

#[given(regex = r"^camera device (\d+) is not connected$")]
async fn camera_is_not_connected(world: &mut CameraWorld, _device: u32) {
    world.camera().set_connected(false).await.unwrap();
}

#[when(regex = r"^I connect camera device (\d+)$")]
async fn connect_camera(world: &mut CameraWorld, _device: u32) {
    world.camera().set_connected(true).await.unwrap();
}

#[when(regex = r"^I disconnect camera device (\d+)$")]
async fn disconnect_camera(world: &mut CameraWorld, _device: u32) {
    world.camera().set_connected(false).await.unwrap();
}

#[given(regex = r"^an exposure is in flight on camera device (\d+)$")]
async fn exposure_in_flight(world: &mut CameraWorld, _device: u32) {
    world.start_in_flight().await;
}

// --- boolean property reports (used as Given precondition and Then check) ----

#[given(regex = r"^camera device (\d+) reports (\w+) as (true|false)$")]
#[then(regex = r"^camera device (\d+) reports (\w+) as (true|false)$")]
async fn camera_reports_bool(
    world: &mut CameraWorld,
    _device: u32,
    property: String,
    expected: bool,
) {
    let camera = world.camera();
    let actual = match property.as_str() {
        "Connected" => camera.connected().await.unwrap(),
        "ImageReady" => camera.image_ready().await.unwrap(),
        "CanAsymmetricBin" => camera.can_asymmetric_bin().await.unwrap(),
        "HasShutter" => camera.has_shutter().await.unwrap(),
        "CanAbortExposure" => camera.can_abort_exposure().await.unwrap(),
        "CanStopExposure" => camera.can_stop_exposure().await.unwrap(),
        "CanPulseGuide" => camera.can_pulse_guide().await.unwrap(),
        "CoolerOn" => camera.cooler_on().await.unwrap(),
        "CanSetCCDTemperature" => camera.can_set_ccd_temperature().await.unwrap(),
        "CanGetCoolerPower" => camera.can_get_cooler_power().await.unwrap(),
        other => panic!("unknown boolean property: {other}"),
    };
    assert_eq!(
        actual, expected,
        "{property} expected {expected}, got {actual}"
    );
}

/// One scenario with a data table rather than a `Scenario Outline`: the
/// contract is about the capability surface as a whole — one member answering
/// while its neighbours refuse is the contradiction the check exists to remove
/// — and an outline pays a service start per member, the cost that pushed
/// `sky-survey-camera`'s suite past its 60s Bazel budget on Windows. The
/// members stay named in the feature file, per testing.md §2.5.
#[then(regex = r"^reading these members from camera device (\d+) is rejected with ASCOM (\w+):$")]
async fn capability_reads_rejected(
    world: &mut CameraWorld,
    step: &Step,
    _device: u32,
    code: String,
) {
    let camera = world.camera();
    let table = step
        .table()
        .expect("capability refusal step needs a data table of member names");
    for row in table.rows.iter().skip(1) {
        let member = &row[0];
        let error = match member.as_str() {
            "HasShutter" => camera.has_shutter().await.err(),
            "CanSetCCDTemperature" => camera.can_set_ccd_temperature().await.err(),
            "CanGetCoolerPower" => camera.can_get_cooler_power().await.err(),
            "CanAbortExposure" => camera.can_abort_exposure().await.err(),
            "CanStopExposure" => camera.can_stop_exposure().await.err(),
            "CanPulseGuide" => camera.can_pulse_guide().await.err(),
            "IsPulseGuiding" => camera.is_pulse_guiding().await.err(),
            other => panic!("unknown capability member: {other}"),
        };
        let error =
            error.unwrap_or_else(|| panic!("{member} answered instead of refusing with {code}"));
        assert_eq!(
            error.code.raw(),
            ascom_code(&code),
            "{member} refused with the wrong code"
        );
    }
}

// --- enumeration / health ---------------------------------------------------

#[then(regex = r"^ASCOM camera device (\d+) is available$")]
async fn camera_is_available(world: &mut CameraWorld, _device: u32) {
    assert!(
        world.camera.is_some(),
        "camera device {_device} not registered"
    );
}

#[then(regex = r"^camera device (\d+) reports a non-empty UniqueID$")]
async fn camera_non_empty_unique_id(world: &mut CameraWorld, _device: u32) {
    // `unique_id` is a sync `Device` member (not an HTTP round-trip).
    let camera = world.camera();
    assert_ne!(camera.unique_id(), "");
}

#[then("no ASCOM camera devices are registered")]
async fn no_cameras_registered(world: &mut CameraWorld) {
    assert!(world.camera.is_none(), "expected no Camera devices");
}

#[then("the service is healthy")]
async fn service_healthy(world: &mut CameraWorld) {
    assert!(world.management_responds().await, "service did not respond");
}

// --- generic rejection assertions -------------------------------------------

#[then(regex = r"^the (?:set|exposure|call) is rejected with ASCOM (\w+)$")]
async fn rejected_with(world: &mut CameraWorld, code: String) {
    assert_eq!(
        world.last_error_code,
        Some(ascom_code(&code)),
        "expected {code}, got {:?}",
        world.last_error_code
    );
}
