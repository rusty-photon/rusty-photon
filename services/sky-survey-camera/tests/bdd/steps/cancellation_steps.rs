//! Step definitions for `cancellation.feature` (contracts A1-A4).

use crate::world::SkySurveyCameraWorld;
use cucumber::{then, when};

#[when("I AbortExposure")]
async fn abort_exposure(world: &mut SkySurveyCameraWorld) {
    world.last_ascom_error = None;
    world.put_camera("abortexposure", &[]).await;
}

#[when("I StopExposure")]
async fn stop_exposure(world: &mut SkySurveyCameraWorld) {
    world.last_ascom_error = None;
    world.put_camera("stopexposure", &[]).await;
}

#[then("ImageReady is false")]
async fn image_ready_false(world: &mut SkySurveyCameraWorld) {
    let url = format!("{}/api/v1/camera/0/imageready", world.base_url());
    let client = world.http();
    let response = client
        .get(&url)
        .query(&[("ClientID", "1"), ("ClientTransactionID", "1")])
        .send()
        .await
        .expect("GET /imageready failed");
    let body: serde_json::Value = response.json().await.expect("response not JSON");
    let value = body["Value"]
        .as_bool()
        .expect("Value field missing or not bool");
    assert!(!value, "expected ImageReady=false, got {value}");
}

#[then("ImageReady is true")]
async fn image_ready_true(world: &mut SkySurveyCameraWorld) {
    let url = format!("{}/api/v1/camera/0/imageready", world.base_url());
    let client = world.http();
    let response = client
        .get(&url)
        .query(&[("ClientID", "1"), ("ClientTransactionID", "1")])
        .send()
        .await
        .expect("GET /imageready failed");
    let body: serde_json::Value = response.json().await.expect("response not JSON");
    let value = body["Value"]
        .as_bool()
        .expect("Value field missing or not bool");
    assert!(value, "expected ImageReady=true, got {value}");
}

#[then("the cancellation succeeds")]
fn cancellation_succeeds(world: &mut SkySurveyCameraWorld) {
    if let Some(code) = world.last_ascom_error {
        panic!("expected cancellation to succeed, got ASCOM {code:#X}");
    }
}
