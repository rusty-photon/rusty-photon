//! Steps for `equipment_page.feature` — the roster view, capability tiers,
//! and the add/edit/remove config-surgery flows against a real rp.

use cucumber::gherkin::Step;
use cucumber::{given, then, when};

use crate::dom;
use crate::world::UiWorld;

#[given(
    regex = r#"^a running dsd-fp2 driver registered in rp's roster as cover calibrator "([\w-]+)"$"#
)]
async fn driver_in_rp_roster(world: &mut UiWorld, id: String) {
    world.start_panel_and_rp_with_cover_calibrator(&id).await;
}

#[given(
    regex = r#"^a running dsd-fp2 driver with TLS and auth registered in rp's roster as cover calibrator "([\w-]+)"$"#
)]
async fn auth_driver_in_rp_roster(world: &mut UiWorld, id: String) {
    world
        .start_panel_with_tls_auth_and_rp_with_cover_calibrator(&id)
        .await;
}

#[given(
    regex = r#"^a running qhy-focuser driver registered in rp's roster as focuser "([\w-]+)"$"#
)]
async fn focuser_in_rp_roster(world: &mut UiWorld, id: String) {
    world.start_driver_and_rp_with_focuser(&id).await;
}

#[when("I open the equipment page")]
async fn open_equipment(world: &mut UiWorld) {
    world.get("/equipment").await;
}

#[when(regex = r#"^I open the equipment page as the host "([^"]+)"$"#)]
async fn open_equipment_as_host(world: &mut UiWorld, host: String) {
    world.get_as_host("/equipment", &host).await;
}

#[when(regex = r#"^I open the add-equipment form for "([\w_]+)"$"#)]
async fn open_add_form(world: &mut UiWorld, kind: String) {
    world.get(&format!("/equipment/{kind}/new")).await;
}

#[when(regex = r#"^I open the edit-equipment form for cover calibrator "([\w-]+)"$"#)]
async fn open_edit_form(world: &mut UiWorld, id: String) {
    world
        .get(&format!("/equipment/cover_calibrators/{id}/edit"))
        .await;
}

/// Submit the currently-rendered equipment form with the table's field edits.
#[when("I submit the equipment form with:")]
async fn submit_equipment_form(world: &mut UiWorld, step: &Step) {
    let table = step.table.as_ref().expect("step needs a table");
    let changes: Vec<(String, String)> = table
        .rows
        .iter()
        .skip(1) // header row: | field | value |
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect();
    let refs: Vec<(&str, &str)> = changes
        .iter()
        .map(|(f, v)| (f.as_str(), v.as_str()))
        .collect();
    world.submit_rendered_form(&refs).await;
}

/// Submit the currently-rendered equipment form with the table's field edits
/// plus a checkbox group: one submitted pair per checked value, sharing the
/// group's field name (the multi-value shape a browser posts).
#[when(regex = r#"^I submit the equipment form checking "([\w_]+)" values "([^"]+)" with:$"#)]
async fn submit_equipment_form_with_checked(
    world: &mut UiWorld,
    name: String,
    values: String,
    step: &Step,
) {
    let table = step.table.as_ref().expect("step needs a table");
    let changes: Vec<(String, String)> = table
        .rows
        .iter()
        .skip(1) // header row: | field | value |
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect();
    let refs: Vec<(&str, &str)> = changes
        .iter()
        .map(|(f, v)| (f.as_str(), v.as_str()))
        .collect();
    let checked: Vec<&str> = values.split(',').map(str::trim).collect();
    world
        .submit_rendered_form_multi(&refs, (&name, &checked))
        .await;
}

#[when(regex = r#"^I remove cover calibrator "([\w-]+)" from the roster$"#)]
async fn remove_entry(world: &mut UiWorld, id: String) {
    // The Remove affordance is an htmx button (hx-post + hx-confirm); drive
    // the POST it would issue after the operator confirms.
    world
        .post_htmx(&format!("/equipment/cover_calibrators/{id}/delete"))
        .await;
}

#[then(regex = r#"^the roster section "([^"]+)" lists "([\w-]+)"$"#)]
fn section_lists(world: &mut UiWorld, section: String, id: String) {
    let kind_id = section_css(&section);
    assert!(
        dom::text_contains(&world.last_body, &format!("{kind_id} .svc-id"), &id),
        "section {section} does not list {id}:\n{}",
        world.last_body
    );
}

// The roster shows what rp is RUNNING; a just-persisted entry appears only
// after rp's next start (the restart callout says so).
#[then(regex = r#"^the roster section "([^"]+)" does not yet list "([\w-]+)"$"#)]
fn section_does_not_list(world: &mut UiWorld, section: String, id: String) {
    let kind_id = section_css(&section);
    assert!(
        !dom::text_contains(&world.last_body, &format!("{kind_id} .svc-id"), &id),
        "section {section} unexpectedly lists {id} before an rp restart:\n{}",
        world.last_body
    );
}

