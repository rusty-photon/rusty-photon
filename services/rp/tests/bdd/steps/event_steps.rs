//! BDD step definitions for event delivery feature

use std::time::Duration;

use cucumber::{given, then, when};

use bdd_infra::rp_harness::WebhookReceiver;

use crate::world::RpWorld;

// --- Given steps ---

#[given(expr = "a test webhook receiver subscribed to {string}")]
async fn webhook_receiver_subscribed_to(world: &mut RpWorld, event_type: String) {
    setup_webhook_receiver(world).await;
    add_event_plugin(world, vec![event_type]);
}

#[given(expr = "a test webhook receiver subscribed to {string} and {string}")]
async fn webhook_receiver_subscribed_to_two(world: &mut RpWorld, event1: String, event2: String) {
    setup_webhook_receiver(world).await;
    add_event_plugin(world, vec![event1, event2]);
}

/// Comma-separated subscription list, for scenarios that assert
/// ordering across more than two event types (`motion_gate.feature`).
#[given(expr = "a test webhook receiver subscribed to the events {string}")]
async fn webhook_receiver_subscribed_to_list(world: &mut RpWorld, event_types: String) {
    setup_webhook_receiver(world).await;
    add_event_plugin(
        world,
        event_types
            .split(',')
            .map(|s| s.trim().to_string())
            .collect(),
    );
}

#[given(
    expr = "the test webhook receiver acknowledges with estimated {int} seconds and max {int} seconds"
)]
const fn webhook_ack_config(world: &mut RpWorld, estimated: i32, max: i32) {
    world.webhook_ack_config = Some((
        Duration::from_secs(estimated as u64),
        Duration::from_secs(max as u64),
    ));
}

/// An auth-enabled subscriber: the receiver 401s anything that does not
/// present exactly this credential, the way a plugin serving behind
/// `rp_auth`'s middleware does. rp reaches it only when the registration
/// carries a matching `auth` block.
#[given(
    expr = "a test webhook receiver requiring the credential {string} with password {string} subscribed to {string}"
)]
async fn webhook_receiver_requiring_auth(
    world: &mut RpWorld,
    username: String,
    password: String,
    event_type: String,
) {
    assert!(
        world.webhook_receiver.is_none(),
        "the auth-requiring receiver must be the scenario's first"
    );
    let (estimated, max) = world
        .webhook_ack_config
        .unwrap_or((Duration::from_secs(5), Duration::from_secs(10)));
    world.webhook_receiver = Some(
        WebhookReceiver::start_requiring_auth(
            world.received_events.clone(),
            estimated,
            max,
            &username,
            &password,
        )
        .await,
    );
    add_event_plugin(world, vec![event_type]);
}

#[given(
    expr = "the event plugin registration carries the credential {string} with password {string}"
)]
fn event_plugin_registration_carries_credential(
    world: &mut RpWorld,
    username: String,
    password: String,
) {
    let entry = world
        .plugin_configs
        .iter_mut()
        .find(|p| p.get("name").and_then(|v| v.as_str()) == Some("test-event-plugin"))
        .expect("the event plugin registration must exist before its credential is set");
    entry
        .as_object_mut()
        .expect("a plugin registration is a JSON object")
        .insert(
            "auth".to_string(),
            serde_json::json!({ "username": username, "password": password }),
        );
}

#[given(expr = "a plugin configured with webhook URL {string} subscribed to {string}")]
fn plugin_with_url(world: &mut RpWorld, webhook_url: String, event_type: String) {
    world.plugin_configs.push(serde_json::json!({
        "name": "unreachable-plugin",
        "type": "event",
        "webhook_url": webhook_url,
        "subscribes_to": [event_type]
    }));
}

#[given("rp is running with a camera on the simulator and the test plugin")]
async fn rp_running_with_camera_and_plugin(world: &mut RpWorld) {
    crate::steps::tool_steps::ensure_omnisim(world).await;
    crate::steps::tool_steps::add_camera(world);
    crate::steps::tool_steps::start_rp(world).await;
}

#[given("rp is running with a filter wheel on the simulator and the test plugin")]
async fn rp_running_with_fw_and_plugin(world: &mut RpWorld) {
    crate::steps::tool_steps::ensure_omnisim(world).await;
    crate::steps::tool_steps::add_filter_wheel(world);
    crate::steps::tool_steps::start_rp(world).await;
}

