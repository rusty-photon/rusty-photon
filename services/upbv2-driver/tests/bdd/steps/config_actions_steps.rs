//! Step definitions for `config_actions.feature`.
//!
//! Reuses the config + `I start the UPBv2 server` steps from `server_steps`. The
//! spawned binary builds both devices with the config-action context wired
//! (via `main`'s `run_with_reload` loop), so both dispatch the actions against one
//! config file + reload signal.

use std::sync::Arc;

use ascom_alpaca::ASCOMErrorCode;
use cucumber::{then, when};

use crate::world::Upbv2World;

#[when("the supported actions are queried on the switch device")]
async fn query_supported_actions_switch(world: &mut Upbv2World) {
    let switch = Arc::clone(world.switch.as_ref().expect("switch not discovered"));
    world.last_supported_actions = Some(switch.supported_actions().await.unwrap());
}

#[when("the supported actions are queried on the observingconditions device")]
async fn query_supported_actions_oc(world: &mut Upbv2World) {
    let oc = Arc::clone(world.oc.as_ref().expect("oc not discovered"));
    world.last_supported_actions = Some(oc.supported_actions().await.unwrap());
}

#[then("the queried supported actions should include config.get, config.apply, and config.schema")]
async fn assert_supported_actions(world: &mut Upbv2World) {
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

#[when("config.schema is called on the switch device")]
async fn call_config_schema(world: &mut Upbv2World) {
    let switch = Arc::clone(world.switch.as_ref().expect("switch not discovered"));
    let body = switch
        .action("config.schema".to_string(), String::new())
        .await
        .expect("config.schema failed");
    world.last_response =
        Some(serde_json::from_str(&body).expect("config.schema returned invalid JSON"));
}

#[then("the schema should describe the serial, server, switch, and observingconditions sections")]
async fn assert_schema_sections(world: &mut Upbv2World) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let props = response["schema"]["properties"]
        .as_object()
        .expect("schema.properties is not an object");
    for section in ["serial", "server", "switch", "observingconditions"] {
        assert!(
            props.contains_key(section),
            "schema is missing section {section}: {props:?}"
        );
    }
}

#[then(
    "the schema should mark switch.unique_id and observingconditions.unique_id as locked fields"
)]
async fn assert_schema_locked_fields(world: &mut Upbv2World) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let locked = response["locked_fields"]
        .as_array()
        .expect("`locked_fields` is not an array");
    for expected in ["switch.unique_id", "observingconditions.unique_id"] {
        assert!(
            locked.iter().any(|f| f.as_str() == Some(expected)),
            "locked_fields {locked:?} does not include {expected}"
        );
    }
}

#[when("config.get is called on the switch device")]
async fn call_config_get(world: &mut Upbv2World) {
    world.current_config().await;
}

#[then(regex = r"^the config should report serial\.port as (\S+)$")]
async fn assert_config_serial_port(world: &mut Upbv2World, expected: String) {
    let response = world.last_response.as_ref().expect("no response stashed");
    assert_eq!(
        response["config"]["serial"]["port"].as_str(),
        Some(expected.as_str())
    );
}

#[then("the config should report no overrides")]
async fn assert_no_overrides(world: &mut Upbv2World) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let overrides = response["overrides"]
        .as_array()
        .expect("`overrides` is not an array");
    assert!(
        overrides.is_empty(),
        "expected no overrides, got {overrides:?}"
    );
}

#[when(regex = r#"^config\.apply pins the bound port and sets the switch name to "([^"]+)"$"#)]
async fn apply_pin_port_and_switch_name(world: &mut Upbv2World, name: String) {
    let port = world.bound_port();
    let mut config = world.current_config().await;
    config["server"]["port"] = serde_json::json!(port);
    config["switch"]["name"] = serde_json::json!(name);
    world.call_config_apply(config).await;
}

#[when("config.apply is called with an empty serial port")]
async fn apply_empty_serial_port(world: &mut Upbv2World) {
    let mut config = world.current_config().await;
    config["serial"]["port"] = serde_json::json!("");
    world.call_config_apply(config).await;
}

#[then(regex = r"^the apply status should be (\w+)$")]
async fn assert_apply_status(world: &mut Upbv2World, expected: String) {
    let response = world.last_response.as_ref().expect("no response stashed");
    assert_eq!(response["status"].as_str(), Some(expected.as_str()));
}

#[then(regex = r#"^the reloaded service serves switch name "([^"]+)"$"#)]
async fn assert_reloaded_serves(world: &mut Upbv2World, expected: String) {
    world.wait_for_config_switch_name(&expected).await;
}

#[then("the response should contain validation errors")]
async fn assert_validation_errors(world: &mut Upbv2World) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let errors = response["errors"]
        .as_array()
        .expect("`errors` is not an array");
    assert!(!errors.is_empty(), "expected validation errors, got none");
}

#[when(regex = r#"^the action "([^"]+)" is called on the switch device$"#)]
async fn call_named_action(world: &mut Upbv2World, name: String) {
    let switch = Arc::clone(world.switch.as_ref().expect("switch not discovered"));
    match switch.action(name, String::new()).await {
        Ok(_) => world.last_error = None,
        Err(e) => world.last_error = Some(e),
    }
}

#[then("the call should fail with an action-not-implemented error")]
async fn assert_action_not_implemented(world: &mut Upbv2World) {
    let err = world
        .last_error
        .as_ref()
        .expect("expected an error from the call");
    assert_eq!(err.code, ASCOMErrorCode::ACTION_NOT_IMPLEMENTED);
}
