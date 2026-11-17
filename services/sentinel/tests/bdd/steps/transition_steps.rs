//! BDD step definitions for the transition rules feature

use std::time::Duration;

use cucumber::{given, then, when};

use crate::world::SentinelWorld;

// --- Given steps ---

#[given(expr = "an unreadable transition rule for {string}")]
fn unreadable_transition(world: &mut SentinelWorld, monitor_name: String) {
    world.sentinel_has_notifiers = true;
    world.sentinel_transitions.push(serde_json::json!({
        "monitor_name": monitor_name,
        "direction": "unreadable",
        "notifiers": ["pushover"],
        "message_template": "%monitor_name% is now %new_state%"
    }));
}

#[given("the Pushover API never answers")]
async fn pushover_never_answers(world: &mut SentinelWorld) {
    let port = world.start_silent_device().await;
    world.pushover_stub_url = Some(format!("http://127.0.0.1:{port}/1/messages.json"));
}

// --- When steps ---

#[when("the filemonitor device is disconnected")]
async fn filemonitor_disconnected(world: &mut SentinelWorld) {
    world.disconnect_filemonitor().await;
}

#[when("filemonitor stops")]
async fn filemonitor_stops(world: &mut SentinelWorld) {
    world.stop_filemonitor().await;
}

#[when("filemonitor starts again")]
async fn filemonitor_starts_again(world: &mut SentinelWorld) {
    world.start_filemonitor().await;
}

// --- Then steps ---

#[then("the filemonitor device should report connected")]
async fn filemonitor_reports_connected(world: &mut SentinelWorld) {
    for _ in 0..60 {
        if world.filemonitor_connected().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("filemonitor's device was not connected again within 15 s");
}

#[then(expr = "the notification history should hold the message {string}")]
async fn history_holds_message(world: &mut SentinelWorld, expected: String) {
    let history = world
        .wait_for_history(|h| h["message"].as_str() == Some(expected.as_str()))
        .await;
    assert!(
        history
            .iter()
            .any(|h| h["message"].as_str() == Some(expected.as_str())),
        "Expected a history record with message '{expected}'. History: {history:?}"
    );
}

#[then(expr = "the notification history should hold a failed notification for {string}")]
async fn history_holds_failed(world: &mut SentinelWorld, monitor_name: String) {
    let failed = |h: &serde_json::Value| {
        h["monitor_name"].as_str() == Some(monitor_name.as_str())
            && h["success"].as_bool() == Some(false)
    };
    // The notifier gives up on its POST after 10 s.
    let mut history = world.get_history().await;
    for _ in 0..120 {
        if history.iter().any(failed) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        history = world.get_history().await;
    }
    panic!("Expected a failed notification for '{monitor_name}'. History: {history:?}");
}

#[then(expr = "the notification history should hold {int} message(s)")]
async fn history_holds_count(world: &mut SentinelWorld, expected: usize) {
    let mut history = world.get_history().await;
    for _ in 0..60 {
        if history.len() >= expected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        history = world.get_history().await;
    }
    assert_eq!(
        history.len(),
        expected,
        "Expected {expected} history records. History: {history:?}"
    );
}
