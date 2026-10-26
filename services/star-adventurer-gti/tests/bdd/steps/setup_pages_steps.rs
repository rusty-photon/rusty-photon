//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra`, the Givens that start this service in each shape, the steps
//! on the transport selector, the recovery page's Stop and recovery, the steps
//! on the mount's (empty) action list, and the reload steps
//! `connection_lifecycle.feature` and `unpark_from_ap_position.feature`
//! share: a save on the mount's page is how a scenario reloads the driver.

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

/// The recovery, at the mount's page's path with `?recover`.
fn recovery_page() -> String {
    format!("{}?recover", mount_page())
}

/// Open the recovery page and post one of its forms, `pairs`, from
/// `origin` (the service's own when `None`). The page carries two forms,
/// the Stop and the recovery, so the controls sent are that form's alone.
async fn post_recovery_form(
    world: &mut StarAdventurerWorld,
    pairs: Vec<(String, String)>,
    origin: Option<&str>,
) {
    setup_pages::submit_with(
        world.setup_pages(),
        &recovery_page(),
        origin,
        |_, controls| *controls = pairs,
    )
    .await;
}

/// Submit the recovery form from `origin`, its pose set to `pose` and its
/// box ticked when `confirm`.
async fn submit_recovery(
    world: &mut StarAdventurerWorld,
    pose: &str,
    confirm: bool,
    origin: Option<&str>,
) {
    let mut pairs = vec![
        ("command".to_string(), "recover".to_string()),
        ("pose".to_string(), pose.to_string()),
    ];
    if confirm {
        pairs.push(("confirm".to_string(), "on".to_string()));
    }
    post_recovery_form(world, pairs, origin).await;
}

/// Press the recovery page's Stop from `origin`.
async fn press_stop(world: &mut StarAdventurerWorld, origin: Option<&str>) {
    let pairs = vec![("command".to_string(), "stop".to_string())];
    post_recovery_form(world, pairs, origin).await;
}

#[when("I open the mount's recovery page")]
async fn open_recovery(world: &mut StarAdventurerWorld) {
    setup_pages::get(world.setup_pages(), &recovery_page(), true).await;
}

#[when(expr = "I recover the mount from {string} on its setup page, confirming it is there")]
async fn recover_confirmed(world: &mut StarAdventurerWorld, pose: String) {
    submit_recovery(world, &pose, true, None).await;
}

#[when(expr = "I recover the mount from {string} on its setup page without ticking the box")]
async fn recover_unconfirmed(world: &mut StarAdventurerWorld, pose: String) {
    submit_recovery(world, &pose, false, None).await;
}

#[when(expr = "I recover the mount from {string} on its setup page from the origin {string}")]
async fn recover_from_origin(world: &mut StarAdventurerWorld, pose: String, origin: String) {
    submit_recovery(world, &pose, true, Some(&origin)).await;
}

#[when("I press Stop on the mount's recovery page")]
async fn stop_on_page(world: &mut StarAdventurerWorld) {
    press_stop(world, None).await;
}

#[when(expr = "I press Stop on the mount's recovery page from the origin {string}")]
async fn stop_on_page_from_origin(world: &mut StarAdventurerWorld, origin: String) {
    press_stop(world, Some(&origin)).await;
}

#[then("the page links to the mount's recovery")]
fn links_to_recovery(world: &mut StarAdventurerWorld) {
    let state = world.setup_pages();
    let hrefs = dom::hrefs(&state.last_body, "a");
    assert!(
        hrefs.iter().any(|href| href == "?recover"),
        "no link to the recovery among {hrefs:?}:\n{}",
        state.last_body
    );
}

/// The reload `unpark_from_ap_position.feature` needs: pin the bound port,
/// then save a new power-up pose on the mount's page.
#[when(expr = "the mount's setup page saves the power-up pose {string}")]
async fn save_power_up_pose(world: &mut StarAdventurerWorld, pose: String) {
    save_mount_page(world, &[("mount.unpark_from_ap_position", &pose)]).await;
}

/// Also a `When`: the barrier that the reload has landed.
#[when(expr = "the reloaded service serves the power-up pose {string}")]
#[then(expr = "the reloaded service serves the power-up pose {string}")]
async fn reloaded_serves_power_up_pose(world: &mut StarAdventurerWorld, pose: String) {
    let what = format!("mount.unpark_from_ap_position={pose:?}");
    setup_pages::wait_until(world.setup_pages(), &mount_page(), &what, |body| {
        dom::select(body, "mount.unpark_from_ap_position")
            .is_some_and(|select| select.chosen.as_deref() == Some(pose.as_str()))
    })
    .await;
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

#[when("the supported actions are queried")]
async fn query_supported_actions(world: &mut StarAdventurerWorld) {
    let mount = Arc::clone(world.mount());
    world.last_supported_actions = Some(mount.supported_actions().await.unwrap());
}

#[then("the supported actions are empty")]
fn assert_no_actions(world: &mut StarAdventurerWorld) {
    let actions = world
        .last_supported_actions
        .as_ref()
        .expect("no supported actions queried");
    assert!(actions.is_empty(), "SupportedActions lists {actions:?}");
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
