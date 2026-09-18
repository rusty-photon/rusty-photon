//! Step definitions for `connection_lifecycle.feature` (contracts C1-C4).

use crate::world::SkySurveyCameraWorld;
use cucumber::gherkin::Step;
use cucumber::{given, then, when};

#[given("a sky-survey-camera with default optics")]
const fn default_optics(_world: &mut SkySurveyCameraWorld) {
    // Defaults are baked into `World::build_config_json`. This step
    // exists to anchor the scenario's first Given clause; concrete
    // tweaks happen in subsequent Givens.
}

#[given("a writable cache directory")]
const fn writable_cache_dir(_world: &mut SkySurveyCameraWorld) {
    // No-op: the default cache_dir under `temp_dir/cache` is writable.
    // This step is for documentation in the feature file.
}

#[given("a non-writable cache directory")]
fn non_writable_cache_dir(world: &mut SkySurveyCameraWorld) {
    world.set_unwritable_cache_dir();
}

#[given("SkyView is reachable")]
async fn skyview_reachable(world: &mut SkySurveyCameraWorld) {
    world.spawn_skyview_stub_ok().await;
}

#[given("SkyView is unreachable")]
fn skyview_unreachable(world: &mut SkySurveyCameraWorld) {
    world.set_unreachable_survey_endpoint();
}

#[when("I start the service")]
async fn start_service(world: &mut SkySurveyCameraWorld) {
    world.start_service().await;
}

#[when("I connect the camera")]
async fn connect_camera(world: &mut SkySurveyCameraWorld) {
    world.set_camera_connected(true).await;
    if let Some(code) = world.last_ascom_error {
        panic!("expected connect to succeed, got ASCOM error {code:#X}");
    }
}

#[when("I try to connect the camera")]
async fn try_connect_camera(world: &mut SkySurveyCameraWorld) {
    world.set_camera_connected(true).await;
}

#[when("I disconnect the camera")]
async fn disconnect_camera(world: &mut SkySurveyCameraWorld) {
    world.set_camera_connected(false).await;
}

#[then("the camera is connected")]
async fn camera_is_connected(world: &mut SkySurveyCameraWorld) {
    let connected = read_connected(world).await;
    assert!(connected, "expected camera to be connected");
}

#[then("the camera is not connected")]
async fn camera_is_not_connected(world: &mut SkySurveyCameraWorld) {
    let connected = read_connected(world).await;
    assert!(!connected, "expected camera to be disconnected");
}

#[then("the connect attempt fails with ASCOM UNSPECIFIED_ERROR")]
fn connect_fails_unspecified(world: &mut SkySurveyCameraWorld) {
    let code = world
        .last_ascom_error
        .expect("no ASCOM error captured — was set_connected called?");
    assert_eq!(
        code, 0x500,
        "expected UNSPECIFIED_ERROR (0x500), got {code:#X}"
    );
}

async fn read_connected(world: &mut SkySurveyCameraWorld) -> bool {
    let url = format!("{}/api/v1/camera/0/connected", world.base_url());
    let client = world.http();
    let response = client
        .get(&url)
        .query(&[("ClientID", "1"), ("ClientTransactionID", "1")])
        .send()
        .await
        .expect("GET /connected failed");
    let body: serde_json::Value = response.json().await.expect("response body not JSON");
    body["Value"]
        .as_bool()
        .expect("Value field missing or not bool")
}

/// Read every listed member of the exposure-state surface and assert each is
/// refused (C5). One step over a table rather than a scenario outline: the
/// contract is about the surface as a whole — one member answering while its
/// neighbours refuse is the contradiction the check exists to remove — and an
/// outline would pay a service start, and for the post-frame case a survey
/// exposure, per member. That cost is what pushed this suite past its 60s
/// Bazel budget on Windows; the members stay listed in the feature file, which
/// is what §2.5 of the testing skill asks for.
#[then("reading these members is rejected with ASCOM NOT_CONNECTED:")]
async fn reads_rejected_not_connected(world: &mut SkySurveyCameraWorld, step: &Step) {
    let table = step
        .table
        .as_ref()
        .expect("step requires a data table of member names");
    for row in table.rows.iter().skip(1) {
        let member = row.first().expect("table row must name a member");
        let method = match member.as_str() {
            "CameraState" => "camerastate",
            "ImageReady" => "imageready",
            "PercentCompleted" => "percentcompleted",
            "LastExposureStartTime" => "lastexposurestarttime",
            "LastExposureDuration" => "lastexposureduration",
            "ImageArray" => "imagearray",
            other => panic!("unknown exposure-state member: {other}"),
        };
        world.last_ascom_error = None;
        world.get_camera(method).await;
        let actual = world.last_ascom_error.unwrap_or_else(|| {
            panic!(
                "{member} answered instead of refusing (body: {:?})",
                world.last_http_body
            )
        });
        assert_eq!(
            actual, 0x407,
            "{member}: expected NOT_CONNECTED (0x407), got {actual:#X} (body: {:?})",
            world.last_http_body
        );
    }
    // The refusals this step asserted on are consumed here. `put_camera` only
    // ever *sets* `last_ascom_error`, so a later step's success guard (e.g.
    // "I connect the camera") would otherwise trip on the last read's expected
    // 0x407 and report a connect failure that never happened.
    world.last_ascom_error = None;
}
