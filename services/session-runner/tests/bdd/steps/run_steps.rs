//! Run-lifecycle step definitions shared by every feature: the simulator,
//! the flat plan the twilight document reads, and the `POST /runs` →
//! `GET /runs/{id}` observations. The process topology lives in
//! [`crate::steps::infrastructure`]; this file holds only the Gherkin
//! wiring the feature files have in common.

use std::time::Duration;

use cucumber::{given, then, when};

use crate::steps::infrastructure::ensure_omnisim;
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

/// Wait for the run to reach `complete`. Two minutes covers the shipped
/// documents' real-speed `OmniSim` work (slews, per-filter exposure
/// searches, park) with room for a loaded CI runner.
#[when("the workflow document runs to completion")]
async fn workflow_runs_to_completion(world: &mut SessionRunnerWorld) {
    let state = world.wait_for_run_end(Duration::from_mins(2)).await;
    assert_eq!(
        state.as_deref(),
        Some("complete"),
        "the workflow document did not complete within 120s (run: {:?})",
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
