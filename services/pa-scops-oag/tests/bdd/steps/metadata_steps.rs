//! Step definitions for `device_metadata.feature`

use crate::world::ScopsWorld;
use ascom_alpaca::ASCOMErrorCode;
use cucumber::{given, then, when};
use pa_scops_oag::Config;

#[given(expr = "a focuser service configured with name {string}")]
async fn focuser_with_name(world: &mut ScopsWorld, name: String) {
    let mut config = Config::default();
    config.focuser.name = name;
    world.config = Some(config);
    world.start_focuser().await;
}

#[given(expr = "a focuser service configured with unique ID {string}")]
async fn focuser_with_unique_id(world: &mut ScopsWorld, unique_id: String) {
    let mut config = Config::default();
    config.focuser.unique_id = unique_id;
    world.config = Some(config);
    world.start_focuser().await;
}

#[given(expr = "a focuser service configured with description {string}")]
async fn focuser_with_description(world: &mut ScopsWorld, description: String) {
    let mut config = Config::default();
    config.focuser.description = description;
    world.config = Some(config);
    world.start_focuser().await;
}

#[given(expr = "a focuser service configured with max step {int}")]
async fn focuser_with_max_step(world: &mut ScopsWorld, max_step: u32) {
    let mut config = Config::default();
    config.focuser.max_step = max_step;
    world.config = Some(config);
    world.start_focuser().await;
}

#[when("I try to enable temperature compensation")]
async fn try_enable_temp_comp(world: &mut ScopsWorld) {
    match world.focuser().set_temp_comp(true).await {
        Ok(()) => {
            world.last_error = None;
            world.last_error_code = None;
        }
        Err(e) => {
            world.last_error = Some(e.to_string());
            world.last_error_code = Some(e.code.raw());
        }
    }
}

#[when("I try to read step size")]
async fn try_read_step_size(world: &mut ScopsWorld) {
    match world.focuser().step_size().await {
        Ok(_) => {
            world.last_error = None;
            world.last_error_code = None;
        }
        Err(e) => {
            world.last_error = Some(e.to_string());
            world.last_error_code = Some(e.code.raw());
        }
    }
}

#[then(expr = "the device name should be {string}")]
async fn device_name_should_be(world: &mut ScopsWorld, expected: String) {
    let name = world.focuser().name().await.unwrap();
    assert_eq!(name, expected);
}

#[then(expr = "the device unique ID should be {string}")]
fn device_unique_id_should_be(world: &mut ScopsWorld, expected: String) {
    let unique_id = world.focuser().unique_id();
    assert_eq!(unique_id, expected);
}

#[then(expr = "the device description should be {string}")]
async fn device_description_should_be(world: &mut ScopsWorld, expected: String) {
    let description = world.focuser().description().await.unwrap();
    assert_eq!(description, expected);
}

#[then(expr = "the driver info should contain {string}")]
async fn driver_info_should_contain(world: &mut ScopsWorld, expected: String) {
    let info = world.focuser().driver_info().await.unwrap();
    assert!(
        info.contains(&expected),
        "expected driver info to contain '{expected}', got: {info}"
    );
}

#[then("the driver version should not be empty")]
async fn driver_version_not_empty(world: &mut ScopsWorld) {
    let version = world.focuser().driver_version().await.unwrap();
    assert_ne!(version, "");
}

#[then("the focuser should be absolute")]
async fn focuser_should_be_absolute(world: &mut ScopsWorld) {
    assert!(world.focuser().absolute().await.unwrap());
}

#[then(expr = "the max step should be {int}")]
async fn max_step_should_be(world: &mut ScopsWorld, expected: u32) {
    assert_eq!(world.focuser().max_step().await.unwrap(), expected);
}

#[then(expr = "the max increment should be {int}")]
async fn max_increment_should_be(world: &mut ScopsWorld, expected: u32) {
    assert_eq!(world.focuser().max_increment().await.unwrap(), expected);
}

#[then("temperature compensation should not be available")]
async fn temp_comp_not_available(world: &mut ScopsWorld) {
    assert!(!world.focuser().temp_comp_available().await.unwrap());
}

#[then("temperature compensation should be off")]
async fn temp_comp_should_be_off(world: &mut ScopsWorld) {
    assert!(!world.focuser().temp_comp().await.unwrap());
}

#[then("the operation should fail with not-implemented")]
fn operation_should_fail_not_implemented(world: &mut ScopsWorld) {
    let code = world
        .last_error_code
        .expect("expected an error but none occurred");
    assert_eq!(
        code,
        ASCOMErrorCode::NOT_IMPLEMENTED.raw(),
        "expected NOT_IMPLEMENTED error code, got: {code}"
    );
}

#[then(expr = "the device interface ID should be {int}")]
async fn device_interface_id(world: &mut ScopsWorld, expected: i32) {
    let id = world.focuser().interface_version().await.unwrap();
    assert_eq!(id, expected as u16);
}
