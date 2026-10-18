//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra`, plus the Whens that start this service in each shape, the
//! file-only check for the switch labels, and the steps on its (now empty)
//! action lists.

use std::sync::Arc;

use bdd_infra::setup_pages::dom;
use cucumber::{then, when};

use crate::world::PpbaWorld;

bdd_infra::setup_pages_steps!(PpbaWorld);

#[when(
    expr = "I start the PPBA server with the serial port pinned to {string} by a command-line override"
)]
async fn start_with_serial_override(world: &mut PpbaWorld, port: String) {
    world.start_ppba_with_args(&["--port", &port]).await;
}

#[when("I start the PPBA server with TLS and auth")]
async fn start_with_tls_and_auth(world: &mut PpbaWorld) {
    world.start_ppba_with_tls_and_auth().await;
}

#[then(expr = "the page names {string} as edited in the configuration file")]
fn assert_named_file_only(world: &mut PpbaWorld, field: String) {
    let html = &world.setup_pages.last_body;
    let css = format!("div.file-only[id=\"{field}\"]");
    assert!(
        dom::text_contains(html, &css, "is edited in the configuration file"),
        "no file-only note for {field} in:\n{html}"
    );
}

#[when("the supported actions are queried on the switch device")]
async fn query_switch_supported_actions(world: &mut PpbaWorld) {
    let switch = Arc::clone(world.switch.as_ref().expect("switch not discovered"));
    world.last_supported_actions = Some(switch.supported_actions().await.unwrap());
}

#[when("the supported actions are queried on the observingconditions device")]
async fn query_oc_supported_actions(world: &mut PpbaWorld) {
    let oc = Arc::clone(world.oc.as_ref().expect("OC device not discovered"));
    world.last_supported_actions = Some(oc.supported_actions().await.unwrap());
}

#[then("the supported actions are empty")]
fn assert_supported_actions_empty(world: &mut PpbaWorld) {
    let actions = world
        .last_supported_actions
        .as_ref()
        .expect("no supported actions queried");
    assert_eq!(actions, &Vec::<String>::new());
}
