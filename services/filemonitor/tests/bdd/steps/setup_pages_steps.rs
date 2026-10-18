//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra`, plus the Given that starts this service behind TLS and auth
//! and the steps on its (now empty) action list.

use std::sync::Arc;

use ascom_alpaca::ASCOMErrorCode;
use cucumber::{given, then, when};

use crate::world::FilemonitorWorld;

bdd_infra::setup_pages_steps!(FilemonitorWorld);

#[given("filemonitor is running with TLS and auth")]
async fn running_with_tls_and_auth(world: &mut FilemonitorWorld) {
    world.start_with_tls_and_auth_pages().await;
}

#[when("the supported actions are queried")]
async fn query_supported_actions(world: &mut FilemonitorWorld) {
    let monitor = Arc::clone(world.monitor());
    world.last_supported_actions = Some(monitor.supported_actions().await.unwrap());
}

#[then("the supported actions are empty")]
fn assert_supported_actions_empty(world: &mut FilemonitorWorld) {
    let actions = world
        .last_supported_actions
        .as_ref()
        .expect("no supported actions queried");
    assert_eq!(actions, &Vec::<String>::new());
}

#[when(regex = r#"^the action "([^"]+)" is called$"#)]
async fn call_named_action(world: &mut FilemonitorWorld, name: String) {
    let monitor = Arc::clone(world.monitor());
    world.last_ascom_error = monitor.action(name, String::new()).await.err();
}

#[then("the call should fail with an action-not-implemented error")]
fn assert_action_not_implemented(world: &mut FilemonitorWorld) {
    let err = world
        .last_ascom_error
        .take()
        .expect("expected an error from the call");
    assert_eq!(err.code, ASCOMErrorCode::ACTION_NOT_IMPLEMENTED);
}
