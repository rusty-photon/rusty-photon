//! BDD step definitions for the closed-loop centering scenario.
//!
//! Phase-4 integration path from
//! `docs/plans/archive/sky-survey-camera-mount-following.md`: the camera is a
//! real `sky-survey-camera` process configured to follow `OmniSim`'s
//! Telescope. The scenario primes a one-shot pointing override on
//! the camera (F7) before invoking `center_on_target`, so iter 0
//! sees the camera "off-target", syncs the mount, slews, and iter 1
//! reads the mount fresh and converges.

use cucumber::{given, when};

use bdd_infra::rp_harness::{CameraConfig, MountConfig};
use bdd_infra::sky_survey_camera_harness::{
    start_sky_survey_camera, SkySurveyCameraConfigBuilder, SkyViewStub, TelescopeFollow,
};

use crate::steps::tool_steps::{ensure_omnisim, start_rp};
use crate::world::RpWorld;

// --- Given steps ---

#[given("the SkyView stub is ready")]
async fn skyview_stub_ready(world: &mut RpWorld) {
    if world.sky_view_stub.is_none() {
        world.sky_view_stub = Some(SkyViewStub::start().await);
    }
}

#[given(
    expr = "sky-survey-camera follows the simulated mount with offset_ra_arcsec {float} offset_dec_arcsec {float}"
)]
async fn sky_survey_camera_follows(world: &mut RpWorld, offset_ra: f64, offset_dec: f64) {
    ensure_omnisim(world).await;
    let omnisim_url = world.omnisim_url();
    let stub_url = world
        .sky_view_stub
        .as_ref()
        .expect("SkyView stub must be started before sky-survey-camera")
        .url
        .clone();

    let cfg = SkySurveyCameraConfigBuilder::new(stub_url)
        .with_sensor(64, 48)
        .with_follow(TelescopeFollow {
            alpaca_url: omnisim_url,
            device_number: 0,
            offset_ra_arcsec: offset_ra,
            offset_dec_arcsec: offset_dec,
            request_timeout: std::time::Duration::from_secs(2),
        })
        .build();

    let (handle, cache) = start_sky_survey_camera(&cfg).await;
    let cam_url = handle.base_url.clone();

    world.sky_survey_camera = Some(handle);
    world.sky_survey_camera_cache = Some(cache);
    world.cameras.push(CameraConfig {
        id: "main-cam".to_string(),
        alpaca_url: cam_url,
        device_number: 0,
        cooler_targets_c: Vec::new(),
    });
}

#[given("rp is running with the sky-survey-camera and a mount on the simulator")]
async fn rp_with_sky_survey_camera_and_mount(world: &mut RpWorld) {
    let url = world.omnisim_url();
    world.mount = Some(MountConfig {
        alpaca_url: url,
        device_number: 0,
        settle_after_slew: None,
    });
    start_rp(world).await;
}

// --- When steps ---

#[when(expr = "sky-survey-camera is told its next exposure is at ra_deg {float} dec_deg {float}")]
async fn arm_one_shot_override(world: &mut RpWorld, ra_deg: f64, dec_deg: f64) {
    // POSTs the override directly to sky-survey-camera's
    // `/sky-survey/position` endpoint (F7). In follow mode the
    // request arms a one-shot pointing override that the next light
    // exposure consumes; subsequent exposures resume reading the
    // mount. The mount-follow + offset config from the Given steps
    // remains in place.
    let cam_url = world
        .sky_survey_camera
        .as_ref()
        .expect("sky-survey-camera handle missing — was the Given step run?")
        .base_url
        .clone();
    let url = format!("{cam_url}/sky-survey/position");
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "ra": ra_deg, "dec": dec_deg }))
        .send()
        .await
        .expect("POST /sky-survey/position failed");
    assert!(
        resp.status().is_success(),
        "POST /sky-survey/position returned {}",
        resp.status()
    );
}
