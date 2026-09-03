//! BDD step definitions for the event-subscription contract: the engine's
//! SSE intake and the `wait.until_event` instruction, exercised through
//! purpose-built fixture documents (`tests/fixtures/workflows/`).

use std::time::Duration;

use cucumber::{given, then};

use crate::steps::infrastructure::{
    configure_default_equipment, stage_run, start_rp_service, start_session_runner_service,
};
use crate::world::SessionRunnerWorld;

#[given(expr = "rp is running with a camera and session-runner running the {string} workflow")]
async fn rp_running_with_fixture_workflow(world: &mut SessionRunnerWorld, workflow: String) {
    configure_default_equipment(world).await;
    let parameters = match workflow.as_str() {
        "wait_for_exposure_event" => Some(serde_json::json!({ "camera_id": "main-cam" })),
        "wait_for_missing_event" => None,
        "trigger_between_exposures" | "trigger_once" | "trigger_cooldown" => {
            Some(serde_json::json!({ "camera_id": "main-cam", "filter_wheel_id": "main-fw" }))
        }
        "trigger_poll" => Some(serde_json::json!({ "filter_wheel_id": "main-fw" })),
        // 4 × 2s exposures: slow enough to interrupt mid-loop after two
        // recorded frames, fast enough for the suite's time budget.
        "recovery_capture_loop" => Some(serde_json::json!({
            "camera_id": "main-cam", "filter_wheel_id": "main-fw", "plan": 4
        })),
        other => panic!("no run parameters defined for fixture workflow `{other}`"),
    };
    stage_run(world, &workflow, parameters);
    start_rp_service(world).await;
    start_session_runner_service(world).await;
}

/// The run reaches a terminal state — complete, failed, or stopped —
/// within the budget; which one is the scenario's next assertion.
#[then(expr = "the run ends within {int} seconds")]
async fn run_ends_within(world: &mut SessionRunnerWorld, seconds: u64) {
    assert!(
        world
            .wait_for_run_end(Duration::from_secs(seconds))
            .await
            .is_some(),
        "expected the run to end within {seconds}s (run: {:?})",
        world.run_record().await
    );
}
