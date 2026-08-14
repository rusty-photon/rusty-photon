//! Step definitions for `exposure_survey.feature` (contracts S1-S6).

use crate::world::{make_zero_fits, SkySurveyCameraWorld, StubBehavior};
use cucumber::{given, then, when};
use std::time::Duration;

#[given("the survey backend returns a healthy FITS cutout")]
fn survey_healthy(world: &mut SkySurveyCameraWorld) {
    let fits = make_zero_fits(640, 480);
    world.set_stub_behavior(StubBehavior::ServingFits(fits));
}

#[given("the survey backend returns HTTP 500")]
fn survey_500(world: &mut SkySurveyCameraWorld) {
    world.set_stub_behavior(StubBehavior::Status500);
}

#[given("the survey backend exceeds the request timeout")]
fn survey_timeout(world: &mut SkySurveyCameraWorld) {
    world.set_stub_behavior(StubBehavior::Hold);
}

#[given("the survey backend returns a malformed FITS body")]
fn survey_malformed(world: &mut SkySurveyCameraWorld) {
    world.set_stub_behavior(StubBehavior::Malformed);
}

#[given("the cache contains a hit for the next request")]
fn cache_hit(world: &mut SkySurveyCameraWorld) {
    use sky_survey_camera::camera::build_full_sensor_request;
    use sky_survey_camera::config::{
        AlpacaServerConfig, Config, DeviceConfig, OpticsConfig, PointingConfig, SurveyConfig,
    };
    use sky_survey_camera::pointing::PointingState;

    let pointing = PointingState::new(
        world.initial_ra_deg,
        world.initial_dec_deg,
        world.initial_rotation_deg,
    );
    // World defaults to 1000mm focal length, 3.76um pixels, 640x480
    // sensor — match what build_config_json injects.
    let config = Config {
        device: DeviceConfig {
            name: "Test Sky Survey Camera".into(),
            unique_id: "sky-survey-camera-test-001".into(),
            description: "BDD test instance".into(),
        },
        optics: OpticsConfig {
            focal_length_mm: world.focal_length_mm.unwrap_or(1000.0),
            pixel_size_x_um: world.pixel_size_x_um.unwrap_or(3.76),
            pixel_size_y_um: world.pixel_size_y_um.unwrap_or(3.76),
            sensor_width_px: world.sensor_width_px.unwrap_or(640),
            sensor_height_px: world.sensor_height_px.unwrap_or(480),
        },
        pointing: PointingConfig {
            initial_ra_deg: world.initial_ra_deg,
            initial_dec_deg: world.initial_dec_deg,
            initial_rotation_deg: world.initial_rotation_deg,
            telescope: None,
            rotator: None,
        },
        survey: SurveyConfig {
            name: world
                .survey_name
                .clone()
                .unwrap_or_else(|| "DSS2 Red".to_string()),
            request_timeout: Duration::from_secs(5),
            cache_dir: world.cache_dir(),
            endpoint: "http://placeholder/".to_string(),
        },
        server: AlpacaServerConfig::new(0),
    };
    let req = build_full_sensor_request(&config, pointing, 1, 1);
    let key = req.cache_key();
    let fits = make_zero_fits(640, 480);
    world.preseed_cache(&key, &fits);
}

#[when("I StartExposure with default parameters")]
async fn start_exposure_default(world: &mut SkySurveyCameraWorld) {
    world.drive_start_exposure_default().await;
    if let Some(code) = world.last_ascom_error {
        panic!("StartExposure rejected with ASCOM {code:#X}; expected to spawn task");
    }
}

#[when("I StartExposure with Light=false")]
async fn start_exposure_dark(world: &mut SkySurveyCameraWorld) {
    world.drive_start_exposure_dark().await;
    if let Some(code) = world.last_ascom_error {
        panic!("StartExposure rejected with ASCOM {code:#X}; expected to spawn task");
    }
}

#[then(expr = "the resulting image has dimensions {int} by {int}")]
async fn image_dimensions(world: &mut SkySurveyCameraWorld, w: u32, h: u32) {
    assert!(
        world.wait_for_image_ready(Duration::from_secs(10)).await,
        "image never became ready within 10s"
    );
    let (actual_w, actual_h) = world.get_image_dimensions().await;
    assert_eq!(
        (actual_w, actual_h),
        (w, h),
        "expected {w}x{h}, got {actual_w}x{actual_h}"
    );
}

#[then("every pixel of the resulting image is zero")]
async fn image_all_zero(world: &mut SkySurveyCameraWorld) {
    assert!(
        world.wait_for_image_ready(Duration::from_secs(10)).await,
        "image never became ready within 10s"
    );
    world.assert_image_all_zero().await;
}

#[then("the exposure fails with ASCOM UNSPECIFIED_ERROR")]
async fn exposure_fails_unspecified(world: &mut SkySurveyCameraWorld) {
    // Wait for the spawned task to clear in_flight (success or failure).
    // Timeout includes the survey request_timeout (configured 5s) plus a
    // safety margin.
    let deadline = Duration::from_secs(8);
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        let url = format!("{}/api/v1/camera/0/imagearray", world.base_url());
        let client = world.http();
        let response = client
            .get(&url)
            .query(&[("ClientID", "1"), ("ClientTransactionID", "1")])
            .header("accept", "application/json")
            .send()
            .await
            .expect("GET /imagearray failed");
        let body: serde_json::Value = response.json().await.expect("response not JSON");
        let err = body
            .get("ErrorNumber")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        if err == 0x500 {
            world.last_ascom_error = Some(err);
            return;
        }
        assert!(
            !(err != 0 && err != 0x40B),
            "expected ASCOM UNSPECIFIED_ERROR (0x500), got {err:#X}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("exposure never failed with UNSPECIFIED_ERROR within {deadline:?}");
}

#[then("no outbound survey HTTP request was made")]
async fn no_outbound_request(world: &mut SkySurveyCameraWorld) {
    // For Light=false the spawned task synthesises a zero array and
    // never contacts the survey backend. For cache-hit it loads from
    // disk. Either way, get_count should be exactly zero (HEAD calls
    // during connect are not counted by the stub).
    let count = world.stub_get_count();
    assert_eq!(count, 0, "expected 0 outbound GETs, got {count}");
}
