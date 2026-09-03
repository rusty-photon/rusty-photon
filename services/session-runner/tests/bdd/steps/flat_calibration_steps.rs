//! BDD step definitions for the calibrator-flats workflow-document port.
//!
//! The scenarios spawn three processes: `OmniSim` (Alpaca simulator), rp
//! (equipment gateway + session orchestrator), and session-runner (the
//! generic document engine under test). The process topology lives in
//! [`crate::steps::infrastructure`]; this file holds only the Gherkin
//! step wiring and the flats-specific registration parameters.

use std::time::Duration;

use cucumber::{given, then, when};

use crate::steps::infrastructure::{
    add_event_plugin, configure_default_equipment, ensure_camera, ensure_cover_calibrator,
    ensure_omnisim, ensure_webhook_receiver, stage_run, start_rp_service,
    start_session_runner_service,
};
use crate::world::SessionRunnerWorld;

// ---------------------------------------------------------------------------
// Given steps
// ---------------------------------------------------------------------------

#[given("a running Alpaca simulator")]
async fn running_alpaca_simulator(world: &mut SessionRunnerWorld) {
    ensure_omnisim(world).await;
}

#[given(expr = "a flat plan of {int} {string} flats and {int} {string} flats")]
async fn flat_plan(
    world: &mut SessionRunnerWorld,
    count1: u32,
    filter1: String,
    count2: u32,
    filter2: String,
) {
    world.flat_plan = vec![(filter1, count1), (filter2, count2)];
}

#[given(expr = "a test webhook receiver subscribed to {string}")]
async fn webhook_receiver_subscribed_to(world: &mut SessionRunnerWorld, event_type: String) {
    ensure_webhook_receiver(world).await;
    add_event_plugin(world, vec![event_type]);
}

#[given(expr = "the cover starts {word}")]
async fn cover_starts(world: &mut SessionRunnerWorld, state: String) {
    ensure_omnisim(world).await;
    bdd_infra::rp_harness::OmniSimHandle::set_cover_closed(state == "closed")
        .await
        .unwrap_or_else(|e| panic!("failed to preset the cover {state}: {e}"));
}

#[given(expr = "a flat plan of {int} {string} flats with no filter wheel")]
async fn flat_plan_filterless(world: &mut SessionRunnerWorld, count: u32, group: String) {
    world.flat_plan = vec![(group, count)];
    world.no_filter_wheel = true;
}

#[given("rp is running with a camera, filter wheel, cover calibrator, and session-runner")]
async fn rp_running_with_equipment_and_session_runner(world: &mut SessionRunnerWorld) {
    configure_default_equipment(world).await;
    stage_calibrator_flats(world);
    start_rp_service(world).await;
    start_session_runner_service(world).await;
}

#[given("rp is running with a camera, cover calibrator, and session-runner")]
async fn rp_running_filterless_and_session_runner(world: &mut SessionRunnerWorld) {
    ensure_omnisim(world).await;
    ensure_camera(world);
    ensure_cover_calibrator(world);
    stage_calibrator_flats(world);
    start_rp_service(world).await;
    start_session_runner_service(world).await;
}

// ---------------------------------------------------------------------------
// When steps
// ---------------------------------------------------------------------------

/// Start the staged run at session-runner's own `POST /runs`; the
/// response's `run_id` / `session_id` drive every later observation.
#[when("a run is started")]
async fn start_run(world: &mut SessionRunnerWorld) {
    let body = world
        .run_request
        .clone()
        .expect("no run staged — the Given step must name a workflow");
    let url = format!("{}/runs", world.session_runner_url());
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&body)
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

#[when("the workflow document runs to completion")]
async fn workflow_runs_to_completion(world: &mut SessionRunnerWorld) {
    // Full workflow: close cover (~5s in OmniSim), calibrator on (~2s),
    // per-filter exposure search, batch captures, calibrator off (~2s),
    // open cover (~5s). Allow 120s total, matching the Rust
    // calibrator-flats suite this port must stay equivalent to.
    let state = world.wait_for_run_end(Duration::from_mins(2)).await;
    assert_eq!(
        state.as_deref(),
        Some("complete"),
        "the calibrator flats document did not complete within 120s (run: {:?})",
        world.run_record().await
    );
}

// ---------------------------------------------------------------------------
// Then steps
// ---------------------------------------------------------------------------

#[then(expr = "the run should report {string}")]
async fn run_should_report(world: &mut SessionRunnerWorld, expected: String) {
    let record = world
        .run_record()
        .await
        .expect("session-runner did not answer GET /runs/{id}");
    let actual = record
        .get("state")
        .and_then(|v| v.as_str())
        .expect("state field missing");
    assert_eq!(
        actual, expected,
        "expected the run to report '{expected}' but got '{actual}': {record}"
    );
}

#[then(expr = "the test webhook receiver should have received at least {int} {string} event(s)")]
async fn should_receive_at_least_n_events(
    world: &mut SessionRunnerWorld,
    count: usize,
    event_type: String,
) {
    assert!(
        world.wait_for_events(&event_type, count).await,
        "expected at least {count} '{event_type}' event(s) within timeout"
    );
}

#[then(expr = "the cover should be {word}")]
async fn cover_should_be(_world: &mut SessionRunnerWorld, expected: String) {
    let target = if expected == "closed" { 1 } else { 3 };
    let state = bdd_infra::rp_harness::OmniSimHandle::cover_state()
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

/// Register the shipped `calibrator_flats` document as the orchestrator's
/// workflow. Tolerance `1.0` and `max_iterations = 1` mirror the Rust
/// calibrator-flats suite: these scenarios verify end-to-end plumbing,
/// not convergence math (the engine's unit tests own that).
fn stage_calibrator_flats(world: &mut SessionRunnerWorld) {
    let filters: Vec<serde_json::Value> = world
        .flat_plan
        .iter()
        .map(|(name, count)| serde_json::json!({ "name": name, "count": count }))
        .collect();
    let mut parameters = serde_json::json!({
        "camera_id": "main-cam",
        "calibrator_id": "flat-panel",
        "target_adu_fraction": 0.5,
        "tolerance": 1.0,
        "max_iterations": 1,
        "initial_duration": "100ms",
        "filters": filters
    });
    // A filterless plan omits the parameter, exercising the document's
    // `""` default (set_filter is skipped).
    if !world.no_filter_wheel {
        parameters["filter_wheel_id"] = serde_json::json!("main-fw");
    }
    stage_run(world, "calibrator_flats", Some(parameters));
}
