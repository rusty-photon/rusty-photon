//! Step definitions for `config_page.feature`.
//!
//! Every scenario drives the real BFF over HTTP against a real dsd-fp2 driver
//! (see [`crate::world::UiWorld`]); the Then-steps assert on the HTML the BFF
//! actually renders using CSS-selector DOM checks (see [`crate::dom`]) — Layer A
//! of the UI-testing plan (docs/plans/archive/ui-testing.md §4).

use cucumber::{given, then, when};

use crate::world::UiWorld;
use crate::{dom, snapshot};

// --- Given: stand up the real services --------------------------------------

#[given(
    regex = r#"^a dsd-fp2 driver running with serial\.port "([^"]+)" and max_brightness (\d+)$"#
)]
async fn driver_running(world: &mut UiWorld, port: String, max_brightness: u32) {
    world.start_driver_and_bff(&port, max_brightness).await;
}

#[given(
    regex = r#"^a dsd-fp2 driver running with the serial port pinned to "([^"]+)" by a command-line override$"#
)]
async fn driver_running_with_override(world: &mut UiWorld, port: String) {
    world.start_driver_with_serial_override_and_bff(&port).await;
}

#[given("the BFF is pointed at a dsd-fp2 driver that is not running")]
async fn driver_not_running(world: &mut UiWorld) {
    world.start_bff_with_unreachable_driver().await;
}

#[given("the driver's bound port is pinned so a reload keeps the same address")]
async fn pin_driver_port(world: &mut UiWorld) {
    world.pin_driver_port().await;
}

// --- When: interact with the BFF --------------------------------------------

#[when("I open the dsd-fp2 config page")]
async fn open_page(world: &mut UiWorld) {
    let path = UiWorld::device_config_path();
    world.get(&path).await;
}

#[when(regex = r"^I open the dsd-fp2 config page with ([\w.]+) unlocked$")]
async fn open_page_with_unlock(world: &mut UiWorld, field: String) {
    // Follow the page's rendered "Unlock to edit" affordance the way htmx would,
    // rather than fabricating the `?unlock=` URL out of band.
    world.open_with_unlock(&field).await;
}

#[when("I open the configuration index")]
async fn open_index(world: &mut UiWorld) {
    world.get("/").await;
}

// The service key is the reserved `rp` or a roster-derived `rp:{kind}:{id}`
// key — hence the `:` in the class.
#[when(regex = r#"^I open the config page for "([\w:-]+)"$"#)]
async fn open_named_config_page(world: &mut UiWorld, service: String) {
    world.get(&format!("/config/{service}")).await;
}

#[when(regex = r"^I submit the config form setting max_brightness to (\d+)$")]
async fn submit_max_brightness(world: &mut UiWorld, value: String) {
    world
        .submit_form(&[("cover_calibrator.max_brightness", &value)])
        .await;
}

#[when(regex = r"^I submit the config form setting baud_rate to (\d+)$")]
async fn submit_baud_rate(world: &mut UiWorld, value: String) {
    world.submit_form(&[("serial.baud_rate", &value)]).await;
}

#[when("I submit the config form without changing anything")]
async fn submit_unchanged(world: &mut UiWorld) {
    world.submit_form(&[]).await;
}

#[when(regex = r"^I poll the reconnect status until max_brightness (\d+) is served$")]
async fn poll_until_served(world: &mut UiWorld, value: String) {
    world
        .poll_status_until_value("cover_calibrator.max_brightness", &value)
        .await;
}

// --- Then: assert on the rendered HTML --------------------------------------

