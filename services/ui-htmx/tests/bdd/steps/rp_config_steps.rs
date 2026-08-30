//! Steps for `rp_config_page.feature` — the `/config/rp` page over rp's
//! plain-REST config API, including the restart callout and on-disk asserts.

use cucumber::{given, then, when};

use crate::dom;
use crate::world::UiWorld;

#[given("a running rp orchestrator with an empty roster")]
async fn running_rp_empty_roster(world: &mut UiWorld) {
    world.start_rp_with_empty_roster().await;
}

#[given("a BFF pointed at rp")]
async fn bff_pointed_at_rp(world: &mut UiWorld) {
    world.start_bff_with_rp().await;
}

#[given("a BFF pointed at an unreachable rp")]
async fn bff_unreachable_rp(world: &mut UiWorld) {
    world.start_bff_with_unreachable_rp().await;
}

#[when(regex = r#"^I submit the rp form with "([\w.]+)" set to "([^"]*)"$"#)]
async fn submit_rp_form(world: &mut UiWorld, field: String, value: String) {
    world
        .submit_form_at("/config/rp", &[(&field, &value)])
        .await;
}

#[then(regex = r#"^the page shows an input named "([\w.]+)" with value "([^"]*)"$"#)]
fn input_with_value(world: &mut UiWorld, name: String, value: String) {
    let input = dom::input(&world.last_body, &name)
        .unwrap_or_else(|| panic!("no input named {name}:\n{}", world.last_body));
    assert_eq!(input.value, value, "input {name}");
}

#[then(regex = r#"^the input named "([\w.]+)" is disabled$"#)]
fn input_disabled(world: &mut UiWorld, name: String) {
    let input = dom::input(&world.last_body, &name)
        .unwrap_or_else(|| panic!("no input named {name}:\n{}", world.last_body));
    assert!(input.disabled, "input {name} unexpectedly enabled");
}

#[then("the page reports the changes take effect when rp is restarted")]
fn restart_callout_shown(world: &mut UiWorld) {
    assert!(
        dom::text_contains(&world.last_body, ".banner.warn", "when rp is restarted"),
        "no restart callout:\n{}",
        world.last_body
    );
}

#[then(regex = r#"^the restart callout lists "([\w.]+)"$"#)]
fn restart_callout_lists(world: &mut UiWorld, path: String) {
    assert!(
        dom::text_contains(&world.last_body, ".banner.warn", &path),
        "restart callout does not list {path}:\n{}",
        world.last_body
    );
}

#[then(regex = r#"^rp's config file on disk contains the string "([^"]+)"$"#)]
fn rp_config_contains(world: &mut UiWorld, needle: String) {
    let on_disk = world.rp_config_on_disk().to_string();
    assert!(
        on_disk.contains(&needle),
        "rp config lacks {needle}: {on_disk}"
    );
}

#[then(regex = r#"^rp's config file on disk leaves "([\w.]+)" unset$"#)]
fn rp_config_unset_at(world: &mut UiWorld, path: String) {
    let on_disk = world.rp_config_on_disk();
    let pointer = format!("/{}", path.replace('.', "/"));
    // Unset is the *absent* key, and nothing weaker: rp's optional config
    // fields are `skip_serializing_if`, so a cleared field leaves no key
    // behind (ADR-016 amendment 8). An explicit null would read back as
    // unset, but accepting one here would let a regression that drops the
    // attribute — putting `"file_naming_pattern": null` back in the
    // operator's file — pass this scenario unnoticed.
    assert!(
        on_disk.pointer(&pointer).is_none(),
        "{path} is present in rp's config on disk; unset is spelled by the \
         key's absence: {on_disk}",
    );
}

#[then(regex = r#"^rp's config file on disk does not contain the string "([^"]+)"$"#)]
fn rp_config_lacks(world: &mut UiWorld, needle: String) {
    let on_disk = world.rp_config_on_disk().to_string();
    assert!(
        !on_disk.contains(&needle),
        "rp config unexpectedly contains {needle}: {on_disk}"
    );
}

#[then(regex = r#"^the field "([\w.]+)" shows an error mentioning "([^"]+)"$"#)]
fn field_shows_error(world: &mut UiWorld, field: String, needle: String) {
    assert!(
        dom::field_error(&world.last_body, &field, &needle),
        "no error mentioning {needle:?} on {field}:\n{}",
        world.last_body
    );
}

#[then(regex = r#"^the page shows an error banner mentioning "([^"]+)"$"#)]
fn error_banner(world: &mut UiWorld, needle: String) {
    assert!(
        dom::text_contains(&world.last_body, ".banner.error", &needle),
        "no error banner mentioning {needle:?}:\n{}",
        world.last_body
    );
}
