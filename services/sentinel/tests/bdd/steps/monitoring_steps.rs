//! BDD step definitions for end-to-end monitoring feature

use cucumber::{given, then, when};

use crate::world::SentinelWorld;

// --- Given steps ---

#[given(expr = "a monitoring file containing {string}")]
fn monitoring_file_containing(world: &mut SentinelWorld, content: String) {
    world.create_temp_file(&content);
}

#[given(expr = "filemonitor is running with a contains rule {string} as safe")]
async fn filemonitor_running_with_rule(world: &mut SentinelWorld, pattern: String) {
    world.fm_rules.push(serde_json::json!({
        "type": "contains",
        "pattern": pattern,
        "safe": true
    }));
    world.start_filemonitor().await;
}

#[given(
    expr = "filemonitor is running with a contains rule {string} as safe and {int} second polling"
)]
async fn filemonitor_running_with_rule_and_polling(
    world: &mut SentinelWorld,
    pattern: String,
    interval: u64,
) {
    world.fm_rules.push(serde_json::json!({
        "type": "contains",
        "pattern": pattern,
        "safe": true
    }));
    world.fm_polling_interval = interval;
    world.start_filemonitor().await;
}

#[given(expr = "sentinel is configured to monitor the filemonitor with {int} second polling")]
fn sentinel_configured_with_polling(world: &mut SentinelWorld, interval: u64) {
    world.sentinel_polling_interval = interval;
    world.add_filemonitor_monitor("Roof Monitor");
}

#[given(expr = "a safe-to-unsafe transition rule for {string}")]
fn safe_to_unsafe_transition(world: &mut SentinelWorld, monitor_name: String) {
    world.sentinel_has_notifiers = true;
    world.sentinel_transitions.push(serde_json::json!({
        "monitor_name": monitor_name,
        "direction": "safe_to_unsafe",
        "notifiers": ["pushover"],
        "message_template": "%monitor_name% changed to %new_state%"
    }));
}

#[given("sentinel is configured to monitor a device at an unreachable address")]
fn sentinel_configured_unreachable(world: &mut SentinelWorld) {
    world.sentinel_monitors.push(serde_json::json!({
        "type": "alpaca_safety_monitor",
        "name": "Unreachable Monitor",
        "host": "127.0.0.1",
        "port": 1,
        "device_number": 0,
        "polling_interval": "1s"
    }));
}

// --- When steps ---

#[when("I wait for sentinel to poll")]
async fn wait_for_poll(world: &mut SentinelWorld) {
    world.wait_for_poll().await;
}

#[when(expr = "the monitoring file changes to {string}")]
fn file_changes(world: &mut SentinelWorld, content: String) {
    let path = world
        .temp_file_path
        .as_ref()
        .expect("temp file not created");
    std::fs::write(path, content).expect("failed to write to temp file");
}

// --- Then steps ---

#[then(expr = "the dashboard status should show {string} for {string}")]
async fn dashboard_status_shows(world: &mut SentinelWorld, expected_state: String, name: String) {
    let state = world.wait_for_status(&name, &expected_state).await;
    assert_eq!(
        state.as_deref(),
        Some(expected_state.as_str()),
        "Expected '{name}' to reach state '{expected_state}', but last observed {state:?}"
    );
}

#[then(expr = "the dashboard history should contain a record for {string}")]
async fn dashboard_history_contains(world: &mut SentinelWorld, monitor_name: String) {
    let history = world
        .wait_for_history(|h| h["monitor_name"].as_str() == Some(monitor_name.as_str()))
        .await;
    let found = history
        .iter()
        .any(|h| h["monitor_name"].as_str() == Some(monitor_name.as_str()));
    assert!(
        found,
        "Expected notification history to contain record for '{monitor_name}', but it didn't. History: {history:?}"
    );
}

#[then(expr = "the history record message should contain {string}")]
async fn history_record_message_contains(world: &mut SentinelWorld, expected: String) {
    let history = world
        .wait_for_history(|h| h["message"].as_str().is_some_and(|m| m.contains(&expected)))
        .await;
    let found = history
        .iter()
        .any(|h| h["message"].as_str().is_some_and(|m| m.contains(&expected)));
    assert!(
        found,
        "Expected a history record with message containing '{expected}', but none found. History: {history:?}"
    );
}
