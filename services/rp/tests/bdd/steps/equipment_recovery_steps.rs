//! BDD step definitions for equipment session recovery (rp.md § Device
//! Session Recovery): the reconnect supervisor re-establishes device
//! sessions that a downstream service restart killed, and picks up
//! devices that were unreachable when rp started.
//!
//! The downstream service is [`AlpacaDeviceStub`] — a restartable
//! in-process Alpaca server the scenario stops and brings back on the
//! same port, which the shared `OmniSim` instance must never do
//! mid-run.

use std::time::Duration;

use cucumber::{given, then, when};

use bdd_infra::rp_harness::{AlpacaDeviceStub, CameraConfig, SafetyMonitorConfig, StubDevice};

use crate::world::RpWorld;

/// How long a poll-until-observed step waits before failing the
/// scenario. A floor, not an estimate: sanitizer-instrumented binaries
/// run the supervisor and its connect retries several times slower
/// than a dev build, and every loop below exits as soon as the
/// condition holds.
const OBSERVATION_BUDGET: Duration = Duration::from_secs(15);

/// Config id used by the stub-backed safety monitor scenarios.
const STUB_MONITOR_ID: &str = "stub-monitor";

// --- Given steps ---

#[given("a stub Alpaca service hosting a safety monitor")]
async fn stub_hosting_safety_monitor(world: &mut RpWorld) {
    world.alpaca_stub = Some(AlpacaDeviceStub::start(StubDevice::SafetyMonitor));
}

#[given("a stub Alpaca service hosting a camera, currently stopped")]
async fn stub_hosting_camera_stopped(world: &mut RpWorld) {
    world.alpaca_stub = Some(AlpacaDeviceStub::start_stopped(StubDevice::Camera).await);
}

#[given("rp is configured with a safety monitor on the stub service")]
fn configured_with_stub_safety_monitor(world: &mut RpWorld) {
    let url = stub(world).url();
    world.safety_monitors.push(SafetyMonitorConfig {
        id: STUB_MONITOR_ID.to_string(),
        alpaca_url: url,
        device_number: 0,
    });
    // Fast safety polling so the unsafe/safe transitions around the
    // service restart are detected in test time, not the production
    // default (10 s) — same pinning as safety_steps.
    world.safety_poll_interval = Some(Duration::from_millis(250));
}

#[given("rp is configured with a camera on the stub service")]
fn configured_with_stub_camera(world: &mut RpWorld) {
    let url = stub(world).url();
    world.cameras.push(CameraConfig {
        id: "main-cam".to_string(),
        alpaca_url: url,
        device_number: 0,
        cooler_targets_c: Vec::new(),
    });
}

#[given(expr = "an equipment reconnect interval of {int} milliseconds")]
const fn reconnect_interval(world: &mut RpWorld, millis: u64) {
    world.reconnect_interval = Some(Duration::from_millis(millis));
}

// --- When steps ---

#[when("the stub Alpaca service stops")]
async fn stub_service_stops(world: &mut RpWorld) {
    stub_mut(world).stop().await;
}

#[when("the stub Alpaca service comes back with its session state lost")]
async fn stub_service_comes_back(world: &mut RpWorld) {
    stub_mut(world).restart().await;
}

// --- Then steps ---

#[then("the equipment status should show the stub safety monitor as connected")]
async fn stub_monitor_connected_now(world: &mut RpWorld) {
    assert_eq!(
        device_connected(world, "safety_monitors", STUB_MONITOR_ID).await,
        Some(true),
        "expected {STUB_MONITOR_ID} to be connected"
    );
}

#[then(
    expr = "the equipment status should show the stub safety monitor as connected within {int} seconds"
)]
async fn stub_monitor_connected_within(world: &mut RpWorld, seconds: u64) {
    poll_device_connected(world, "safety_monitors", STUB_MONITOR_ID, true, seconds).await;
}

#[then(
    expr = "the equipment status should show the stub safety monitor as disconnected within {int} seconds"
)]
async fn stub_monitor_disconnected_within(world: &mut RpWorld, seconds: u64) {
    poll_device_connected(world, "safety_monitors", STUB_MONITOR_ID, false, seconds).await;
}