#[given("rp is running with a camera on the simulator and the unreachable plugin")]
async fn rp_running_with_camera_and_unreachable_plugin(world: &mut RpWorld) {
    crate::steps::tool_steps::ensure_omnisim(world).await;
    crate::steps::tool_steps::add_camera(world);
    crate::steps::tool_steps::start_rp(world).await;
}

// --- Then steps ---

#[then(expr = "the test webhook receiver should receive an {string} event")]
async fn should_receive_event(world: &mut RpWorld, event_type: String) {
    assert!(
        world.wait_for_events(&event_type, 1).await,
        "expected to receive '{event_type}' event within timeout"
    );
}

#[then(expr = "the test webhook receiver should receive a {string} event")]
async fn should_receive_event_a(world: &mut RpWorld, event_type: String) {
    assert!(
        world.wait_for_events(&event_type, 1).await,
        "expected to receive '{event_type}' event within timeout"
    );
}

#[then("the event payload should contain the document id")]
async fn event_has_document_id(world: &mut RpWorld) {
    let events = world.received_events.read().await;
    let event = events
        .iter()
        .find(|e| e.event_type == "exposure_complete")
        .expect("no exposure_complete event found");

    assert!(
        event.payload.get("document_id").is_some()
            || event
                .payload
                .get("document")
                .and_then(|d| d.get("id"))
                .is_some(),
        "expected document id in event payload, got: {:?}",
        event.payload
    );
}

#[then("the event payload should contain the file path")]
async fn event_has_file_path(world: &mut RpWorld) {
    let events = world.received_events.read().await;
    let event = events
        .iter()
        .find(|e| e.event_type == "exposure_complete")
        .expect("no exposure_complete event found");

    assert!(
        event
            .payload
            .get("file_path")
            .and_then(|v| v.as_str())
            .is_some(),
        "expected file_path in event payload, got: {:?}",
        event.payload
    );
}

#[then(expr = "{string} should have been received before {string}")]
async fn event_order(world: &mut RpWorld, first: String, second: String) {
    let events = world.received_events.read().await;

    let first_time = events
        .iter()
        .find(|e| e.event_type == first)
        .map_or_else(|| panic!("event '{first}' not received"), |e| e.received_at);

    let second_time = events.iter().find(|e| e.event_type == second).map_or_else(
        || panic!("event '{second}' not received"),
        |e| e.received_at,
    );

    assert!(
        first_time < second_time,
        "expected '{first}' before '{second}', but '{first}' arrived at {first_time:?} and '{second}' at {second_time:?}"
    );
}

#[then("rp should have recorded the plugin timing estimates")]
async fn timing_estimates_recorded(world: &mut RpWorld) {
    // Verify by checking that the event was delivered and acknowledged.
    // The actual recording is internal to rp — we verify it didn't error.
    let events = world.received_events.read().await;
    assert!(
        !events.is_empty(),
        "expected at least one event to have been delivered and acknowledged"
    );
}

#[then(expr = "the {string} event payload field {string} should be {string}")]
async fn event_payload_field_equals(
    world: &mut RpWorld,
    event_type: String,
    field: String,
    expected: String,
) {
    // Webhook delivery is asynchronous — the POST may still be in
    // flight when the tool call returns, so wait for the event
    // before reading it.
    assert!(
        world.wait_for_events(&event_type, 1).await,
        "expected to receive '{event_type}' event within timeout"
    );
    let events = world.received_events.read().await;
    let event = events
        .iter()
        .find(|e| e.event_type == event_type)
        .unwrap_or_else(|| panic!("no '{event_type}' event found"));

    let actual = event
        .payload
        .get(&field)
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            panic!(
                "expected string field '{}' in '{}' event payload, got: {:?}",
                field, event_type, event.payload
            )
        });
    assert_eq!(
        actual, expected,
        "expected '{event_type}' payload field '{field}' to be '{expected}', got '{actual}'"
    );
}

#[then(expr = "the {string} event payload should contain a {string}")]
async fn event_payload_contains_field(world: &mut RpWorld, event_type: String, field: String) {
    // Same asynchronous-delivery wait as `event_payload_field_equals`.
    assert!(
        world.wait_for_events(&event_type, 1).await,
        "expected to receive '{event_type}' event within timeout"
    );
    let events = world.received_events.read().await;
    let event = events
        .iter()
        .find(|e| e.event_type == event_type)
        .unwrap_or_else(|| panic!("no '{event_type}' event found"));

    assert!(
        event.payload.get(&field).is_some(),
        "expected '{}' in '{}' event payload, got: {:?}",
        field,
        event_type,
        event.payload
    );
}

