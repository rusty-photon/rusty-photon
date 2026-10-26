//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra`, plus the Whens that start this service in each shape and
//! the steps on its (now empty) action lists.

use std::sync::Arc;

use cucumber::{then, when};

use crate::world::GeminiWorld;

bdd_infra::setup_pages_steps!(GeminiWorld);

#[when(
    expr = "I start the Gemini hub server with the serial port pinned to {string} by a command-line override"
)]
async fn start_with_serial_override(world: &mut GeminiWorld, port: String) {
    world.start_gemini_with_args(&["--port", &port]).await;
}

#[when("I start the Gemini hub server with TLS and auth")]
async fn start_with_tls_and_auth(world: &mut GeminiWorld) {
    world.start_gemini_with_tls_and_auth().await;
}

#[when("the supported actions are queried on the switch device")]
async fn query_switch_supported_actions(world: &mut GeminiWorld) {
    let switch = Arc::clone(world.switch.as_ref().expect("switch not discovered"));
    world.last_supported_actions = Some(switch.supported_actions().await.unwrap());
}

#[when("the supported actions are queried on the observingconditions device")]
async fn query_oc_supported_actions(world: &mut GeminiWorld) {
    let oc = Arc::clone(world.oc.as_ref().expect("OC device not discovered"));
    world.last_supported_actions = Some(oc.supported_actions().await.unwrap());
}

#[then("the supported actions are empty")]
fn assert_supported_actions_empty(world: &mut GeminiWorld) {
    let actions = world
        .last_supported_actions
        .as_ref()
        .expect("no supported actions queried");
    assert_eq!(actions, &Vec::<String>::new());
}
