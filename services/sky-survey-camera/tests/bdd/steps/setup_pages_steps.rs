//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra`, plus the Givens that start this service in each shape and
//! the steps on its (now empty) action list. The optics and `SkyView`
//! Givens are `connection_steps`', the follow-mode ones `follow_mode_steps`'.

use ascom_alpaca::ASCOMErrorCode;
use cucumber::{given, then, when};

use crate::world::SkySurveyCameraWorld;

bdd_infra::setup_pages_steps!(SkySurveyCameraWorld);

#[given("the service is running")]
async fn service_running(world: &mut SkySurveyCameraWorld) {
    world.start_service().await;
}

#[given("the service is running with TLS and auth")]
async fn service_running_with_tls_and_auth(world: &mut SkySurveyCameraWorld) {
    world.start_with_tls_and_auth_pages().await;
}

#[when("the supported actions are queried")]
async fn query_supported_actions(world: &mut SkySurveyCameraWorld) {
    let camera = world.acquire_camera().await;
    world.last_supported_actions = Some(camera.supported_actions().await.unwrap());
}

#[then("the supported actions are empty")]
fn assert_supported_actions_empty(world: &mut SkySurveyCameraWorld) {
    let actions = world
        .last_supported_actions
        .as_ref()
        .expect("no supported actions queried");
    assert_eq!(actions, &Vec::<String>::new());
}

#[when(regex = r#"^the action "([^"]+)" is called$"#)]
async fn call_named_action(world: &mut SkySurveyCameraWorld, name: String) {
    let camera = world.acquire_camera().await;
    match camera.action(name, String::new()).await {
        Ok(_) => world.last_action_error_code = None,
        Err(e) => world.last_action_error_code = Some(e.code.raw()),
    }
}

#[then("the call should fail with an action-not-implemented error")]
fn assert_action_not_implemented(world: &mut SkySurveyCameraWorld) {
    assert_eq!(
        world.last_action_error_code,
        Some(ASCOMErrorCode::ACTION_NOT_IMPLEMENTED.raw()),
        "expected ACTION_NOT_IMPLEMENTED, got {:?}",
        world.last_action_error_code
    );
}
