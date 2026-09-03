//! BDD step definitions for the end-to-end calibrator-flats orchestrator
//! workflow.
//!
//! The scenarios spawn three processes: `OmniSim` (Alpaca simulator), rp
//! (equipment gateway), and calibrator-flats (the orchestrator being
//! tested), in that order — calibrator-flats' config names rp's MCP
//! endpoint. Runs start at calibrator-flats' own `POST /runs` and end on
//! its `GET /status`; rp keeps no session registry. All three are
//! coordinated via `bdd_infra::rp_harness` helpers; this file holds only
//! the Gherkin step wiring and the calibrator-flats-specific config
//! builder.

use std::time::Duration;

use bdd_infra::rp_harness::{
    build_calibrator_flats_config, start_rp, write_temp_config_file, OmniSimHandle, WebhookReceiver,
};
use bdd_infra::ServiceHandle;
use cucumber::{given, then, when};

use crate::world::CalibratorFlatsWorld;

// ---------------------------------------------------------------------------
// Given steps
// ---------------------------------------------------------------------------

#[given("a running Alpaca simulator")]
async fn running_alpaca_simulator(world: &mut CalibratorFlatsWorld) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
}

#[given(
    expr = "the calibrator-flats service is configured for {int} {string} flats and {int} {string} flats"
)]
async fn configure_calibrator_flats(
    world: &mut CalibratorFlatsWorld,
    count1: u32,
    filter1: String,
    count2: u32,
    filter2: String,
) {
    world.flat_plan = vec![(filter1, count1), (filter2, count2)];
}

#[given(expr = "a test webhook receiver subscribed to {string}")]
async fn webhook_receiver_subscribed_to(world: &mut CalibratorFlatsWorld, event_type: String) {
    ensure_webhook_receiver(world).await;
    add_event_plugin(world, vec![event_type]);
}

#[given(expr = "the cover starts {word}")]
async fn cover_starts(world: &mut CalibratorFlatsWorld, state: String) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
    OmniSimHandle::set_cover_closed(state == "closed")
        .await
        .unwrap_or_else(|e| panic!("failed to preset the cover {state}: {e}"));
}

#[given(
    expr = "the calibrator-flats service is configured for {int} {string} flats with no filter wheel"
)]
async fn configure_calibrator_flats_filterless(
    world: &mut CalibratorFlatsWorld,
    count: u32,
    group: String,
) {
    world.flat_plan = vec![(group, count)];
    world.no_filter_wheel = true;
}

#[given(
    "rp is running with a camera, filter wheel, cover calibrator, and the calibrator-flats orchestrator"
)]
async fn rp_running_with_equipment_and_calibrator_flats(world: &mut CalibratorFlatsWorld) {
    configure_default_equipment(world).await;
    start_rp_service(world).await;
    start_calibrator_flats_service(world).await;
}

#[given("rp is running with a camera, cover calibrator, and the calibrator-flats orchestrator")]
async fn rp_running_filterless_and_calibrator_flats(world: &mut CalibratorFlatsWorld) {
    configure_default_equipment(world).await;
    start_rp_service(world).await;
    start_calibrator_flats_service(world).await;
}

// ---------------------------------------------------------------------------
// When steps
// ---------------------------------------------------------------------------

/// Start a run at calibrator-flats' own `POST /runs`.
#[when("a run is started")]
async fn start_run(world: &mut CalibratorFlatsWorld) {
    let url = format!("{}/runs", world.calibrator_flats_url());
    let resp = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .expect("failed to POST /runs");

    world.last_api_status = Some(resp.status().as_u16());
    let text = resp
        .text()
        .await
        .expect("failed to read the /runs response");
    assert_eq!(
        world.last_api_status,
        Some(202),
        "POST /runs was not accepted: {text}"
    );
    world.last_api_body = serde_json::from_str(&text).ok();
}

/// Poll `GET /status` until the run has ended — `complete` or `error`;
/// which one is the scenario's next assertion.
#[when("the calibrator-flats run ends")]
async fn run_ends(world: &mut CalibratorFlatsWorld) {
    // Full workflow: close cover (~5s in OmniSim), calibrator on (~2s),
    // per-filter iterative exposure search (up to 5 iterations), batch
    // captures, calibrator off (~2s), open cover (~5s). Allow 120s total.
    let mut last = None;
    for _ in 0..480 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        last = world.status_phase().await;
        if matches!(last.as_deref(), Some("complete" | "error")) {
            return;
        }
    }
    panic!("the calibrator-flats run did not end within 120s (last phase: {last:?})");
}

