//! Configuration-action steps (`config.get` / `config.apply` / `config.schema`).

use ascom_alpaca::ASCOMErrorCode;
use cucumber::{then, when};

use crate::world::CameraWorld;

#[when(regex = r"^the supported actions are queried on camera device (\d+)$")]
async fn query_supported_actions(world: &mut CameraWorld, _device: u32) {
    let camera = world.camera();
    world.last_actions = Some(camera.supported_actions().await.unwrap());
}

#[then("the supported actions should include config.get, config.apply, and config.schema")]
async fn supported_actions_include_config(world: &mut CameraWorld) {
    let actions = world
        .last_actions
        .as_ref()
        .expect("no supported actions stashed");
    for action in ["config.get", "config.apply", "config.schema"] {
        assert!(
            actions.iter().any(|a| a == action),
            "supported actions {actions:?} missing {action}"
        );
    }
}

#[when("config.schema is called")]
async fn call_config_schema(world: &mut CameraWorld) {
    world.call_action("config.schema", "").await;
}

#[then("the schema should describe the devices and server sections")]
async fn schema_describes_sections(world: &mut CameraWorld) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let props = response["schema"]["properties"]
        .as_object()
        .expect("schema.properties is an object");
    for section in ["devices", "server"] {
        assert!(
            props.contains_key(section),
            "schema missing section {section}"
        );
    }
}

#[then("the schema should offer both MaxADU reporting modes")]
async fn schema_offers_max_adu_modes(world: &mut CameraWorld) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let schema = &response["schema"];
    let field = &schema["properties"]["max_adu_reporting"];
    assert!(
        !field.is_null(),
        "schema missing max_adu_reporting: {schema}"
    );

    // The default belongs in the schema too: an operator needs to know which
    // contract they get by leaving the key out.
    assert_eq!(
        field["default"].as_str(),
        Some("saturation_threshold"),
        "schema must advertise the accurate default"
    );

    // Read the *selectable values*. Substring-searching the serialized schema
    // would also match a mode named only in some description, and so could
    // pass while the field offered nothing.
    //
    // The values may sit behind a $ref or inline on the field itself — a
    // generator may do either, and both are correct schema — so resolve only
    // when a $ref is actually present rather than requiring one.
    let def = match field["$ref"].as_str() {
        Some(target) => {
            let name = target.rsplit('/').next().expect("$ref has a final segment");
            schema["$defs"]
                .get(name)
                .or_else(|| schema["definitions"].get(name))
                .unwrap_or_else(|| panic!("schema has no definition for {name}: {schema}"))
        }
        None => field,
    };

    // schemars renders a *documented* fieldless enum as `oneOf[].const`, and an
    // undocumented one as a flat `enum` array. Accept either, so that editing a
    // doc comment cannot quietly gut this assertion.
    let mut modes: Vec<&str> = def["oneOf"].as_array().map_or_default(|variants| {
        variants
            .iter()
            .filter_map(|v| v["const"].as_str())
            .collect()
    });
    if modes.is_empty() {
        modes = def["enum"].as_array().map_or_default(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect()
        });
    }
    modes.sort_unstable();
    assert_eq!(
        modes,
        ["container_full_scale", "saturation_threshold"],
        "schema must offer exactly the two documented modes, got {modes:?}"
    );
}

#[then(regex = r"^the schema should mark (\S+) as a read-only field$")]
async fn schema_marks_read_only(world: &mut CameraWorld, field: String) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let read_only = response["read_only_fields"]
        .as_array()
        .expect("read_only_fields is an array");
    assert!(
        read_only.iter().any(|v| v.as_str() == Some(field.as_str())),
        "read_only_fields {read_only:?} missing {field}"
    );
}

#[then("the schema should report no locked identity fields")]
async fn schema_no_locked_fields(world: &mut CameraWorld) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let locked = response["locked_fields"]
        .as_array()
        .expect("locked_fields is an array");
    assert!(
        locked.is_empty(),
        "expected no locked fields, got {locked:?}"
    );
}

#[when("config.get is called")]
async fn call_config_get(world: &mut CameraWorld) {
    world.call_action("config.get", "").await;
}

#[then("the config should report an empty devices map")]
async fn config_empty_devices(world: &mut CameraWorld) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let devices = response["config"]["devices"]
        .as_object()
        .expect("config.devices is an object");
    assert!(
        devices.is_empty(),
        "expected empty devices, got {devices:?}"
    );
}

#[then("the config should report no CLI-pinned override paths")]
async fn config_no_overrides(world: &mut CameraWorld) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let overrides = response["overrides"]
        .as_array()
        .expect("overrides is an array");
    assert!(
        overrides.is_empty(),
        "expected no overrides, got {overrides:?}"
    );
}

#[when(regex = r#"^config\.apply sets the devices override "([^"]+)" name to "([^"]+)"$"#)]
async fn apply_device_name(world: &mut CameraWorld, serial: String, name: String) {
    let mut config = world.config_get().await;
    config["devices"]
        .as_object_mut()
        .expect("config.devices is an object")
        .insert(serial, serde_json::json!({ "name": name }));
    world.call_action("config.apply", &config.to_string()).await;
}

#[when("config.apply sets a devices override name to a number")]
async fn apply_malformed_device_name(world: &mut CameraWorld) {
    let mut config = world.config_get().await;
    config["devices"]
        .as_object_mut()
        .expect("config.devices is an object")
        .insert("ASI-test".to_string(), serde_json::json!({ "name": 42 }));
    world.call_action("config.apply", &config.to_string()).await;
}

#[then(regex = r"^the apply status should be (\w+)$")]
async fn apply_status_is(world: &mut CameraWorld, status: String) {
    let response = world.last_response.as_ref().expect("no response stashed");
    assert_eq!(response["status"].as_str(), Some(status.as_str()));
}

#[then(regex = r"^the reload list should include (\w+)$")]
async fn reload_list_includes(world: &mut CameraWorld, section: String) {
    let response = world.last_response.as_ref().expect("no response stashed");
    let reload = response["reload"].as_array().expect("reload is an array");
    assert!(
        reload
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s.starts_with(&section))),
        "reload {reload:?} does not include {section}"
    );
}

#[then("the call should fail with an invalid-value error")]
async fn call_invalid_value(world: &mut CameraWorld) {
    assert_eq!(
        world.last_error_code,
        Some(ASCOMErrorCode::INVALID_VALUE.raw())
    );
}

#[when(regex = r#"^the action "([^"]+)" is called on camera device (\d+)$"#)]
async fn call_named_action(world: &mut CameraWorld, action: String, _device: u32) {
    world.call_action(&action, "").await;
}

#[then("the call should fail with an action-not-implemented error")]
async fn call_action_not_implemented(world: &mut CameraWorld) {
    assert_eq!(
        world.last_error_code,
        Some(ASCOMErrorCode::ACTION_NOT_IMPLEMENTED.raw())
    );
}