#[then(regex = r#"^the roster section "([^"]+)" is empty$"#)]
fn section_empty(world: &mut UiWorld, section: String) {
    let kind_id = section_css(&section);
    assert!(
        dom::matches(&world.last_body, &format!("{kind_id} .empty-kind")),
        "section {section} is not empty:\n{}",
        world.last_body
    );
}

#[then(regex = r#"^the roster row for "([\w-]+)" shows a connected LED$"#)]
fn row_connected(world: &mut UiWorld, id: String) {
    assert!(
        dom::matches(&world.last_body, &row_css(&id, ".led.ok")),
        "no connected LED for {id}:\n{}",
        world.last_body
    );
}

#[then(regex = r#"^the roster row for "([\w-]+)" shows an unknown LED$"#)]
fn row_unknown(world: &mut UiWorld, id: String) {
    assert!(
        dom::matches(&world.last_body, &row_css(&id, ".led.unknown")),
        "no unknown LED for {id}:\n{}",
        world.last_body
    );
}

#[then(regex = r#"^the roster row for "([\w-]+)" carries the "([\w]+)" tier$"#)]
fn row_tier(world: &mut UiWorld, id: String, tier: String) {
    assert!(
        dom::matches(
            &world.last_body,
            &row_css(&id, &format!(".tier-badge.{tier}"))
        ),
        "row {id} does not carry tier {tier}:\n{}",
        world.last_body
    );
}

#[then(
    regex = r#"^the roster row for "([\w-]+)" links to the driver's path "([^"]+)" at the driver's port$"#
)]
fn row_links_to_driver_path(world: &mut UiWorld, id: String, path: String) {
    let expected = format!(
        "{}://127.0.0.1:{}{path}",
        world.panel_scheme(),
        world.panel_port()
    );
    let href = dom::attr(&world.last_body, &row_css(&id, "a.setup-link"), "href");
    assert_eq!(
        href.as_deref(),
        Some(expected.as_str()),
        "setup-page link for {id}:\n{}",
        world.last_body
    );
}

#[then(regex = r#"^the roster row for "([\w-]+)" links to "([^"]+)" at the driver's port$"#)]
fn row_links_to_host(world: &mut UiWorld, id: String, origin: String) {
    let expected = format!("{origin}:{}/", world.panel_port());
    let href = dom::attr(&world.last_body, &row_css(&id, "a.setup-link"), "href")
        .unwrap_or_else(|| panic!("no setup-page link for {id}:\n{}", world.last_body));
    assert!(
        href.starts_with(&expected),
        "setup-page link {href} for {id} does not start with {expected}"
    );
}

#[then(regex = r#"^the equipment form shows a problem mentioning "([^"]+)"$"#)]
fn form_problem(world: &mut UiWorld, needle: String) {
    assert!(
        dom::text_contains(&world.last_body, ".banner.error", &needle),
        "no problem banner mentioning {needle:?}:\n{}",
        world.last_body
    );
}

#[then(regex = r#"^the form offers a "([\w_]+)" checkbox for value "(-?\d+)"$"#)]
fn form_offers_checkbox(world: &mut UiWorld, name: String, value: String) {
    assert!(
        dom::has_checkbox(&world.last_body, &name, &value),
        "no checkbox named {name:?} with value {value:?} in:\n{}",
        world.last_body
    );
}

#[then(regex = r#"^rp's config file JSON at "([^"]+)" equals "([^"]+)"$"#)]
fn rp_config_json_at(world: &mut UiWorld, pointer: String, expected: String) {
    let on_disk = world.rp_config_on_disk();
    let expected: serde_json::Value =
        serde_json::from_str(&expected).expect("the expected value must be JSON");
    assert_eq!(
        on_disk.pointer(&pointer),
        Some(&expected),
        "config file JSON at {pointer}:\n{on_disk}"
    );
}

#[then(regex = r#"^rp's config file on disk contains the string "([^"]+)" exactly once$"#)]
fn rp_config_contains_once(world: &mut UiWorld, needle: String) {
    let on_disk = world.rp_config_on_disk().to_string();
    let count = on_disk.matches(&needle).count();
    assert_eq!(
        count, 1,
        "expected exactly one {needle:?}, found {count}: {on_disk}"
    );
}

/// CSS id of a roster kind section from its display heading.
fn section_css(section: &str) -> String {
    let key = section.to_lowercase().replace(' ', "_");
    format!("#kind-{key}")
}

/// CSS selector scoped to one roster row (rows carry `id="row-{kind}-{id}"`;
/// the id is unique enough across kinds for these scenarios' asserts).
fn row_css(id: &str, suffix: &str) -> String {
    format!("li[id$=\"-{id}\"] {suffix}")
}
