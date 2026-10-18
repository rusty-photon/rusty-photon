//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra`, plus the Given that starts this service behind TLS and auth,
//! the hand edit that puts a rule into a running service's file, and the
//! steps on its empty action list.

use std::sync::Arc;

use ascom_alpaca::ASCOMErrorCode;
use cucumber::{given, then, when};

use crate::world::FilemonitorWorld;

bdd_infra::setup_pages_steps!(FilemonitorWorld);

#[given("filemonitor is running with TLS and auth")]
async fn running_with_tls_and_auth(world: &mut FilemonitorWorld) {
    world.start_with_tls_and_auth_pages().await;
}

/// Rewrites the running service's configuration file so its one parsing
/// rule is a regex rule with `pattern`. The load refuses a rule that does
/// not compile, so a hand edit is the only way such a rule reaches the
/// page, which reads the file fresh on every save.
#[given(
    expr = "the configuration file is edited by hand to hold a regex rule with pattern {string}"
)]
fn edit_file_to_hold_regex_rule(world: &mut FilemonitorWorld, pattern: String) {
    let path = world
        .setup_pages
        .config_path
        .as_ref()
        .expect("the service is not running");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("failed to read config"))
            .expect("the config is not JSON");
    config["parsing"]["rules"] = serde_json::json!([{
        "type": "regex",
        "pattern": pattern,
        "safe": true,
    }]);
    std::fs::write(path, config.to_string()).expect("failed to write config");
}

#[when("the supported actions are queried")]
async fn query_supported_actions(world: &mut FilemonitorWorld) {
    let monitor = Arc::clone(world.monitor());
    world.last_supported_actions = Some(monitor.supported_actions().await.unwrap());
}

#[then("the supported actions are empty")]
fn assert_supported_actions_empty(world: &mut FilemonitorWorld) {
    let actions = world
        .last_supported_actions
        .as_ref()
        .expect("no supported actions queried");
    assert_eq!(actions, &Vec::<String>::new());
}

#[when(regex = r#"^the action "([^"]+)" is called$"#)]
async fn call_named_action(world: &mut FilemonitorWorld, name: String) {
    let monitor = Arc::clone(world.monitor());
    world.last_ascom_error = monitor.action(name, String::new()).await.err();
}

#[then("the call should fail with an action-not-implemented error")]
fn assert_action_not_implemented(world: &mut FilemonitorWorld) {
    let err = world
        .last_ascom_error
        .take()
        .expect("expected an error from the call");
    assert_eq!(err.code, ASCOMErrorCode::ACTION_NOT_IMPLEMENTED);
}
