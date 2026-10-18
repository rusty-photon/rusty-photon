//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra`, plus the Givens that start this service as a process in each
//! shape and the steps on its (now empty) action lists.

use std::sync::Arc;

use cucumber::{given, then, when};

use crate::world::FalconRotatorWorld;

bdd_infra::setup_pages_steps!(FalconRotatorWorld);

#[given("a running pa-falcon-rotator process")]
async fn running_process(world: &mut FalconRotatorWorld) {
    world.start_process().await;
}

#[given(
    expr = "a running pa-falcon-rotator process with the serial port pinned to {string} by a command-line override"
)]
async fn running_process_with_serial_override(world: &mut FalconRotatorWorld, port: String) {
    world.start_process_with_serial_override(&port).await;
}

#[given("a running pa-falcon-rotator process with TLS and auth")]
async fn running_process_with_tls_and_auth(world: &mut FalconRotatorWorld) {
    world.start_process_with_tls_and_auth().await;
}

#[when("the supported actions are queried on the rotator device")]
async fn query_rotator_supported_actions(world: &mut FalconRotatorWorld) {
    let rotator = Arc::clone(world.rotator());
    world.last_supported_actions = Some(rotator.supported_actions().await.unwrap());
}

#[when("the supported actions are queried on the switch device")]
async fn query_switch_supported_actions(world: &mut FalconRotatorWorld) {
    let switch = Arc::clone(world.status_switch());
    world.last_supported_actions = Some(switch.supported_actions().await.unwrap());
}

#[then("the supported actions are empty")]
fn assert_supported_actions_empty(world: &mut FalconRotatorWorld) {
    let actions = world
        .last_supported_actions
        .as_ref()
        .expect("no supported actions queried");
    assert_eq!(actions, &Vec::<String>::new());
}