/// Whether two JSON values agree, comparing numbers by value so a
/// feature file's `20.0` matches a serialized `20` or `20.0` alike.
pub fn json_values_match(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    match (actual.as_f64(), expected.as_f64()) {
        (Some(a), Some(e)) => a == e,
        _ => actual == expected,
    }
}

/// A regex sibling of the string-field step for values a `{string}`
/// cannot carry: numbers, booleans, `null`, and nested JSON.
#[then(regex = r#"^the "([^"]+)" event payload field "([^"]+)" should be the JSON (.+)$"#)]
async fn event_payload_field_is_json(
    world: &mut RpWorld,
    event_type: String,
    field: String,
    expected: String,
) {
    let expected: serde_json::Value =
        serde_json::from_str(&expected).expect("the expected value must be JSON");
    assert!(
        world.wait_for_events(&event_type, 1).await,
        "expected to receive '{event_type}' event within timeout"
    );
    let events = world.received_events.read().await;
    let event = events
        .iter()
        .find(|e| e.event_type == event_type)
        .unwrap_or_else(|| panic!("no '{event_type}' event found"));
    let actual = event.payload.get(&field).unwrap_or_else(|| {
        panic!(
            "expected field '{field}' in '{event_type}' event payload, got: {:?}",
            event.payload
        )
    });
    assert!(
        json_values_match(actual, &expected),
        "expected '{event_type}' payload field '{field}' to be {expected}, got {actual}"
    );
}

#[then(expr = "the {string} event payload field {string} should contain {string}")]
async fn event_payload_field_contains(
    world: &mut RpWorld,
    event_type: String,
    field: String,
    needle: String,
) {
    assert!(
        world.wait_for_events(&event_type, 1).await,
        "expected to receive '{event_type}' event within timeout"
    );
    let events = world.received_events.read().await;
    let event = events
        .iter()
        .find(|e| e.event_type == event_type)
        .unwrap_or_else(|| panic!("no '{event_type}' event found"));
    let actual = event
        .payload
        .get(&field)
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            panic!(
                "expected string field '{field}' in '{event_type}' event payload, got: {:?}",
                event.payload
            )
        });
    assert!(
        actual.contains(&needle),
        "expected '{event_type}' payload field '{field}' to contain '{needle}', got '{actual}'"
    );
}

#[then(expr = "the {string} event payload should not contain a {string}")]
async fn event_payload_lacks_field(world: &mut RpWorld, event_type: String, field: String) {
    assert!(
        world.wait_for_events(&event_type, 1).await,
        "expected to receive '{event_type}' event within timeout"
    );
    let events = world.received_events.read().await;
    let event = events
        .iter()
        .find(|e| e.event_type == event_type)
        .unwrap_or_else(|| panic!("no '{event_type}' event found"));
    assert!(
        event.payload.get(&field).is_none(),
        "expected no '{}' in '{}' event payload, got: {:?}",
        field,
        event_type,
        event.payload
    );
}

// --- Mid-scenario waits (When keyword) ---------------------------------
//
// The `should receive` assertions above are `#[then]` steps, which
// cucumber only matches under a Then keyword. The motion-gate
// scenarios need to *wait* for an event mid-When (e.g. hold off the
// dither call until the background capture's `exposure_started`
// arrives), so the same wait is also exposed as a When step.

#[when(expr = "the test webhook receiver has received an {string} event")]
async fn has_received_event_an(world: &mut RpWorld, event_type: String) {
    assert!(
        world.wait_for_events(&event_type, 1).await,
        "expected to receive '{event_type}' event within timeout"
    );
}

#[when(expr = "the test webhook receiver has received a {string} event")]
async fn has_received_event_a(world: &mut RpWorld, event_type: String) {
    assert!(
        world.wait_for_events(&event_type, 1).await,
        "expected to receive '{event_type}' event within timeout"
    );
}

// --- Emission-order assertions (event_seq based) -----------------------
//
// `{string} should have been received before {string}` above compares
// webhook *arrival* instants, which is fine when emissions are far
// apart but racy for back-to-back emissions (delivery POSTs are
// concurrent). These variants compare the envelope's monotonic
// `event_seq` — the emission order itself — so a gate release
// followed microseconds later by the queued motion's `*_started`
// still asserts deterministically.

