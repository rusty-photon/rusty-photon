//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra`, plus the guider's own — the start behind TLS and auth, the
//! restart a saved change waits for, the save that enables the camera
//! facade, the facade's setup paths, which send a browser to the guider's
//! page, and the facade's Actions, which no longer carry the configuration.

use std::path::PathBuf;

use bdd_infra::setup_pages::{self, SetupPagesWorld as _};
use cucumber::{given, then, when};

use super::camera_steps::{facade, record_error};
use crate::world::GuiderWorld;

bdd_infra::setup_pages_steps!(GuiderWorld);

#[given("the guider service is running with TLS and auth")]
async fn running_with_tls_and_auth(world: &mut GuiderWorld) {
    world.start_with_tls_and_auth_pages().await;
}

#[when("the guider service is restarted on its configuration file")]
async fn restarted(world: &mut GuiderWorld) {
    world.restart_service().await;
}

/// Submit the server page with the facade enabled on `image_dir`, its
/// listener on the scenario's reserved port.
async fn submit_enabling_facade(world: &mut GuiderWorld, image_dir: PathBuf) {
    let port = bdd_infra::reserved_port_once(&mut world.camera_port)
        .await
        .to_string();
    let image_dir = image_dir.to_str().expect("a UTF-8 image_dir").to_string();
    setup_pages::submit(
        world.setup_pages(),
        setup_pages::SERVER_PATH,
        &[
            ("camera.enabled", "on"),
            ("camera.pixel_size_x_um", "3.75"),
            ("camera.pixel_size_y_um", "3.75"),
            ("camera.image_dir", &image_dir),
            ("camera.server.port", &port),
        ],
        None,
    )
    .await;
}

#[when("I submit the server setup page enabling the camera facade on its image directory")]
async fn enable_facade(world: &mut GuiderWorld) {
    let dir = world.image_dir();
    submit_enabling_facade(world, dir).await;
}

#[when("I submit the server setup page enabling the camera facade on a missing image directory")]
async fn enable_facade_on_missing_dir(world: &mut GuiderWorld) {
    let dir = world.temp_dir_path().join("no-such-dir");
    submit_enabling_facade(world, dir).await;
}

/// `GET` one of the facade's setup paths, keeping the redirect it answers
/// rather than following it.
async fn open_facade(world: &mut GuiderWorld, path: &str) {
    let port = world.camera_port.expect("the facade is not configured");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("a client that keeps redirects builds");
    let response = client
        .get(format!("http://127.0.0.1:{port}{path}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("the facade did not answer {path}: {e}"));
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    world.facade_setup_answer = Some((response.status().as_u16(), location));
}

#[when("I open the facade's server setup page")]
async fn open_facade_server_page(world: &mut GuiderWorld) {
    open_facade(world, setup_pages::SERVER_PATH).await;
}

#[when(expr = "I open the facade's setup page of device {string} {int}")]
async fn open_facade_device_page(world: &mut GuiderWorld, device_type: String, number: u32) {
    open_facade(world, &setup_pages::device_path(&device_type, number)).await;
}

#[when(expr = "the client calls the facade's Action {string}")]
async fn call_facade_action(world: &mut GuiderWorld, name: String) {
    let camera = facade(world).await;
    let result = camera.action(name, String::new()).await.map(|_| ());
    record_error(world, result);
}

#[then("the facade sends the browser to the guider's server setup page")]
fn sent_to_guider_page(world: &mut GuiderWorld) {
    let (status, location) = world
        .facade_setup_answer
        .clone()
        .expect("no facade setup path was opened");
    let port = world
        .service_handle
        .as_ref()
        .expect("the guider service was not started")
        .port();
    assert_eq!(status, 303, "the facade's answer");
    assert_eq!(
        location,
        Some(format!("http://127.0.0.1:{port}/setup")),
        "the facade's redirect"
    );
}

#[then(expr = "the page shows {string} under the Identity disclosure")]
fn under_identity(world: &mut GuiderWorld, name: String) {
    let body = &world.setup_pages.last_body;
    let css = format!(r#"details.identity input[name="{name}"]"#);
    assert!(
        setup_pages::dom::matches(body, &css),
        "{name} is not under the Identity disclosure:\n{body}"
    );
}

#[then("the facade's supported actions should be the sweep lease's alone")]
async fn lease_actions_alone(world: &mut GuiderWorld) {
    let actions = facade(world)
        .await
        .supported_actions()
        .await
        .expect("SupportedActions");
    assert_eq!(
        actions,
        [
            "rustyphoton:sweep-lease-acquire",
            "rustyphoton:sweep-lease-renew",
            "rustyphoton:sweep-lease-release"
        ]
    );
}

#[then(expr = "the page does not contain {string}")]
fn page_lacks(world: &mut GuiderWorld, needle: String) {
    let body = &world.setup_pages.last_body;
    assert!(!body.is_empty(), "no page was fetched");
    assert!(
        !body.contains(&needle),
        "{needle:?} is on the page:\n{body}"
    );
}