#[then(regex = r#"^the page shows the value "([^"]+)"$"#)]
fn page_shows_value(world: &mut UiWorld, expected: String) {
    // An input is *rendered* with this exact value — stricter than a substring,
    // which would also match the value buried inside the hidden `__config` blob.
    let css = format!(r#"input[value="{expected}"]"#);
    assert!(
        dom::matches(&world.last_body, &css),
        "expected an input rendered with value {expected:?}, body was:\n{}",
        world.last_body
    );
}

#[then(regex = r"^the ([\w.]+) field is disabled$")]
fn field_disabled(world: &mut UiWorld, field: String) {
    let input = dom::input(&world.last_body, &field)
        .unwrap_or_else(|| panic!("no input named {field:?} in:\n{}", world.last_body));
    assert!(input.disabled, "{field} should be disabled but is editable");
}

#[then(regex = r"^the ([\w.]+) field is editable$")]
fn field_editable(world: &mut UiWorld, field: String) {
    let input = dom::input(&world.last_body, &field)
        .unwrap_or_else(|| panic!("no input named {field:?} in:\n{}", world.last_body));
    assert!(
        !input.disabled,
        "{field} should be editable but is disabled"
    );
}

#[then(regex = r"^the page offers to unlock ([\w.]+) to edit it$")]
fn offers_to_unlock(world: &mut UiWorld, field: String) {
    // The identity hint plus an htmx unlock link to `?unlock=<field>`.
    assert!(
        dom::text_contains(
            &world.last_body,
            ".field .hint",
            "Identity — the driver owns this"
        ),
        "missing identity hint:\n{}",
        world.last_body
    );
    assert!(
        dom::unlock_url(&world.last_body, &field).is_some(),
        "missing unlock affordance for {field}:\n{}",
        world.last_body
    );
}

#[then("the page explains the field is pinned by a command-line override")]
fn explains_pinned(world: &mut UiWorld) {
    assert!(
        dom::text_contains(
            &world.last_body,
            ".field .hint",
            "Pinned by a command-line override"
        ),
        "missing pinned explanation:\n{}",
        world.last_body
    );
}

#[then("the page reports the driver is reloading")]
fn reports_reloading(world: &mut UiWorld) {
    assert!(
        dom::matches(&world.last_body, "div.banner.applying"),
        "missing reloading banner:\n{}",
        world.last_body
    );
}

#[then("the page polls the device's status route every 1s for reconnection")]
fn polls_for_reconnection(world: &mut UiWorld) {
    let status_path = format!("{}/status", UiWorld::device_config_path());
    assert_eq!(
        dom::attr(&world.last_body, "#config-card", "hx-get").as_deref(),
        Some(status_path.as_str()),
        "reconnecting card should poll the status route:\n{}",
        world.last_body
    );
    assert_eq!(
        dom::attr(&world.last_body, "#config-card", "hx-trigger").as_deref(),
        Some("every 1s"),
        "reconnecting card should poll every 1s:\n{}",
        world.last_body
    );
}

#[then("the page reports the configuration was saved without a reload")]
fn reports_saved_no_reload(world: &mut UiWorld) {
    assert!(
        dom::text_contains(&world.last_body, "div.banner.ok", "No reload was needed"),
        "missing saved-without-reload banner:\n{}",
        world.last_body
    );
}

#[then(regex = r#"^the form shows the validation error "([^"]+)" on serial\.baud_rate$"#)]
fn shows_validation_error(world: &mut UiWorld, message: String) {
    // The error text appears inside the serial.baud_rate field's own wrapper,
    // and that wrapper carries the `invalid` class — all three tied together.
    assert!(
        dom::field_error(&world.last_body, "serial.baud_rate", &message),
        "expected validation error {message:?} on an invalid serial.baud_rate field:\n{}",
        world.last_body
    );
}

#[then(regex = r"^the submitted baud_rate value (\d+) is preserved$")]
fn baud_rate_preserved(world: &mut UiWorld, value: String) {
    let input = dom::input(&world.last_body, "serial.baud_rate")
        .unwrap_or_else(|| panic!("no serial.baud_rate input:\n{}", world.last_body));
    assert_eq!(input.value, value, "submitted baud_rate not preserved");
}

#[then("the page shows a driver error")]
fn shows_driver_error(world: &mut UiWorld) {
    assert!(
        dom::text_contains(
            &world.last_body,
            "div.banner.error",
            "could not reach the driver"
        ),
        "missing driver error banner:\n{}",
        world.last_body
    );
}

// --- Then (P2): byte-equivalence snapshot of the rendered output ------------

/// Capture the last response's exact bytes as a committed golden (Layer B / P2).
/// Rides alongside the P1 DOM assertions on the same captured output; the golden
/// is the cross-OS-comparable artifact (see [`crate::snapshot`]).
#[then(regex = r#"^the rendered output matches the "([\w-]+)" snapshot$"#)]
fn matches_snapshot(world: &mut UiWorld, name: String) {
    snapshot::assert_html(&name, &world.last_body);
}
