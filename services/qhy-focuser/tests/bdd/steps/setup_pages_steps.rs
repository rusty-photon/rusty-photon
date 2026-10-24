//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra`, plus the Givens that start this service in each shape and
//! the two steps on its (now empty) action list.

use std::sync::Arc;

use cucumber::{given, then, when};

use crate::world::QhyFocuserWorld;

bdd_infra::setup_pages_steps!(QhyFocuserWorld);

#[given(
    expr = "a running focuser service with the serial port pinned to {string} by a command-line override"
)]
async fn running_with_serial_override(world: &mut QhyFocuserWorld, port: String) {
    world.start_with_serial_override(&port).await;
}

#[given("a running focuser service with TLS and auth")]
async fn running_with_tls_and_auth(world: &mut QhyFocuserWorld) {
    world.start_with_tls_and_auth().await;
}

#[when("the supported actions are queried")]
async fn query_supported_actions(world: &mut QhyFocuserWorld) {
    let focuser = Arc::clone(world.focuser());
    world.last_supported_actions = Some(focuser.supported_actions().await.unwrap());
}

#[then("the supported actions are empty")]
fn assert_supported_actions_empty(world: &mut QhyFocuserWorld) {
    let actions = world
        .last_supported_actions
        .as_ref()
        .expect("no supported actions queried");
    assert_eq!(actions, &Vec::<String>::new());
}