#[then(expr = "the equipment status should show the camera as connected within {int} seconds")]
async fn camera_connected_within(world: &mut RpWorld, seconds: u64) {
    poll_device_connected(world, "cameras", "main-cam", true, seconds).await;
}

#[then(expr = "an {string} event should report the device {string} as connected")]
async fn event_reports_device_connected(world: &mut RpWorld, event_type: String, device: String) {
    wait_for_equipment_event(world, &event_type, &device, true).await;
}

#[then(expr = "an {string} event should report the device {string} as disconnected")]
async fn event_reports_device_disconnected(
    world: &mut RpWorld,
    event_type: String,
    device: String,
) {
    wait_for_equipment_event(world, &event_type, &device, false).await;
}

#[then("the safety monitor should report safe again without an rp restart")]
async fn safety_monitor_safe_again(world: &mut RpWorld) {
    assert!(
        world.rp.is_some(),
        "rp is not running — the scenario must not have restarted or stopped it"
    );
    // A "safe" transition strictly after the restart-induced "unsafe"
    // one proves the poll loop reads through the re-established
    // session; the pre-restart steady state emits no event at all
    // (transitions only), so ordering is what discriminates.
    let deadline = std::time::Instant::now() + OBSERVATION_BUDGET;
    loop {
        {
            let events = world.received_events.read().await;
            let transitions: Vec<&str> = events
                .iter()
                .filter(|e| e.event_type == "safety_changed")
                .filter_map(|e| e.payload.get("new_state").and_then(|v| v.as_str()))
                .collect();
            let unsafe_at = transitions.iter().position(|s| *s == "unsafe");
            if let Some(idx) = unsafe_at {
                if transitions.iter().skip(idx).any(|s| *s == "safe") {
                    return;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no safe-after-unsafe transition within {OBSERVATION_BUDGET:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// --- helpers ---

const fn stub(world: &RpWorld) -> &AlpacaDeviceStub {
    world
        .alpaca_stub
        .as_ref()
        .expect("no stub Alpaca service — add a 'Given a stub Alpaca service ...' step")
}

const fn stub_mut(world: &mut RpWorld) -> &mut AlpacaDeviceStub {
    world
        .alpaca_stub
        .as_mut()
        .expect("no stub Alpaca service — add a 'Given a stub Alpaca service ...' step")
}

/// One `GET /api/equipment` read of the `connected` flag for `id` in
/// the array at `array_key`. `None` when the request failed or the
/// device is absent — distinct from a definite `Some(false)`.
async fn device_connected(world: &RpWorld, array_key: &str, id: &str) -> Option<bool> {
    let url = format!("{}/api/equipment", world.rp_url());
    let body: serde_json::Value = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    body.get(array_key)?
        .as_array()?
        .iter()
        .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(id))?
        .get("connected")?
        .as_bool()
}

/// Poll `GET /api/equipment` until the device's `connected` flag reads
/// `expected`, failing the scenario after `seconds`.
async fn poll_device_connected(
    world: &RpWorld,
    array_key: &str,
    id: &str,
    expected: bool,
    seconds: u64,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        if device_connected(world, array_key, id).await == Some(expected) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "equipment status never showed {id} connected={expected} within {seconds}s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait for an event of `event_type` whose payload names `device` with
/// the given `connected` value.
async fn wait_for_equipment_event(
    world: &RpWorld,
    event_type: &str,
    device: &str,
    connected: bool,
) {
    let deadline = std::time::Instant::now() + OBSERVATION_BUDGET;
    loop {
        {
            let events = world.received_events.read().await;
            let found = events.iter().any(|e| {
                e.event_type == event_type
                    && e.payload.get("device").and_then(|v| v.as_str()) == Some(device)
                    && e.payload
                        .get("connected")
                        .and_then(serde_json::Value::as_bool)
                        == Some(connected)
            });
            if found {
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no '{event_type}' event with device={device} connected={connected} within {OBSERVATION_BUDGET:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
