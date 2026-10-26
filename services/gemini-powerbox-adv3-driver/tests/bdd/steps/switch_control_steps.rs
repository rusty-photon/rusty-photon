//! Step definitions for `switch_control.feature`
//!
//! Also defines the shared "Given a running Gemini hub server with the
//! switch connected" step and the preset resolver the other features' Given
//! steps use to put the simulated hub in a state the driver cannot set.

use crate::steps::infrastructure::default_test_config;
use crate::world::GeminiWorld;
use cucumber::{given, then, when};

// The environment variables the mock reads its presets from, spelled out
// here because the BDD crate links the driver without its mock module. The
// documented list is the knob table in
// docs/services/gemini-powerbox-adv3-driver.md.
const ENV_FIRMWARE: &str = "GEMINI_MOCK_FIRMWARE";
const ENV_SENSORS: &str = "GEMINI_MOCK_SENSORS";
const ENV_DEW_MODES: &str = "GEMINI_MOCK_DEW_MODES";
const ENV_INPUT_VOLTAGE: &str = "GEMINI_MOCK_INPUT_VOLTAGE";
const ENV_STALE_FRAMES: &str = "GEMINI_MOCK_STALE_FRAMES";
const ENV_IGNORE_FIRST_IDENTITY: &str = "GEMINI_MOCK_IGNORE_FIRST_IDENTITY";
const ENV_STUCK_OUTPUT: &str = "GEMINI_MOCK_STUCK_OUTPUT";
const ENV_STREAMING_AT_START: &str = "GEMINI_MOCK_STREAMING_AT_START";
const ENV_CUT_FRAMES: &str = "GEMINI_MOCK_CUT_FRAMES";

/// Resolve a feature file's wording for a hub preset to the mock's
/// environment (testing.md §3.4): the feature says what the hub is like,
/// and this is the one place that knows which knob spells it.
///
/// # Panics
///
/// Panics on a phrase it does not know, so a typo in a feature file fails
/// at the Given rather than running the scenario against the default hub.
pub fn preset_env(phrase: &str) -> Vec<(&'static str, String)> {
    if let Some(firmware) = phrase.strip_prefix("firmware ") {
        return vec![(ENV_FIRMWARE, firmware.to_string())];
    }
    if phrase == "a hub still streaming from an earlier session and cutting frames short to answer"
    {
        return vec![
            (ENV_STREAMING_AT_START, "1".to_string()),
            (ENV_CUT_FRAMES, "1".to_string()),
        ];
    }
    let (name, value) = match phrase {
        "both dew channels in Auto" => (ENV_DEW_MODES, "00"),
        "both dew channels in Switch" => (ENV_DEW_MODES, "22"),
        "DEW6 in Auto and DEW7 in Manual" => (ENV_DEW_MODES, "01"),
        "the AHT20 unplugged" => (ENV_SENSORS, "01"),
        "the DS18B20 unplugged" => (ENV_SENSORS, "10"),
        "both sensors unplugged" => (ENV_SENSORS, "00"),
        "a 24 V supply on the input" => (ENV_INPUT_VOLTAGE, "24.0"),
        "two streamed frames queued ahead of every reply" => (ENV_STALE_FRAMES, "1"),
        "an ignored first identity request" => (ENV_IGNORE_FIRST_IDENTITY, "1"),
        "DC3 tripped and refusing to switch" => (ENV_STUCK_OUTPUT, "3"),
        other => panic!("unknown hub preset in the feature file: {other:?}"),
    };
    vec![(name, value.to_string())]
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

#[given("a running Gemini hub server with the switch connected")]
async fn running_server_with_switch_connected(world: &mut GeminiWorld) {
    world.config = default_test_config();
    world.start_gemini().await;
    world.switch_ref().set_connected(true).await.unwrap();
}

// ============================================================================
// When steps
// ============================================================================

#[when("I wait for the switch data to be available")]
async fn wait_for_switch_data(world: &mut GeminiWorld) {
    world.wait_for_switch_data().await;
}

#[when(expr = "I set switch {int} value to {float}")]
async fn set_switch_value(world: &mut GeminiWorld, id: usize, value: f64) {
    world
        .switch_ref()
        .set_switch_value(id, value)
        .await
        .unwrap();
}

#[when(expr = "I try to set switch {int} value to {float}")]
async fn try_set_switch_value(world: &mut GeminiWorld, id: usize, value: f64) {
    let result = world.switch_ref().set_switch_value(id, value).await;
    world.capture_result(result);
}

#[when(expr = "I set switch {int} boolean to {word}")]
async fn set_switch_boolean(world: &mut GeminiWorld, id: usize, state: String) {
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
async fn switch_value_should_be(world: &mut GeminiWorld, id: usize, expected: f64) {
    let value = world.switch_ref().get_switch_value(id).await.unwrap();
    assert!(
        (value - expected).abs() < 1e-9,
        "switch {id} value: expected {expected}, got {value}"
    );
}

#[then(expr = "switch {int} boolean should be {word}")]
async fn switch_boolean_should_be(world: &mut GeminiWorld, id: usize, expected: String) {
    let expected = switch_state(&expected);
    let actual = world.switch_ref().get_switch(id).await.unwrap();
    assert_eq!(actual, expected, "switch {id} boolean mismatch");
}

#[then(expr = "switches {int} through {int} should be writable")]
async fn switches_should_be_writable(world: &mut GeminiWorld, from: usize, to: usize) {
    let switch = world.switch_ref();
    for id in from..=to {
        assert!(
            switch.can_write(id).await.unwrap(),
            "switch {id} should be writable"
        );
    }
}

#[then(expr = "switches {int} through {int} should not be writable")]
async fn switches_should_not_be_writable(world: &mut GeminiWorld, from: usize, to: usize) {
    let switch = world.switch_ref();
    for id in from..=to {
        assert!(
            !switch.can_write(id).await.unwrap(),
            "switch {id} should not be writable"
        );
    }
}

#[then(expr = "switch {int} should not be writable")]
async fn switch_should_not_be_writable(world: &mut GeminiWorld, id: usize) {
    assert!(
        !world.switch_ref().can_write(id).await.unwrap(),
        "switch {id} should not be writable"
    );
}

#[then(expr = "switch {int} should be writable")]
async fn switch_should_be_writable(world: &mut GeminiWorld, id: usize) {
    assert!(
        world.switch_ref().can_write(id).await.unwrap(),
        "switch {id} should be writable"
    );
}

#[then(expr = "the last error message should contain {string}")]
fn last_error_message_should_contain(world: &mut GeminiWorld, expected: String) {
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
async fn all_switches_queryable(world: &mut GeminiWorld, count: usize) {
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
