//! Step definitions for `config_actions.feature`.

use std::sync::Arc;

use ascom_alpaca::ASCOMErrorCode;
use cucumber::{then, when};

use crate::world::Fp2World;

#[when("the supported actions are queried")]
async fn query_supported_actions(world: &mut Fp2World) {
    let device = Arc::clone(world.device());
    world.last_supported_actions = Some(device.supported_actions().await.unwrap());
}

#[when("config.get is called")]
async fn call_config_get(world: &mut Fp2World) {
    world.call_config_get().await;
}

#[when("config.schema is called")]
async fn call_config_schema(world: &mut Fp2World) {
    let device = Arc::clone(world.device());
    let body = device
        .action("config.schema".to_string(), String::new())
        .await
        .expect("config.schema failed");
    world.last_response =
        Some(serde_json::from_str(&body).expect("config.schema returned invalid JSON"));
}

#[when(regex = r"^config\.apply sets max_brightness to (\d+)$")]
async fn apply_max_brightness(world: &mut Fp2World, value: u32) {
    let mut config = world.current_config().await;
    config["cover_calibrator"]["max_brightness"] = serde_json::json!(value);
    world.call_config_apply(config).await;
}

#[when("config.apply is called with an invalid baud_rate")]
async fn apply_invalid_baud_rate(world: &mut Fp2World) {
    let mut config = world.current_config().await;
    config["serial"]["baud_rate"] = serde_json::json!(0);
    world.call_config_apply(config).await;
}

#[when("config.apply is called with min_brightness above max_brightness")]
async fn apply_invalid_min_brightness(world: &mut Fp2World) {
    let mut config = world.current_config().await;
    config["cover_calibrator"]["min_brightness"] = serde_json::json!(9999);
    world.call_config_apply(config).await;
}

#[when(regex = r"^config\.apply pins the bound port and sets max_brightness to (\d+)$")]
async fn apply_pin_port_and_brightness(world: &mut Fp2World, value: u32) {
    // Pin the (OS-assigned) bound port into the file so the reload rebinds the
    // *same* port — letting this client reach the reloaded server (and
    // exercising the SO_REUSEADDR rebind across a lingering connection).
    let port = world.bound_port();
    let mut config = world.current_config().await;
    config["server"]["port"] = serde_json::json!(port);
    config["cover_calibrator"]["max_brightness"] = serde_json::json!(value);
    world.call_config_apply(config).await;
}

#[when(regex = r#"^the action "([^"]+)" is called$"#)]
async fn call_named_action(world: &mut Fp2World, name: String) {
    let device = Arc::clone(world.device());
    match device.action(name, String::new()).await {
        Ok(body) => {
            world.last_response =
                Some(serde_json::from_str(&body).expect("the action answered invalid JSON"));
            world.last_error = None;
        }
        Err(e) => world.last_error = Some(e),
    }
}

#[then("the supported actions should include config.get and config.apply")]
async fn assert_supported_actions(world: &mut Fp2World) {
    let actions = world
        .last_supported_actions
        .as_ref()
        .expect("no supported actions queried");
    assert!(
        actions.iter().any(|a| a == "config.get"),
        "config.get missing from {actions:?}"
    );
    assert!(
        actions.iter().any(|a| a == "config.apply"),
        "config.apply missing from {actions:?}"
    );
}

#[then("the schema should describe the serial, server, and cover_calibrator sections")]
async fn assert_schema_sections(world: &mut Fp2World) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let props = response["schema"]["properties"]
        .as_object()
        .expect("schema.properties is not an object");
    for section in ["serial", "server", "cover_calibrator"] {
        assert!(
            props.contains_key(section),
            "schema is missing section {section}: {props:?}"
        );
    }
}

#[then("the schema should mark cover_calibrator.unique_id as a locked field")]
async fn assert_schema_locked_field(world: &mut Fp2World) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let locked = response["locked_fields"]
        .as_array()
        .expect("`locked_fields` is not an array");
    assert!(
        locked
            .iter()
            .any(|f| f.as_str() == Some("cover_calibrator.unique_id")),
        "locked_fields {locked:?} does not include cover_calibrator.unique_id"
    );
}

#[then("the schema should mark server.port as a read-only field")]
async fn assert_schema_read_only_field(world: &mut Fp2World) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let read_only = response["read_only_fields"]
        .as_array()
        .expect("`read_only_fields` is not an array");
    assert!(
        read_only.iter().any(|f| f.as_str() == Some("server.port")),
        "read_only_fields {read_only:?} does not include server.port"
    );
}

#[then(regex = r"^the config should report serial\.port as (\S+)$")]
async fn assert_config_serial_port(world: &mut Fp2World, expected: String) {
    let response = world.last_response.as_ref().expect("no response stashed");
    assert_eq!(
        response["config"]["serial"]["port"].as_str(),
        Some(expected.as_str())
    );
}

#[then("the config should report no overrides")]
async fn assert_no_overrides(world: &mut Fp2World) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let overrides = response["overrides"]
        .as_array()
        .expect("`overrides` is not an array");
    assert!(
        overrides.is_empty(),
        "expected no overrides, got {overrides:?}"
    );
}

#[then(regex = r"^the apply status should be (\w+)$")]
async fn assert_apply_status(world: &mut Fp2World, expected: String) {
    let response = world.last_response.as_ref().expect("no response stashed");
    assert_eq!(response["status"].as_str(), Some(expected.as_str()));
}

#[then(regex = r"^the reload list should include (\S+)$")]
async fn assert_reload_includes(world: &mut Fp2World, path: String) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let reload = response["reload"]
        .as_array()
        .expect("`reload` is not an array");
    assert!(
        reload.iter().any(|p| p.as_str() == Some(path.as_str())),
        "reload list {reload:?} does not include {path}"
    );
}

#[then("the response should contain validation errors")]
async fn assert_validation_errors(world: &mut Fp2World) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let errors = response["errors"]
        .as_array()
        .expect("`errors` is not an array");
    assert!(!errors.is_empty(), "expected validation errors, got none");
}

#[then(regex = r"^the reloaded service serves max_brightness (\d+)$")]
async fn assert_reloaded_serves(world: &mut Fp2World, expected: u32) {
    world.wait_for_config_max_brightness(expected).await;
}

#[then("the call should fail with an action-not-implemented error")]
async fn assert_action_not_implemented(world: &mut Fp2World) {
    let err = world
        .last_error
        .take()
        .expect("expected an error from the call");
    assert_eq!(err.code, ASCOMErrorCode::ACTION_NOT_IMPLEMENTED);
}
