//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra`, plus the Givens that start this service in each shape, the
//! steps on its (now empty) action list, the focuser's name after a save,
//! the facts a focuser's page lists, and the server page's pinned port and
//! the overrides it leaves to the focusers' pages.

use ascom_alpaca::ASCOMErrorCode;
use bdd_infra::setup_pages::{dom, SetupPagesWorld as _};
use cucumber::{given, then, when};

use crate::world::FocuserWorld;

bdd_infra::setup_pages_steps!(FocuserWorld);

#[given(regex = r"^a running zwo-focuser service started from the configuration (\{.*\})$")]
async fn service_running_from(world: &mut FocuserWorld, config: String) {
    world.staged_config =
        Some(serde_json::from_str(&config).expect("the configuration is a JSON literal"));
    world.start().await;
}

#[given("a running zwo-focuser service with its port pinned by the command line")]
async fn service_running_with_pinned_port(world: &mut FocuserWorld) {
    world.pinned_port = Some(bdd_infra::reserved_test_port_async().await);
    world.start().await;
}

#[given("the zwo-focuser service is running with TLS and auth")]
async fn service_running_with_tls_and_auth(world: &mut FocuserWorld) {
    world.start_with_tls_and_auth_pages().await;
}

#[then(expr = "the input named {string} shows the pinned port and is disabled")]
fn input_shows_pinned_port(world: &mut FocuserWorld, name: String) {
    let port = world.pinned_port.expect("no port was pinned").to_string();
    let state = world.setup_pages();
    let input = dom::input(&state.last_body, &name)
        .unwrap_or_else(|| panic!("no input named {name:?} in:\n{}", state.last_body));
    assert_eq!(input.value, port, "input {name} value");
    assert!(input.disabled, "input {name} is not disabled");
}

#[then(expr = "the page does not name {string} as edited in the configuration file")]
fn page_does_not_name_file_only(world: &mut FocuserWorld, name: String) {
    let state = world.setup_pages();
    let css = format!(r#"div.file-only[id="{name}"]"#);
    assert!(
        !dom::matches(&state.last_body, &css),
        "{name} is named as file-only:\n{}",
        state.last_body
    );
}

#[then("the page lists the facts:")]
fn page_lists_facts(world: &mut FocuserWorld, step: &cucumber::gherkin::Step) {
    let table = step.table().expect("the facts step needs a data table");
    let state = world.setup_pages();
    let listed = |part: &str, index: usize| {
        dom::text_of(
            &state.last_body,
            &format!("dl.facts {part}:nth-of-type({index})"),
        )
    };
    for (index, row) in (1..).zip(&table.rows) {
        assert_eq!(
            (listed("dt", index), listed("dd", index)),
            (Some(row[0].clone()), Some(row[1].clone())),
            "fact {index} in:\n{}",
            state.last_body
        );
    }
    assert_eq!(
        listed("dt", table.rows.len() + 1),
        None,
        "more facts than listed in:\n{}",
        state.last_body
    );
}

#[then(regex = r#"^focuser device (\d+) reports the Name "([^"]+)"$"#)]
async fn focuser_reports_name(world: &mut FocuserWorld, _device: u32, expected: String) {
    assert_eq!(world.focuser().name().await.unwrap(), expected);
}

#[when(regex = r"^the supported actions are queried on focuser device (\d+)$")]
async fn query_supported_actions(world: &mut FocuserWorld, _device: u32) {
    let focuser = world.focuser();
    world.last_actions = Some(focuser.supported_actions().await.unwrap());
}

#[then("the supported actions are empty")]
fn supported_actions_empty(world: &mut FocuserWorld) {
    let actions = world
        .last_actions
        .as_ref()
        .expect("no supported actions stashed");
    assert_eq!(actions, &Vec::<String>::new());
}

#[when(regex = r#"^the action "([^"]+)" is called on focuser device (\d+)$"#)]
async fn call_named_action(world: &mut FocuserWorld, action: String, _device: u32) {
    match world.focuser().action(action, String::new()).await {
        Ok(_) => world.last_error_code = None,
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[then("the call should fail with an action-not-implemented error")]
fn call_action_not_implemented(world: &mut FocuserWorld) {
    assert_eq!(
        world.last_error_code,
        Some(ASCOMErrorCode::ACTION_NOT_IMPLEMENTED.raw())
    );
}
