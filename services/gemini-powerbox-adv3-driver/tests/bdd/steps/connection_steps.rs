//! Step definitions for `connection_lifecycle.feature`
//!
//! Also defines shared steps used across multiple features:
//! - "Given a running Gemini hub server"
//! - "Given a running Gemini hub server with <preset> and the switch connected"
//! - "When I connect/disconnect the switch/OC device"
//! - "When I try to start a Gemini hub server with <preset>"
//! - "Then the server refuses to start" / "the refusal names ..."
//! - "Then the last operation should have failed"
//! - "Then the last error code should be {word}"

use crate::steps::infrastructure::{default_test_config, run_to_exit};
use crate::steps::switch_control_steps::preset_env;
use crate::world::GeminiWorld;
use ascom_alpaca::ASCOMErrorCode;
use cucumber::{given, then, when};

/// Environment variable the mock reads its `>H#` reply from. Spelled out
/// here because the BDD crate links the driver without its mock module.
const ENV_IDENTITY: &str = "GEMINI_MOCK_IDENTITY";

// ============================================================================
// Given steps
// ============================================================================

#[given("a running Gemini hub server")]
async fn a_running_gemini_server(world: &mut GeminiWorld) {
    world.config = default_test_config();
    world.start_gemini().await;
}

/// A server whose simulated hub was preset before it started — a firmware,
/// unplugged sensors, dew modes, a fault — with the switch connected. The
/// phrases are resolved in `switch_control_steps::preset_env`.
#[given(regex = r"^a running Gemini hub server with (.+) and the switch connected$")]
async fn running_server_with_preset_and_switch(world: &mut GeminiWorld, preset: String) {
    let envs = preset_env(&preset);
    let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    world.config = default_test_config();
    world.start_gemini_with_env(&envs).await;
    world.switch_ref().set_connected(true).await.unwrap();
}

/// The same, with the `ObservingConditions` device connected.
#[given(regex = r"^a running Gemini hub server with (.+) and the OC device connected$")]
async fn running_server_with_preset_and_oc(world: &mut GeminiWorld, preset: String) {
    let envs = preset_env(&preset);
    let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    world.config = default_test_config();
    world.start_gemini_with_env(&envs).await;
    world.oc_ref().set_connected(true).await.unwrap();
}

// ============================================================================
// When steps
// ============================================================================

#[when("I connect the switch device")]
async fn connect_switch_device(world: &mut GeminiWorld) {
    world.switch_ref().set_connected(true).await.unwrap();
}

#[when("I disconnect the switch device")]
async fn disconnect_switch_device(world: &mut GeminiWorld) {
    world.switch_ref().set_connected(false).await.unwrap();
}

#[when("I try to connect the switch device")]
async fn try_connect_switch_device(world: &mut GeminiWorld) {
    let result = world.switch_ref().set_connected(true).await;
    world.capture_result(result);
}

#[when("I connect the OC device")]
async fn connect_oc_device(world: &mut GeminiWorld) {
    world.oc_ref().set_connected(true).await.unwrap();
}

#[when("I disconnect the OC device")]
async fn disconnect_oc_device(world: &mut GeminiWorld) {
    world.oc_ref().set_connected(false).await.unwrap();
}

#[when("I try to connect the OC device")]
async fn try_connect_oc_device(world: &mut GeminiWorld) {
    let result = world.oc_ref().set_connected(true).await;
    world.capture_result(result);
}

#[when(expr = "I cycle the switch device connection {int} times")]
async fn cycle_switch_device_connection(world: &mut GeminiWorld, count: i32) {
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

/// Start a server whose hub was preset to something the handshake refuses,
/// and keep how the start ended.
#[when(regex = r"^I try to start a Gemini hub server with (.+)$")]
async fn try_start_with_preset(world: &mut GeminiWorld, preset: String) {
    let envs = preset_env(&preset);
    let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    world.start_outcome = Some(run_to_exit(&default_test_config(), &envs).await);
}

/// Start a server whose hub answers `>H#` with `identity` — `*H<identity>#`,
/// or the text verbatim when it starts with `raw:`.
#[when(expr = "I try to start a Gemini hub server whose identity reply is {string}")]
async fn try_start_with_identity(world: &mut GeminiWorld, identity: String) {
    world.start_outcome =
        Some(run_to_exit(&default_test_config(), &[(ENV_IDENTITY, identity.as_str())]).await);
}

/// Start a server with the config the Given steps built, expecting the
/// config itself to be refused.
#[when("I try to start the configured Gemini hub server")]
async fn try_start_configured(world: &mut GeminiWorld) {
    world.start_outcome = Some(run_to_exit(&world.config, &[]).await);
}

// ============================================================================
// Then steps
// ============================================================================

#[then("the switch device should report connected")]
async fn switch_device_should_report_connected(world: &mut GeminiWorld) {
    assert!(
        world.switch_ref().connected().await.unwrap(),
        "switch should report connected"
    );
}

#[then("the switch device should report disconnected")]
async fn switch_device_should_report_disconnected(world: &mut GeminiWorld) {
    assert!(
        !world.switch_ref().connected().await.unwrap(),
        "switch should report disconnected"
    );
}

#[then("the OC device should report connected")]
async fn oc_device_should_report_connected(world: &mut GeminiWorld) {
    assert!(
        world.oc_ref().connected().await.unwrap(),
        "OC should report connected"
    );
}

#[then("the OC device should report disconnected")]
async fn oc_device_should_report_disconnected(world: &mut GeminiWorld) {
    assert!(
        !world.oc_ref().connected().await.unwrap(),
        "OC should report disconnected"
    );
}

#[then("the server refuses to start")]
fn server_refuses_to_start(world: &mut GeminiWorld) {
    let outcome = world
        .start_outcome
        .as_ref()
        .expect("no start was attempted");
    assert!(
        outcome.exited,
        "the service kept running instead of refusing: {}",
        outcome.stderr
    );
    assert!(
        !outcome.success,
        "the service exited 0 instead of refusing: {}",
        outcome.stderr
    );
}

#[then(expr = "the refusal names {string}")]
fn refusal_names(world: &mut GeminiWorld, expected: String) {
    let outcome = world
        .start_outcome
        .as_ref()
        .expect("no start was attempted");
    assert!(
        outcome.stderr.contains(&expected),
        "expected the refusal to name {expected:?}; stderr was:\n{}",
        outcome.stderr
    );
}

#[then("the last operation should have failed")]
fn last_operation_should_have_failed(world: &mut GeminiWorld) {
    assert!(
        world.last_error.is_some(),
        "expected an error but none occurred"
    );
}

#[then(expr = "the last error code should be {word}")]
fn last_error_code_should_be(world: &mut GeminiWorld, expected_code: String) {
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
