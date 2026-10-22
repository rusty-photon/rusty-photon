//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra` — the pages' and the camera drivers' Cameras section — plus
//! the Given that starts this service behind TLS and auth, and the steps on
//! its (now empty) action list.

use ascom_alpaca::ASCOMErrorCode;
use cucumber::{given, then, when};

use crate::world::{registered_cameras, CameraWorld};

bdd_infra::setup_pages_steps!(CameraWorld);
bdd_infra::usb_devices_editor_steps!(CameraWorld);

#[given("the svbony-camera service is running with TLS and auth")]
async fn service_running_with_tls_and_auth(world: &mut CameraWorld) {
    world.start_with_tls_and_auth_pages().await;
}

#[when(regex = r"^the supported actions are queried on camera device (\d+)$")]
async fn query_supported_actions(world: &mut CameraWorld, device: u32) {
    let camera = world.device(device);
    world.last_actions = Some(camera.supported_actions().await.unwrap());
}

#[then("the supported actions are empty")]
fn supported_actions_empty(world: &mut CameraWorld) {
    let actions = world
        .last_actions
        .as_ref()
        .expect("no supported actions stashed");
    assert_eq!(actions, &Vec::<String>::new());
}

#[when(regex = r#"^the action "([^"]+)" is called on camera device (\d+)$"#)]
async fn call_named_action(world: &mut CameraWorld, action: String, device: u32) {
    match world.device(device).action(action, String::new()).await {
        Ok(_) => world.last_error_code = None,
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[then("the call should fail with an action-not-implemented error")]
fn call_action_not_implemented(world: &mut CameraWorld) {
    assert_eq!(
        world.last_error_code,
        Some(ASCOMErrorCode::ACTION_NOT_IMPLEMENTED.raw())
    );
}

#[then("the running service registers no Camera device")]
async fn running_service_registers_none(world: &mut CameraWorld) {
    let cameras = registered_cameras(world.port())
        .await
        .expect("the management API answers");
    assert!(
        cameras.is_empty(),
        "the running service registers {} Camera devices",
        cameras.len()
    );
}
