//! Step definitions for `connection_lifecycle.feature`
//!
//! Also defines shared steps used across multiple features:
//! - "Given a running PPBA server"
//! - "When I connect/disconnect the switch/OC device"
//! - "Then the last operation should have failed"
//! - "Then the last error code should be {word}"

use crate::steps::infrastructure::default_test_config;
use crate::world::PpbaWorld;
use ascom_alpaca::ASCOMErrorCode;
use cucumber::{given, then, when};

// ============================================================================
// Given steps
// ============================================================================

#[given("a running PPBA server")]
async fn a_running_ppba_server(world: &mut PpbaWorld) {
    world.config = default_test_config();
    world.start_ppba().await;
}

// ============================================================================
// When steps
// ============================================================================

#[when("I connect the switch device")]
async fn connect_switch_device(world: &mut PpbaWorld) {
    world.switch_ref().set_connected(true).await.unwrap();
}

#[when("I disconnect the switch device")]
async fn disconnect_switch_device(world: &mut PpbaWorld) {
    world.switch_ref().set_connected(false).await.unwrap();
}

#[when("I try to connect the switch device")]
async fn try_connect_switch_device(world: &mut PpbaWorld) {
    let result = world.switch_ref().set_connected(true).await;
    world.capture_result(result);
}

#[when("I connect the OC device")]
async fn connect_oc_device(world: &mut PpbaWorld) {
    world.oc_ref().set_connected(true).await.unwrap();
}

#[when("I disconnect the OC device")]
async fn disconnect_oc_device(world: &mut PpbaWorld) {
    world.oc_ref().set_connected(false).await.unwrap();
}

#[when("I try to connect the OC device")]
async fn try_connect_oc_device(world: &mut PpbaWorld) {
    let result = world.oc_ref().set_connected(true).await;
    world.capture_result(result);
}

#[when(expr = "I cycle the switch device connection {int} times")]
async fn cycle_switch_device_connection(world: &mut PpbaWorld, count: i32) {
    let switch = world.switch_ref();
    for _ in 0..count {
        switch.set_connected(true).await.unwrap();

        assert!(
            switch.connected().await.unwrap(),
            "switch should be connected during cycle"
        );

        switch.set_connected(false).await.unwrap();

        assert!(
            !switch.connected().await.unwrap(),
            "switch should be disconnected during cycle"
        );
    }
}

// ============================================================================
// Then steps
// ============================================================================

#[then("the switch device should report connected")]
async fn switch_device_should_report_connected(world: &mut PpbaWorld) {
    assert!(
        world.switch_ref().connected().await.unwrap(),
        "switch should report connected"
    );
}

#[then("the switch device should report disconnected")]
async fn switch_device_should_report_disconnected(world: &mut PpbaWorld) {
    assert!(
        !world.switch_ref().connected().await.unwrap(),
        "switch should report disconnected"
    );
}

#[then("the OC device should report connected")]
async fn oc_device_should_report_connected(world: &mut PpbaWorld) {
    assert!(
        world.oc_ref().connected().await.unwrap(),
        "OC should report connected"
    );
}

#[then("the OC device should report disconnected")]
async fn oc_device_should_report_disconnected(world: &mut PpbaWorld) {
    assert!(
        !world.oc_ref().connected().await.unwrap(),
        "OC should report disconnected"
    );
}

#[then("the last operation should have failed")]
fn last_operation_should_have_failed(world: &mut PpbaWorld) {
    assert!(
        world.last_error.is_some(),
        "expected an error but none occurred"
    );
}

#[then(expr = "the last error code should be {word}")]
fn last_error_code_should_be(world: &mut PpbaWorld, expected_code: String) {
    let error = world
        .last_error
        .as_ref()
        .expect("expected an error but none was set");
    let expected = match expected_code.as_str() {
        "NOT_CONNECTED" => ASCOMErrorCode::NOT_CONNECTED,
        "INVALID_VALUE" => ASCOMErrorCode::INVALID_VALUE,
        "INVALID_OPERATION" => ASCOMErrorCode::INVALID_OPERATION,
        "NOT_IMPLEMENTED" => ASCOMErrorCode::NOT_IMPLEMENTED,
        "VALUE_NOT_SET" => ASCOMErrorCode::VALUE_NOT_SET,
        other => panic!("Unknown ASCOM error code: {other}"),
    };
    assert_eq!(
        error.code, expected,
        "expected error code {expected_code}, got {:?} (message: {})",
        error.code, error.message
    );
}

#[then(expr = "the last error message should contain {string}")]
fn last_error_message_should_contain(world: &mut PpbaWorld, expected: String) {
    let error = world
        .last_error
        .as_ref()
        .expect("expected an error but none was set");
    assert!(
        error.message.contains(&expected),
        "expected the error message to contain {expected:?}, got {:?}",
        error.message
    );
}
