//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra` — the pages' and the camera drivers' Cameras section — plus
//! the Given that starts this service behind TLS and auth, the steps on its
//! (now empty) action list, and the ones on the filter wheels this driver's
//! entries declare: the Cameras section's wheel column, the confirmation of
//! a wheel's move, and each wheel's page, which edits its filter names.

use ascom_alpaca::ASCOMErrorCode;
use bdd_infra::setup_pages::{self, dom, SetupPagesWorld as _};
use cucumber::{given, then, when};

use crate::world::{registered_devices, CameraWorld};

bdd_infra::setup_pages_steps!(CameraWorld);
bdd_infra::usb_devices_editor_steps!(CameraWorld);

/// A wheel page's filter names control.
const WHEEL_NAMES: &str = "wheel.filter_names";

/// The filter names a step lists, `L, R, G`, as the wheel page's box holds
/// them: one per line. A blank between two commas is a blank slot.
fn lines(names: &str) -> String {
    names
        .split(',')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
}

#[given("the qhy-camera service is running with TLS and auth")]
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

#[then(
    regex = r"^the running service registers (\d+) Camera devices? and (\d+) FilterWheel devices?$"
)]
async fn running_service_registers(world: &mut CameraWorld, cameras: usize, wheels: usize) {
    let (served, served_wheels) = registered_devices(world.port())
        .await
        .expect("the management API answers");
    assert_eq!(
        (served.len(), served_wheels.len()),
        (cameras, wheels),
        "the running service registers {} Camera and {} FilterWheel devices",
        served.len(),
        served_wheels.len()
    );
}

// --- the Cameras section's wheel column --------------------------------------------

/// Submit the server page with the wheel box of `port`'s row set to `on`.
async fn submit_wheel(world: &mut CameraWorld, port: &str, on: bool) {
    setup_pages::submit_with(
        world.setup_pages(),
        setup_pages::SERVER_PATH,
        None,
        |html, controls| {
            let index = dom::row_index(html, port)
                .unwrap_or_else(|| panic!("no row for {port} in:\n{html}"));
            let wheel = format!("usb_devices.{index}.wheel");
            assert_eq!(
                controls.iter().any(|(k, _)| *k == wheel),
                !on,
                "the wheel box of the row for {port} is already as asked:\n{html}"
            );
            controls.retain(|(k, _)| *k != wheel);
            if on {
                controls.push((wheel, "on".to_string()));
            }
        },
    )
    .await;
}

#[when(expr = "I submit the server setup page checking the wheel for port {string}")]
async fn check_wheel(world: &mut CameraWorld, port: String) {
    submit_wheel(world, &port, true).await;
}

#[when(expr = "I submit the server setup page unchecking the wheel for port {string}")]
async fn uncheck_wheel(world: &mut CameraWorld, port: String) {
    submit_wheel(world, &port, false).await;
}

/// The CSS of what the Cameras section says is served as the wheel on
/// `port`'s camera.
fn wheel_serves(port: &str) -> String {
    format!(r#"tr[data-port="{port}"] .wheel-serves"#)
}

#[then(expr = "the Cameras section lists the wheel on port {string} as {string}")]
fn wheel_listed(world: &mut CameraWorld, port: String, serves: String) {
    let state = world.setup_pages();
    assert_eq!(
        dom::text_of(&state.last_body, &wheel_serves(&port)),
        Some(serves),
        "{}",
        state.last_body
    );
}

#[then(expr = "after the reload the Cameras section lists the wheel on port {string} as {string}")]
async fn wheel_listed_after_reload(world: &mut CameraWorld, port: String, serves: String) {
    let css = wheel_serves(&port);
    setup_pages::wait_until(
        world.setup_pages(),
        setup_pages::SERVER_PATH,
        &format!("the wheel on {port} as {serves:?}"),
        |body| dom::text_of(body, &css).as_deref() == Some(serves.as_str()),
    )
    .await;
}

#[then(
    expr = "the page asks to confirm that the filter wheel on {string} moves from {int} to {int}"
)]
fn confirm_wheel_move(world: &mut CameraWorld, port: String, from: u32, to: u32) {
    let state = world.setup_pages();
    assert_eq!(state.last_status, Some(409), "{}", state.last_body);
    let css = format!(
        r#"#confirm li.wheel-move[data-port="{port}"][data-from="{from}"][data-to="{to}"]"#
    );
    assert!(
        dom::matches(&state.last_body, &css),
        "no move of the wheel on {port} from {from} to {to}:\n{}",
        state.last_body
    );
}

#[then("the page asks to confirm that the reload numbers the wheels by what its probe finds")]
fn confirm_wheels_by_probe(world: &mut CameraWorld) {
    let state = world.setup_pages();
    assert_eq!(state.last_status, Some(409), "{}", state.last_body);
    assert!(
        dom::matches(&state.last_body, "#confirm p.wheels-by-probe"),
        "no word of the CFW probe:\n{}",
        state.last_body
    );
}

#[then("the page warns that the rp filter wheel entries bound to those numbers must be renumbered")]
fn confirm_warns_wheels(world: &mut CameraWorld) {
    let state = world.setup_pages();
    assert!(
        dom::text_contains(&state.last_body, "#confirm", "filter wheel entries"),
        "no renumbering warning for the wheels:\n{}",
        state.last_body
    );
}

// --- a wheel's page ------------------------------------------------------------------

#[when(expr = "I submit the setup page of filter wheel {int} naming the filters {string}")]
async fn submit_filter_names(world: &mut CameraWorld, number: u32, names: String) {
    let path = setup_pages::device_path("filterwheel", number);
    let names = lines(&names);
    setup_pages::submit(world.setup_pages(), &path, &[(WHEEL_NAMES, &names)], None).await;
}

#[then(expr = "the page shows the filters {string}")]
fn page_shows_filters(world: &mut CameraWorld, names: String) {
    let state = world.setup_pages();
    assert_eq!(
        dom::textarea(&state.last_body, WHEEL_NAMES),
        Some(lines(&names)),
        "{}",
        state.last_body
    );
}

#[then(expr = "after the reload the setup page of filter wheel {int} shows the filters {string}")]
async fn wheel_page_shows_after_reload(world: &mut CameraWorld, number: u32, names: String) {
    let path = setup_pages::device_path("filterwheel", number);
    let expected = lines(&names);
    setup_pages::wait_until(
        world.setup_pages(),
        &path,
        &format!("the filters {names:?}"),
        |body| dom::textarea(body, WHEEL_NAMES).as_deref() == Some(expected.as_str()),
    )
    .await;
}
