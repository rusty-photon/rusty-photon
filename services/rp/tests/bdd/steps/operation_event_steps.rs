//! BDD step definitions for the operation-event envelope feature
//! (`operation_events.feature`).
//!
//! These steps drive each blocking MCP operation and assert the
//! delivered webhook bodies carry the uniform envelope: a `*_started`
//! event plus a matching `*_complete` / `*_failed`, sharing one
//! `operation_id`, each with a fresh `event_id` and monotonic
//! `event_seq`, the `started_at` / `ended_at` / `elapsed_ms` timing, and
//! the Phase-1-reserved deadline fields absent. Self-contained: the
//! webhook subscription and rp-launch steps here use a dedicated plugin
//! name so they never interact with `event_delivery.feature`'s plugin.

use std::time::Duration;

use cucumber::{given, then, when};

use bdd_infra::rp_harness::{FocuserConfig, ReceivedEvent, WebhookReceiver};

use crate::world::RpWorld;

// --- Given steps -------------------------------------------------------

/// Subscribe a webhook to an operation's full `*_started` /
/// `*_complete` / `*_failed` triple. The triple naming is the contract;
/// subscribing to all three keeps a scenario robust whether the
/// operation succeeds or fails.
#[given(expr = "a webhook subscriber for the {string} operation")]
async fn webhook_subscriber_for_operation(world: &mut RpWorld, operation: String) {
    setup_webhook_receiver(world).await;
    add_event_plugin(
        world,
        vec![
            format!("{operation}_started"),
            format!("{operation}_complete"),
            format!("{operation}_failed"),
        ],
    );
}

#[given("rp is running with a mount and the operation-event plugin")]
async fn rp_with_mount_and_plugin(world: &mut RpWorld) {
    crate::steps::tool_steps::ensure_omnisim(world).await;
    let url = world.omnisim_url();
    world.mount = Some(bdd_infra::rp_harness::MountConfig {
        alpaca_url: url,
        device_number: 0,
        settle_after_slew: None,
    });
    crate::steps::tool_steps::start_rp(world).await;
}

#[given("rp is running with a camera and the operation-event plugin")]
async fn rp_with_camera_and_plugin(world: &mut RpWorld) {
    crate::steps::tool_steps::ensure_omnisim(world).await;
    crate::steps::tool_steps::add_camera(world);
    crate::steps::tool_steps::start_rp(world).await;
}

#[given("rp is running with a focuser and the operation-event plugin")]
async fn rp_with_focuser_and_plugin(world: &mut RpWorld) {
    crate::steps::tool_steps::ensure_omnisim(world).await;
    let url = world.omnisim_url();
    world.focusers.push(FocuserConfig {
        microns_per_step: None,
        id: "main-focuser".to_string(),
        alpaca_url: url,
        device_number: 0,
        min_position: None,
        max_position: None,
        backlash: None,
    });
    crate::steps::tool_steps::start_rp(world).await;
}

// --- When steps --------------------------------------------------------

#[when(expr = "the operator slews to ra {float} dec {float}")]
async fn operator_slews(world: &mut RpWorld, ra: f64, dec: f64) {
    let _ = world
        .mcp()
        .call_tool("slew", serde_json::json!({ "ra": ra, "dec": dec }))
        .await;
}

#[when("the operator parks the mount")]
async fn operator_parks(world: &mut RpWorld) {
    let _ = world.mcp().call_tool("park", serde_json::json!({})).await;
}

#[when("the operator unparks the mount")]
async fn operator_unparks(world: &mut RpWorld) {
    let _ = world.mcp().call_tool("unpark", serde_json::json!({})).await;
}

#[when(expr = "the operator moves the focuser to position {int}")]
async fn operator_moves_focuser(world: &mut RpWorld, position: i32) {
    let _ = world
        .mcp()
        .call_tool(
            "move_focuser",
            serde_json::json!({ "focuser_id": "main-focuser", "position": position }),
        )
        .await;
}

#[when(expr = "the operator syncs the mount to ra {float} dec {float}")]
async fn operator_syncs(world: &mut RpWorld, ra: f64, dec: f64) {
    let _ = world
        .mcp()
        .call_tool("sync_mount", serde_json::json!({ "ra": ra, "dec": dec }))
        .await;
}

#[when(expr = "the operator captures a {int} ms frame on camera {string}")]
async fn operator_captures(world: &mut RpWorld, duration_ms: i32, camera_id: String) {
    let _ = world
        .mcp()
        .call_tool(
            "capture",
            serde_json::json!({
                "camera_id": camera_id,
                "duration": format!("{duration_ms}ms"),
            }),
        )
        .await;
}

// --- Then steps --------------------------------------------------------

#[then(expr = "the webhook delivers the {string} event")]
async fn webhook_delivers(world: &mut RpWorld, event_type: String) {
    assert!(
        world.wait_for_events(&event_type, 1).await,
        "expected a '{event_type}' event to be delivered"
    );
}

#[then(expr = "the webhook delivers no {string} event")]
async fn webhook_delivers_none(world: &mut RpWorld, event_type: String) {
    // Give any stray emission time to arrive before asserting absence.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let events = world.received_events.read().await;
    assert!(
        events.iter().all(|e| e.event_type != event_type),
        "expected no '{event_type}' event, but one was delivered"
    );
}

