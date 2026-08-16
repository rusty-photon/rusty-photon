//! Step definitions for `pointing_api.feature` (contracts P1-P7).

use crate::world::SkySurveyCameraWorld;
use cucumber::{given, then, when};
use serde_json::Value;

#[given(expr = "the camera is connected with initial pointing RA {float} Dec {float}")]
async fn connected_with_pointing(world: &mut SkySurveyCameraWorld, ra: f64, dec: f64) {
    world.initial_ra_deg = ra;
    world.initial_dec_deg = dec;
    world.spawn_skyview_stub_ok().await;
    world.start_service().await;
    world.set_camera_connected(true).await;
    if let Some(code) = world.last_ascom_error {
        panic!("expected connect to succeed in pointing_api setup, got ASCOM {code:#X}");
    }
}

#[given("the camera is started but not connected")]
async fn started_not_connected(world: &mut SkySurveyCameraWorld) {
    world.start_service().await;
}

#[when(expr = "I POST RA {float} Dec {float} to the position endpoint")]
async fn post_position(world: &mut SkySurveyCameraWorld, ra: f64, dec: f64) {
    post_with_optional_rotation(world, ra, dec, None).await;
}

#[when(expr = "I POST RA {float} Dec {float} rotation {float} to the position endpoint")]
async fn post_position_with_rotation(
    world: &mut SkySurveyCameraWorld,
    ra: f64,
    dec: f64,
    rot: f64,
) {
    post_with_optional_rotation(world, ra, dec, Some(rot)).await;
}

async fn post_with_optional_rotation(
    world: &mut SkySurveyCameraWorld,
    ra: f64,
    dec: f64,
    rotation: Option<f64>,
) {
    let url = format!("{}/sky-survey/position", world.base_url());
    let mut body = serde_json::Map::new();
    body.insert("ra".into(), serde_json::json!(ra));
    body.insert("dec".into(), serde_json::json!(dec));
    if let Some(r) = rotation {
        body.insert("rotation".into(), serde_json::json!(r));
    }
    let client = world.http();
    let response = client
        .post(&url)
        .json(&Value::Object(body))
        .send()
        .await
        .expect("POST /sky-survey/position failed");
    world.last_http_status = Some(response.status().as_u16());
    world.last_http_body = Some(response.text().await.unwrap_or_default());
}

#[when("I POST a malformed JSON body to the position endpoint")]
async fn post_malformed(world: &mut SkySurveyCameraWorld) {
    let url = format!("{}/sky-survey/position", world.base_url());
    let client = world.http();
    let response = client
        .post(&url)
        .header("content-type", "application/json")
        .body("{ this is not json")
        .send()
        .await
        .expect("POST malformed failed");
    world.last_http_status = Some(response.status().as_u16());
    world.last_http_body = Some(response.text().await.unwrap_or_default());
}

#[when("I GET the position endpoint")]
async fn get_position(world: &mut SkySurveyCameraWorld) {
    let url = format!("{}/sky-survey/position", world.base_url());
    let client = world.http();
    let response = client.get(&url).send().await.expect("GET failed");
    world.last_http_status = Some(response.status().as_u16());
    world.last_http_body = Some(response.text().await.unwrap_or_default());
}

#[then(expr = "the response status is {int}")]
fn response_status(world: &mut SkySurveyCameraWorld, status: u16) {
    let actual = world
        .last_http_status
        .expect("no HTTP status captured — did the When step run?");
    assert_eq!(
        actual, status,
        "expected HTTP {status}, got {actual} (body: {:?})",
        world.last_http_body
    );
}

#[then(expr = "the position response reports RA {float} Dec {float}")]
fn response_reports_position(world: &mut SkySurveyCameraWorld, ra: f64, dec: f64) {
    let body = world.last_http_body.as_deref().expect("no body captured");
    let value: Value = serde_json::from_str(body).expect("body is not JSON");
    let actual_ra = value["ra"].as_f64().expect("missing ra");
    let actual_dec = value["dec"].as_f64().expect("missing dec");
    assert!(
        (actual_ra - ra).abs() < 1e-6,
        "expected RA {ra}, got {actual_ra}"
    );
    assert!(
        (actual_dec - dec).abs() < 1e-6,
        "expected Dec {dec}, got {actual_dec}"
    );
}

#[then(expr = "the position response reports rotation {float}")]
fn response_reports_rotation(world: &mut SkySurveyCameraWorld, rot: f64) {
    let body = world.last_http_body.as_deref().expect("no body captured");
    let value: Value = serde_json::from_str(body).expect("body is not JSON");
    let actual = value["rotation"].as_f64().expect("missing rotation");
    assert!(
        (actual - rot).abs() < 1e-6,
        "expected rotation {rot}, got {actual}"
    );
}