/// Wait for at least one event of the given type, then return the
/// lowest `event_seq` among them (the first emission).
async fn first_seq_of(world: &mut RpWorld, event_type: &str) -> u64 {
    assert!(
        world.wait_for_events(event_type, 1).await,
        "expected to receive '{event_type}' event within timeout"
    );
    let events = world.received_events.read().await;
    events
        .iter()
        .filter(|e| e.event_type == event_type)
        .filter_map(|e| e.event_seq)
        .min()
        .unwrap_or_else(|| panic!("'{event_type}' events carried no event_seq"))
}

#[then(expr = "the {string} event should have been emitted before the {string} event")]
async fn event_emitted_before(world: &mut RpWorld, first: String, second: String) {
    let first_seq = first_seq_of(world, &first).await;
    let second_seq = first_seq_of(world, &second).await;
    assert!(
        first_seq < second_seq,
        "expected '{first}' (seq {first_seq}) to be emitted before '{second}' (seq {second_seq})"
    );
}

#[then(expr = "the last {string} event should have been emitted after the {string} event")]
async fn last_event_emitted_after(world: &mut RpWorld, first: String, second: String) {
    let second_seq = first_seq_of(world, &second).await;
    // Webhook delivery is asynchronous, so the later `{first}`
    // emission this step is really about may still be in flight when
    // it runs — poll until a `{first}` with a sequence above
    // `{second}`'s arrives instead of snapshotting the current max
    // (10 s bound, matching `wait_for_events`).
    let mut observed_max: Option<u64> = None;
    for _ in 0..40 {
        {
            let events = world.received_events.read().await;
            observed_max = events
                .iter()
                .filter(|e| e.event_type == first)
                .filter_map(|e| e.event_seq)
                .max();
        }
        if observed_max.is_some_and(|last_seq| last_seq > second_seq) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!(
        "expected a '{first}' emitted after '{second}' (seq {second_seq}); \
         highest '{first}' seq observed: {observed_max:?}"
    );
}

#[then(expr = "the test webhook receiver should not have received a {string} event")]
async fn should_not_receive_event_of_type(world: &mut RpWorld, event_type: String) {
    // Absence cannot be polled for; give late deliveries a grace
    // window before asserting (the emission under test, if it
    // happened at all, predates the joined background calls by the
    // stub's settle delay).
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let events = world.received_events.read().await;
    let matching: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == event_type)
        .collect();
    assert!(
        matching.is_empty(),
        "expected no '{}' events, but received {}",
        event_type,
        matching.len()
    );
}

#[then("the test webhook receiver should not have received any events")]
async fn should_not_receive_events(world: &mut RpWorld) {
    // Wait briefly to ensure no events arrive (cannot poll for absence)
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let events = world.received_events.read().await;
    assert!(
        events.is_empty(),
        "expected no events, but received: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
}

#[then(expr = "the test webhook receiver should have received {int} {string} events")]
async fn should_receive_n_events(world: &mut RpWorld, count: usize, event_type: String) {
    assert!(
        world.wait_for_events(&event_type, count).await,
        "expected {count} '{event_type}' events within timeout"
    );

    let events = world.received_events.read().await;
    let actual = events.iter().filter(|e| e.event_type == event_type).count();
    assert_eq!(
        actual, count,
        "expected exactly {count} '{event_type}' events, got {actual}"
    );
}

#[then(expr = "the test webhook receiver should have received at least {int} {string} event(s)")]
async fn should_receive_at_least_n_events(world: &mut RpWorld, count: usize, event_type: String) {
    assert!(
        world.wait_for_events(&event_type, count).await,
        "expected at least {count} '{event_type}' event(s) within timeout"
    );
}

// --- Helpers ---

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

fn add_event_plugin(world: &mut RpWorld, events: Vec<String>) {
    let url = world
        .webhook_receiver
        .as_ref()
        .expect("webhook receiver not started")
        .url
        .clone();

    // Only add plugin config if not already present
    let already_exists = world
        .plugin_configs
        .iter()
        .any(|p| p.get("name").and_then(|v| v.as_str()) == Some("test-event-plugin"));

    if already_exists {
        // Update subscriptions on existing config
        if let Some(config) = world
            .plugin_configs
            .iter_mut()
            .find(|p| p.get("name").and_then(|v| v.as_str()) == Some("test-event-plugin"))
        {
            let existing = config
                .get("subscribes_to")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            let mut merged = existing;
            for e in events {
                if !merged.contains(&e) {
                    merged.push(e);
                }
            }
            config["subscribes_to"] = serde_json::json!(merged);
        }
    } else {
        world.plugin_configs.push(serde_json::json!({
            "name": "test-event-plugin",
            "type": "event",
            "webhook_url": url,
            "subscribes_to": events
        }));
    }
}
