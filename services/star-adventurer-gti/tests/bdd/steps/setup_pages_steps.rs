//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra`, the Givens that start this service in each shape, the steps
//! on the transport selector, the steps on the mount's (unchanged) action
//! list, and the reload steps `connection_lifecycle.feature` shares: a save
//! on the mount's page is how a scenario reloads the driver.

use std::sync::Arc;

use ascom_alpaca::ASCOMErrorCode;
use bdd_infra::setup_pages::{self, dom, SetupPagesWorld as _, SERVER_PATH};
use cucumber::{given, then, when};

use crate::world::StarAdventurerWorld;

bdd_infra::setup_pages_steps!(StarAdventurerWorld);

/// The mount's page.
fn mount_page() -> String {
    setup_pages::device_path("telescope", 0)
}

#[given(
    expr = "a running star-adventurer service with the serial port pinned to {string} by a command-line override"
)]
async fn running_with_port_override(world: &mut StarAdventurerWorld, port: String) {
    world.start_service_with_flags(&["--port", &port]).await;
}

#[given(
    "a running star-adventurer service with the transport pinned to udp by a command-line override"
)]
async fn running_with_transport_override(world: &mut StarAdventurerWorld) {
    world
        .start_service_with_flags(&["--transport", "udp"])
        .await;
}

#[given("a running star-adventurer service with TLS and auth")]
async fn running_with_tls_and_auth(world: &mut StarAdventurerWorld) {
    world.start_with_tls_and_auth_pages().await;
}

/// The reload a connection-lifecycle scenario needs: pin the bound port
/// so the rebuilt server keeps the address, then save a changed field on
/// the mount's page.
#[when(expr = "the mount's setup page saves the description {string}")]
async fn save_description(world: &mut StarAdventurerWorld, description: String) {
    save_mount_page(world, &[("mount.description", &description)]).await;
}

#[when(
    regex = r#"^the mount's setup page saves the description "([^"]+)" and a post-slew settle of (\d+) seconds$"#
)]
async fn save_description_and_settle(
    world: &mut StarAdventurerWorld,
    description: String,
    settle_secs: u64,
) {
    let settle = format!("{settle_secs}s");
    save_mount_page(
        world,
        &[
            ("mount.description", &description),
            ("mount.settle_after_slew", &settle),
        ],
    )
    .await;
}

async fn save_mount_page(world: &mut StarAdventurerWorld, changes: &[(&str, &str)]) {
    let state = world.setup_pages();
    setup_pages::pin_port_in_file(state);
    setup_pages::submit(state, &mount_page(), changes, None).await;
    assert_eq!(state.last_status, Some(200), "{}", state.last_body);
    assert!(
        dom::matches(&state.last_body, "#saved-reloading"),
        "the save did not report a reload:\n{}",
        state.last_body
    );
}

/// Also a `When`: a scenario that goes on to act on the reloaded service
/// uses it as the barrier that the reload has landed.
#[when(expr = "the reloaded service serves mount description {string}")]
#[then(expr = "the reloaded service serves mount description {string}")]
async fn reloaded_serves_description(world: &mut StarAdventurerWorld, expected: String) {
    setup_pages::wait_until_input_shows(
        world.setup_pages(),
        &mount_page(),
        "mount.description",
        &expected,
    )
    .await;
}

#[when(
    expr = "I submit the server setup page switching the transport to udp at {string} bound to {string}"
)]
async fn switch_transport_to_udp(world: &mut StarAdventurerWorld, address: String, bind: String) {
    setup_pages::submit(
        world.setup_pages(),
        SERVER_PATH,
        &[
            ("transport", "udp"),
            ("transport:udp.address", &address),
            ("transport:udp.bind_address", &bind),
        ],
        None,
    )
    .await;
}

#[when(
    expr = "I submit the setup page of device {string} {int} enabling the auto-flip at an offset of {string} hours"
)]
async fn submit_auto_flip(
    world: &mut StarAdventurerWorld,
    device_type: String,
    number: u32,
    offset: String,
) {
    let path = setup_pages::device_path(&device_type, number);
    setup_pages::submit(
        world.setup_pages(),
        &path,
        &[
            ("mount.auto_flip.enabled", "on"),
            ("mount.auto_flip.meridian_offset_hours", &offset),
        ],
        None,
    )
    .await;
}

#[then(expr = "the transport selector shows {string} chosen")]
fn transport_chosen(world: &mut StarAdventurerWorld, kind: String) {
    let state = world.setup_pages();
    let css = format!(r#"input[name="transport"][value="{kind}"][checked]"#);
    assert!(
        dom::matches(&state.last_body, &css),
        "the {kind} radio is not checked:\n{}",
        state.last_body
    );
    assert!(
        !dom::matches(&state.last_body, r#"input[name="transport"][disabled]"#),
        "the selector is disabled:\n{}",
        state.last_body
    );
}

#[then(expr = "the transport selector is pinned to {string}")]
fn transport_pinned(world: &mut StarAdventurerWorld, kind: String) {
    let state = world.setup_pages();
    let css = format!(r#"input[name="transport"][value="{kind}"][checked][disabled]"#);
    assert!(
        dom::matches(&state.last_body, &css),
        "the {kind} radio is not checked and disabled:\n{}",
        state.last_body
    );
}

#[then(expr = "the page names {string} as edited in the configuration file")]
fn names_file_only(world: &mut StarAdventurerWorld, name: String) {
    let state = world.setup_pages();
    let css = format!(r#"div.file-only[id="{name}"]"#);
    assert!(
        dom::matches(&state.last_body, &css),
        "{name} is not named as file-only:\n{}",
        state.last_body
    );
    assert!(
        dom::input(&state.last_body, &name).is_none(),
        "{name} is offered as an input:\n{}",
        state.last_body
    );
}

#[then(expr = "the page reports the submitted values were rejected naming {string}")]
fn rejected_naming(world: &mut StarAdventurerWorld, needle: String) {
    let state = world.setup_pages();
    assert_eq!(state.last_status, Some(422), "{}", state.last_body);
    assert!(
        dom::text_contains(&state.last_body, "#rejected", &needle),
        "no rejection banner naming {needle:?}:\n{}",
        state.last_body
    );
}

#[when("the supported actions are queried")]
async fn query_supported_actions(world: &mut StarAdventurerWorld) {
    let mount = Arc::clone(world.mount());
    world.last_supported_actions = Some(mount.supported_actions().await.unwrap());
}

#[then(
    "the supported actions are exactly SetUnparkFromApPosition, SetPreferredApPark and UnparkFromApPosition"
)]
fn assert_only_the_ap_park_actions(world: &mut StarAdventurerWorld) {
    let actions = world
        .last_supported_actions
        .as_ref()
        .expect("no supported actions queried");
    assert_eq!(
        actions,
        &[
            "SetUnparkFromApPosition",
            "SetPreferredApPark",
            "UnparkFromApPosition",
        ]
    );
}

#[when(regex = r#"^the action "([^"]+)" is called$"#)]
async fn call_named_action(world: &mut StarAdventurerWorld, name: String) {
    let mount = Arc::clone(world.mount());
    match mount.action(name, String::new()).await {
        Ok(_) => world.last_error_code = None,
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[then("the call should fail with an action-not-implemented error")]
fn assert_action_not_implemented(world: &mut StarAdventurerWorld) {
    assert_eq!(
        world.last_error_code,
        Some(ASCOMErrorCode::ACTION_NOT_IMPLEMENTED.raw()),
        "expected ACTION_NOT_IMPLEMENTED, got {:?}",
        world.last_error_code
    );
}