// ---------------------------------------------------------------------------
// Then steps
// ---------------------------------------------------------------------------

#[then(expr = "the calibrator-flats status should report {string}")]
async fn status_should_report(world: &mut CalibratorFlatsWorld, expected: String) {
    let actual = world
        .status_phase()
        .await
        .expect("calibrator-flats did not answer GET /status");
    assert_eq!(
        actual, expected,
        "expected the calibrator-flats status to report '{expected}' but got '{actual}'"
    );
}

#[then(expr = "the test webhook receiver should have received at least {int} {string} event(s)")]
async fn should_receive_at_least_n_events(
    world: &mut CalibratorFlatsWorld,
    count: usize,
    event_type: String,
) {
    assert!(
        world.wait_for_events(&event_type, count).await,
        "expected at least {count} '{event_type}' event(s) within timeout"
    );
}

#[then(expr = "the cover should be {word}")]
async fn cover_should_be(_world: &mut CalibratorFlatsWorld, expected: String) {
    let target = if expected == "closed" { 1 } else { 3 };
    let state = OmniSimHandle::cover_state()
        .await
        .expect("failed to read the simulator's cover state");
    assert_eq!(
        state, target,
        "expected the cover to be {expected} (CoverState {target}), got CoverState {state}"
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn configure_default_equipment(world: &mut CalibratorFlatsWorld) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
    let alpaca_url = world.omnisim_url();

    if world.cameras.is_empty() {
        world.cameras.push(bdd_infra::rp_harness::CameraConfig {
            id: "main-cam".to_string(),
            alpaca_url: alpaca_url.clone(),
            device_number: 0,
            cooler_targets_c: Vec::new(),
        });
    }
    if !world.no_filter_wheel && world.filter_wheels.is_empty() {
        world
            .filter_wheels
            .push(bdd_infra::rp_harness::FilterWheelConfig {
                id: "main-fw".to_string(),
                alpaca_url: alpaca_url.clone(),
                device_number: 0,
                filters: vec![
                    "Luminance".to_string(),
                    "Red".to_string(),
                    "Green".to_string(),
                    "Blue".to_string(),
                ],
            });
    }
    if world.cover_calibrators.is_empty() {
        world
            .cover_calibrators
            .push(bdd_infra::rp_harness::CoverCalibratorConfig {
                id: "flat-panel".to_string(),
                alpaca_url,
                device_number: 0,
                poll_interval: Some(std::time::Duration::from_millis(100)),
            });
    }
}

async fn start_calibrator_flats_service(world: &mut CalibratorFlatsWorld) {
    if world.calibrator_flats.is_some() {
        return;
    }

    let filter_wheel = (!world.no_filter_wheel).then_some("main-fw");
    let mut config = build_calibrator_flats_config(&world.flat_plan, filter_wheel);
    // The service is an MCP client of rp: its config names the endpoint
    // every `POST /runs` run connects to.
    config["mcp_server_url"] = serde_json::json!(format!("{}/mcp", world.rp_url()));
    let config_path = write_temp_config_file("calibrator-flats-config", &config).await;

    world.calibrator_flats = Some(ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await);
}

async fn start_rp_service(world: &mut CalibratorFlatsWorld) {
    if world
        .rp
        .as_ref()
        .is_some_and(bdd_infra::ServiceHandle::is_running)
    {
        return;
    }

    let config = world.build_rp_config();
    world.rp = Some(start_rp(&config).await);

    assert!(
        world.wait_for_rp_healthy().await,
        "rp did not become healthy within timeout"
    );
}

async fn ensure_webhook_receiver(world: &mut CalibratorFlatsWorld) {
    if world.webhook_receiver.is_some() {
        return;
    }
    let (estimated, max) = world
        .webhook_ack_config
        .unwrap_or((Duration::from_secs(5), Duration::from_secs(10)));
    let events = world.received_events.clone();
    world.webhook_receiver = Some(WebhookReceiver::start(events, estimated, max).await);
}

fn add_event_plugin(world: &mut CalibratorFlatsWorld, events: Vec<String>) {
    let url = world
        .webhook_receiver
        .as_ref()
        .expect("webhook receiver not started")
        .url
        .clone();

    let already_exists = world
        .plugin_configs
        .iter()
        .any(|p| p.get("name").and_then(|v| v.as_str()) == Some("test-event-plugin"));

    if already_exists {
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