#[then(expr = "the {string} and {string} events share one operation_id")]
async fn events_share_operation_id(world: &mut RpWorld, first: String, second: String) {
    let a = find_event(world, &first).await;
    let b = find_event(world, &second).await;
    let a_id = a
        .operation_id
        .as_ref()
        .unwrap_or_else(|| panic!("'{first}' carried no operation_id"));
    let b_id = b
        .operation_id
        .as_ref()
        .unwrap_or_else(|| panic!("'{second}' carried no operation_id"));
    assert_eq!(
        a_id, b_id,
        "'{first}' and '{second}' must share one operation_id"
    );
}

#[then(expr = "the {string} and {string} events have distinct event_ids")]
async fn events_have_distinct_event_ids(world: &mut RpWorld, first: String, second: String) {
    let a = find_event(world, &first).await;
    let b = find_event(world, &second).await;
    assert!(!a.event_id.is_empty(), "'{first}' carried no event_id");
    assert_ne!(
        a.event_id, b.event_id,
        "'{first}' and '{second}' must carry distinct event_ids"
    );
}

#[then(expr = "the {string} event has a higher event_seq than the {string} event")]
async fn event_seq_higher(world: &mut RpWorld, later: String, earlier: String) {
    let l = find_event(world, &later).await;
    let e = find_event(world, &earlier).await;
    let l_seq = l
        .event_seq
        .unwrap_or_else(|| panic!("'{later}' carried no event_seq"));
    let e_seq = e
        .event_seq
        .unwrap_or_else(|| panic!("'{earlier}' carried no event_seq"));
    assert!(
        l_seq > e_seq,
        "'{later}' event_seq ({l_seq}) must exceed '{earlier}' event_seq ({e_seq})"
    );
}

#[then(expr = "the {string} event carries a started_at timestamp")]
async fn event_carries_started_at(world: &mut RpWorld, event_type: String) {
    let ev = find_event(world, &event_type).await;
    assert!(
        ev.started_at.is_some(),
        "'{event_type}' must carry started_at"
    );
    assert!(
        ev.ended_at.is_none(),
        "a started event must not carry ended_at"
    );
}

#[then(expr = "the {string} event carries ended_at and elapsed_ms")]
async fn event_carries_end_timing(world: &mut RpWorld, event_type: String) {
    let ev = find_event(world, &event_type).await;
    assert!(
        ev.started_at.is_some(),
        "'{event_type}' must echo started_at"
    );
    assert!(ev.ended_at.is_some(), "'{event_type}' must carry ended_at");
    assert!(
        ev.elapsed_ms.is_some(),
        "'{event_type}' must carry elapsed_ms"
    );
}

#[then(expr = "the {string} event reserves the deadline fields as absent")]
async fn event_reserves_deadline_fields(world: &mut RpWorld, event_type: String) {
    let ev = find_event(world, &event_type).await;
    assert!(
        ev.predicted_duration_ms.is_none(),
        "Phase 1: '{event_type}' must not carry predicted_duration_ms"
    );
    assert!(
        ev.max_duration_ms.is_none(),
        "Phase 1: '{event_type}' must not carry max_duration_ms"
    );
}

#[then(expr = "the {string} event carries the deadline fields")]
async fn event_carries_deadline_fields(world: &mut RpWorld, event_type: String) {
    let ev = find_event(world, &event_type).await;
    let predicted = ev
        .predicted_duration_ms
        .unwrap_or_else(|| panic!("'{event_type}' must carry predicted_duration_ms"));
    let max = ev
        .max_duration_ms
        .unwrap_or_else(|| panic!("'{event_type}' must carry max_duration_ms"));
    assert!(
        max >= predicted,
        "'{event_type}' max_duration_ms ({max}) must be >= predicted_duration_ms ({predicted})"
    );
}

#[then(expr = "the {string} event payload includes {string}")]
async fn event_payload_includes(world: &mut RpWorld, event_type: String, field: String) {
    let ev = find_event(world, &event_type).await;
    assert!(
        ev.payload.get(&field).is_some(),
        "'{event_type}' payload must include '{field}', got: {:?}",
        ev.payload
    );
}

// --- Helpers -----------------------------------------------------------

/// Return the most-recently-received event of the given type, panicking
/// (rather than hanging) if none is present. `Then the webhook delivers
/// the X event` is expected to run first and block until X arrives.
async fn find_event(world: &RpWorld, event_type: &str) -> ReceivedEvent {
    let events = world.received_events.read().await;
    events
        .iter()
        .rev()
        .find(|e| e.event_type == event_type)
        .cloned()
        .unwrap_or_else(|| panic!("no '{event_type}' event received"))
}

async fn setup_webhook_receiver(world: &mut RpWorld) {
    if world.webhook_receiver.is_some() {
        return;
    }
    let (estimated, max) = world
        .webhook_ack_config
        .unwrap_or((Duration::from_secs(5), Duration::from_secs(10)));
    let events = world.received_events.clone();
    world.webhook_receiver = Some(WebhookReceiver::start(events, estimated, max).await);
}

/// Register a dedicated operation-event plugin subscribed to `events`.
/// A distinct plugin name keeps this isolated from
/// `event_delivery.feature`'s `test-event-plugin`.
fn add_event_plugin(world: &mut RpWorld, events: Vec<String>) {
    let url = world
        .webhook_receiver
        .as_ref()
        .expect("webhook receiver not started")
        .url
        .clone();
    world.plugin_configs.push(serde_json::json!({
        "name": "operation-event-plugin",
        "type": "event",
        "webhook_url": url,
        "subscribes_to": events,
    }));
}
