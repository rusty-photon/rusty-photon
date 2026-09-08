//! Step definitions for `switch_control.feature`
//!
//! Also defines the shared "Given a running UPBv2 server with the switch
//! connected" step and the two auto-dew presets other features build on.

use crate::steps::infrastructure::default_test_config;
use crate::world::Upbv2World;
use cucumber::{given, then, when};

/// Environment variable the mock reads its auto-dew mask from.
///
/// Auto-dew is readable but never settable through this driver — it never
/// sends `PD:` — so the only way a scenario can put a channel under device
/// control is to preset the simulated device before it starts.
const ENV_AUTO_DEW: &str = "UPBV2_MOCK_AUTO_DEW";

/// Resolve the feature file's channel wording to the raw `PA` field-20 mask.
///
/// The mask is not a bitfield: the device's table maps 1 to "all three
/// channels" and 2-7 to the individual combinations, so the mapping is a
/// lookup rather than arithmetic. Kept here (testing.md §3.4) so the feature
/// file can say which channels the device drives without carrying a magic
/// number.
fn auto_dew_mask(channels: &str) -> u8 {
    match channels {
        "all channels" => 1,
        "channel A" => 2,
        "channel B" => 3,
        "channel C" => 4,
        other => panic!("unknown auto-dew channel selection: {other}"),
    }
}

/// Resolve the feature file's boolean wording to a `bool`.
pub fn switch_state(word: &str) -> bool {
    match word {
        "true" => true,
        "false" => false,
        other => panic!("expected true or false, got: {other}"),
    }
}

// ============================================================================
// Given steps
// ============================================================================

#[given("a running UPBv2 server with the switch connected")]
async fn running_server_with_switch_connected(world: &mut Upbv2World) {
    world.config = default_test_config();
    world.start_upbv2().await;
    world.switch_ref().set_connected(true).await.unwrap();
}

#[given(regex = r"^a running UPBv2 server with auto-dew controlling (channel [ABC]|all channels)$")]
async fn running_server_with_auto_dew(world: &mut Upbv2World, channels: String) {
    let mask = auto_dew_mask(&channels);
    world.config = default_test_config();
    world
        .start_upbv2_with_env(&[(ENV_AUTO_DEW, &mask.to_string())])
        .await;
    world.switch_ref().set_connected(true).await.unwrap();
    world.wait_for_switch_data().await;
}

// ============================================================================
// When steps
// ============================================================================

#[when("I wait for the switch data to be available")]
async fn wait_for_switch_data(world: &mut Upbv2World) {
    world.wait_for_switch_data().await;
}

#[when(expr = "I set switch {int} value to {float}")]
async fn set_switch_value(world: &mut Upbv2World, id: usize, value: f64) {
    world
        .switch_ref()
        .set_switch_value(id, value)
        .await
        .unwrap();
}

#[when(expr = "I try to set switch {int} value to {float}")]
async fn try_set_switch_value(world: &mut Upbv2World, id: usize, value: f64) {
    let result = world.switch_ref().set_switch_value(id, value).await;
    world.capture_result(result);
}

#[when(expr = "I set switch {int} boolean to {word}")]
async fn set_switch_boolean(world: &mut Upbv2World, id: usize, state: String) {
    world
        .switch_ref()
        .set_switch(id, switch_state(&state))
        .await
        .unwrap();
}

// ============================================================================
// Then steps
// ============================================================================

#[then(expr = "switch {int} value should be {float}")]
async fn switch_value_should_be(world: &mut Upbv2World, id: usize, expected: f64) {
    let value = world.switch_ref().get_switch_value(id).await.unwrap();
    assert!(
        (value - expected).abs() < f64::EPSILON,
        "switch {id} value: expected {expected}, got {value}"
    );
}

#[then(expr = "switch {int} boolean should be {word}")]
async fn switch_boolean_should_be(world: &mut Upbv2World, id: usize, expected: String) {
    let expected = switch_state(&expected);
    let actual = world.switch_ref().get_switch(id).await.unwrap();
    assert_eq!(actual, expected, "switch {id} boolean mismatch");
}

#[then(expr = "switches {int} through {int} should be writable")]
async fn switches_should_be_writable(world: &mut Upbv2World, from: usize, to: usize) {
    let switch = world.switch_ref();
    for id in from..=to {
        assert!(
            switch.can_write(id).await.unwrap(),
            "switch {id} should be writable"
        );
    }
}

#[then(expr = "switches {int} through {int} should not be writable")]
async fn switches_should_not_be_writable(world: &mut Upbv2World, from: usize, to: usize) {
    let switch = world.switch_ref();
    for id in from..=to {
        assert!(
            !switch.can_write(id).await.unwrap(),
            "switch {id} should not be writable"
        );
    }
}

#[then(expr = "switch {int} should not be writable")]
async fn switch_should_not_be_writable(world: &mut Upbv2World, id: usize) {
    assert!(
        !world.switch_ref().can_write(id).await.unwrap(),
        "switch {id} should not be writable"
    );
}

#[then(expr = "switch {int} should be writable")]
async fn switch_should_be_writable(world: &mut Upbv2World, id: usize) {
    assert!(
        world.switch_ref().can_write(id).await.unwrap(),
        "switch {id} should be writable"
    );
}

#[then(expr = "the last error message should contain {string}")]
fn last_error_message_should_contain(world: &mut Upbv2World, expected: String) {
    let error = world
        .last_error
        .as_ref()
        .expect("expected an error but none was set");
    assert!(
        error.message.contains(&expected),
        "expected error message to contain '{expected}', got: {}",
        error.message
    );
}

#[then(
    expr = "all {int} switches should be queryable for name, description, min, max, step, value, and can_write"
)]
async fn all_switches_queryable(world: &mut Upbv2World, count: usize) {
    let switch = world.switch_ref();
    for id in 0..count {
        switch.get_switch_name(id).await.unwrap();
        switch.get_switch_description(id).await.unwrap();
        switch.min_switch_value(id).await.unwrap();
        switch.max_switch_value(id).await.unwrap();
        switch.switch_step(id).await.unwrap();
        switch.get_switch_value(id).await.unwrap();
        switch.can_write(id).await.unwrap();
    }
}
