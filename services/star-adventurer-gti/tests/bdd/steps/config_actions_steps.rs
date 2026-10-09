//! Step definitions for `config_actions.feature`.
//!
//! Reuses the `Given a running star-adventurer service` step from
//! `connection_steps`; the spawned binary serves the actions through its
//! `run_with_reload` loop.

use std::sync::Arc;

use ascom_alpaca::ASCOMErrorCode;
use cucumber::{then, when};

use crate::world::StarAdventurerWorld;

#[when("the supported actions are queried")]
async fn query_supported_actions(world: &mut StarAdventurerWorld) {
    let mount = Arc::clone(world.mount());
    world.last_supported_actions = Some(mount.supported_actions().await.unwrap());
}

#[then("the supported actions should include config.get, config.apply, and config.schema")]
async fn assert_config_actions(world: &mut StarAdventurerWorld) {
    let actions = world
        .last_supported_actions
        .as_ref()
        .expect("no supported actions queried");
    for expected in ["config.get", "config.apply", "config.schema"] {
        assert!(
            actions.iter().any(|a| a == expected),
            "{expected} missing from {actions:?}"
        );
    }
}

#[then("the supported actions should still include SetPreferredApPark")]
async fn assert_appark_action(world: &mut StarAdventurerWorld) {
    let actions = world
        .last_supported_actions
        .as_ref()
        .expect("no supported actions queried");
    assert!(
        actions.iter().any(|a| a == "SetPreferredApPark"),
        "SetPreferredApPark missing from {actions:?}"
    );
}

#[when("config.schema is called")]
async fn call_config_schema(world: &mut StarAdventurerWorld) {
    let mount = Arc::clone(world.mount());
    let body = mount
        .action("config.schema".to_string(), String::new())
        .await
        .expect("config.schema failed");
    world.last_response =
        Some(serde_json::from_str(&body).expect("config.schema returned invalid JSON"));
}

#[then("the schema should describe the transport, server, and mount sections")]
async fn assert_schema_sections(world: &mut StarAdventurerWorld) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let props = response["schema"]["properties"]
        .as_object()
        .expect("schema.properties is not an object");
    for section in ["transport", "server", "mount"] {
        assert!(
            props.contains_key(section),
            "schema is missing section {section}: {props:?}"
        );
    }
}

#[then("the schema should mark mount.unique_id as a locked field")]
async fn assert_schema_locked_field(world: &mut StarAdventurerWorld) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let locked = response["locked_fields"]
        .as_array()
        .expect("`locked_fields` is not an array");
    assert!(
        locked.iter().any(|f| f.as_str() == Some("mount.unique_id")),
        "locked_fields {locked:?} does not include mount.unique_id"
    );
}

#[then("the schema should mark transport.kind as a read-only field")]
async fn assert_schema_read_only_field(world: &mut StarAdventurerWorld) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let read_only = response["read_only_fields"]
        .as_array()
        .expect("`read_only_fields` is not an array");
    assert!(
        read_only
            .iter()
            .any(|f| f.as_str() == Some("transport.kind")),
        "read_only_fields {read_only:?} does not include transport.kind"
    );
}

#[when("config.get is called")]
async fn call_config_get(world: &mut StarAdventurerWorld) {
    world.current_config().await;
}

#[then("the config should report no overrides")]
async fn assert_no_overrides(world: &mut StarAdventurerWorld) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let overrides = response["overrides"]
        .as_array()
        .expect("`overrides` is not an array");
    assert!(
        overrides.is_empty(),
        "expected no overrides, got {overrides:?}"
    );
}

#[when(
    regex = r#"^config\.apply pins the bound port and sets the mount description to "([^"]+)"$"#
)]
async fn apply_pin_port_and_description(world: &mut StarAdventurerWorld, description: String) {
    let port = world.bound_port();
    let mut config = world.current_config().await;
    config["server"]["port"] = serde_json::json!(port);
    config["mount"]["description"] = serde_json::json!(description);
    world.call_config_apply(config).await;
}

#[when(
    regex = r#"^config\.apply pins the bound port, sets the mount description to "([^"]+)" and the post-slew settle to (\d+) seconds$"#
)]
async fn apply_pin_port_description_and_settle(
    world: &mut StarAdventurerWorld,
    description: String,
    settle_secs: u64,
) {
    let port = world.bound_port();
    let mut config = world.current_config().await;
    config["server"]["port"] = serde_json::json!(port);
    config["mount"]["description"] = serde_json::json!(description);
    config["mount"]["settle_after_slew"] = serde_json::json!(format!("{settle_secs}s"));
    world.call_config_apply(config).await;
}

#[when("config.apply is called with an empty mount unique_id")]
async fn apply_empty_unique_id(world: &mut StarAdventurerWorld) {
    let mut config = world.current_config().await;
    config["mount"]["unique_id"] = serde_json::json!("");
    world.call_config_apply(config).await;
}

#[then(regex = r"^the apply status should be (\w+)$")]
async fn assert_apply_status(world: &mut StarAdventurerWorld, expected: String) {
    let response = world.last_response.as_ref().expect("no response stashed");
    assert_eq!(response["status"].as_str(), Some(expected.as_str()));
}

/// Also a `When`: a scenario that goes on to act on the reloaded service
/// uses it as the barrier that the reload has landed.
#[when(regex = r#"^the reloaded service serves mount description "([^"]+)"$"#)]
#[then(regex = r#"^the reloaded service serves mount description "([^"]+)"$"#)]
async fn reloaded_serves_description(world: &mut StarAdventurerWorld, expected: String) {
    world.wait_for_config_description(&expected).await;
}

#[then("the response should contain validation errors")]
async fn assert_validation_errors(world: &mut StarAdventurerWorld) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let errors = response["errors"]
        .as_array()
        .expect("`errors` is not an array");
    assert!(!errors.is_empty(), "expected validation errors, got none");
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
async fn assert_action_not_implemented(world: &mut StarAdventurerWorld) {
    assert_eq!(
        world.last_error_code,
        Some(ASCOMErrorCode::ACTION_NOT_IMPLEMENTED.raw()),
        "expected ACTION_NOT_IMPLEMENTED, got {:?}",
        world.last_error_code
    );
}
