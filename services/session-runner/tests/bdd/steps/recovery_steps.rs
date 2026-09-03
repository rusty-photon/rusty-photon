//! BDD step definitions for the resume contract (design § Re-entrancy
//! Contract, § Safety Behavior, § Runs): a run interrupted mid-way — the
//! engine killed, `rp` itself gone, or a safety monitor turning unsafe —
//! continues from the persisted blackboard when it resumes, without
//! repeating recorded work. Nobody re-invokes the engine: the killed
//! engine resumes its run manifest on restart, the outage and safety
//! pauses are waited out in-process, and every resume is observed on
//! session-runner's own `GET /runs/{id}`.
//!
//! The safety scenario exercises `rp`'s own machinery end-to-end (rp
//! cancels the in-flight call and refuses the run's gated calls; the
//! run waits on rp's safety status). The rp-outage scenario restarts rp
//! on the port the run was configured for, as a real restart would.

use std::time::Duration;

use cucumber::{given, then, when};

use bdd_infra::rp_harness::{OmniSimHandle, SafetyMonitorConfig};

use crate::steps::infrastructure::{
    ensure_omnisim, start_rp_service, start_session_runner_service,
};
use crate::steps::trigger_steps::settled_event_count;
use crate::world::SessionRunnerWorld;

#[when(expr = "the blackboard records at least {int} frames")]
async fn blackboard_records_frames(world: &mut SessionRunnerWorld, frames: u64) {
    crate::steps::observation::await_blackboard_counter(world, "frames", frames).await;
}

#[given("a safety monitor guards the session")]
async fn safety_monitor_guards_session(world: &mut SessionRunnerWorld) {
    ensure_omnisim(world).await;
    // Start from a known-safe reading regardless of what a previous
    // scenario (or crashed run) left in the simulator's memory.
    OmniSimHandle::set_safety_monitor_is_safe(true)
        .await
        .expect("failed to reset OmniSim's safety monitor to safe");
    world.safety_monitors.push(SafetyMonitorConfig {
        id: "weather-watcher".to_string(),
        alpaca_url: world.omnisim_url(),
        device_number: 0,
    });
    // Fast polling so rp detects the flips in test time, not the
    // production default (10 s).
    world.safety_poll_interval = Some(Duration::from_millis(250));
}

#[when("the safety monitor reports unsafe")]
async fn safety_monitor_reports_unsafe(_world: &mut SessionRunnerWorld) {
    OmniSimHandle::set_safety_monitor_is_safe(false)
        .await
        .expect("failed to flip OmniSim's safety monitor to unsafe");
}

#[when("the safety monitor reports safe again")]
async fn safety_monitor_reports_safe(_world: &mut SessionRunnerWorld) {
    OmniSimHandle::set_safety_monitor_is_safe(true)
        .await
        .expect("failed to flip OmniSim's safety monitor to safe");
}

/// Poll `GET /runs/{id}` until the run reports `expected`. When that is
/// `paused`, the engine has stopped writing, so the persisted frame
/// counter is stable — it is noted for "only the remaining frames".
#[then(expr = "the run reports {string} within {int} seconds")]
async fn run_reports_state(world: &mut SessionRunnerWorld, expected: String, seconds: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    let mut last = None;
    while std::time::Instant::now() < deadline {
        last = world.run_state().await;
        if last.as_deref() == Some(expected.as_str()) {
            if expected == "paused" {
                // Only the recovery fixture keeps a `session.frames`
                // counter; the deep-sky document counts elsewhere.
                world.frames_before_resume = world.blackboard_frames().await;
            }
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "the run never reported '{expected}' within {seconds}s (last: {last:?}, record: {:?})",
        world.run_record().await
    );
}

#[then(expr = "the run is paused for {string}")]
async fn run_is_paused_for(world: &mut SessionRunnerWorld, reason: String) {
    let record = world
        .run_record()
        .await
        .expect("session-runner did not answer GET /runs/{id}");
    assert_eq!(
        record.get("state").and_then(|v| v.as_str()),
        Some("paused"),
        "{record}"
    );
    assert_eq!(
        record.get("paused_reason").and_then(|v| v.as_str()),
        Some(reason.as_str()),
        "{record}"
    );
}

#[then("the blackboard is kept")]
async fn blackboard_is_kept(world: &mut SessionRunnerWorld) {
    assert!(
        world.blackboard_path().exists(),
        "a paused run must keep its blackboard for the resume"
    );
}

#[when("the session-runner is killed")]
async fn session_runner_is_killed(world: &mut SessionRunnerWorld) {
    world
        .session_runner
        .as_mut()
        .expect("session-runner not started")
        .kill()
        .await;
    world.session_runner = None;
}

#[when("the session-runner is restarted")]
async fn session_runner_is_restarted(world: &mut SessionRunnerWorld) {
    assert!(
        world.session_runner.is_none(),
        "restart follows a kill — the previous instance is still recorded as running"
    );
    // Reuses the scenario's state_dir, so the new process finds the old
    // one's blackboard and run manifest — and resumes the run by itself
    // (resume_on_start) — and its workflows_dir, so the document
    // resolves. The run id survives the restart: the manifest carries
    // it, so `GET /runs/{id}` keeps answering.
    start_session_runner_service(world).await;
}

#[when("rp is killed")]
async fn rp_is_killed(world: &mut SessionRunnerWorld) {
    world.rp.as_mut().expect("rp not started").kill().await;
    world.rp = None;
    // The SSE client died with rp; drop it so a later "watching rp's event
    // stream" step attaches a fresh one to the restarted instance.
    world.sse_client = None;
}

#[when("rp is restarted")]
async fn rp_is_restarted(world: &mut SessionRunnerWorld) {
    assert!(
        world.rp.is_none(),
        "restart follows a kill — the previous instance is still recorded as running"
    );
    // Same accumulated config, fresh process on the SAME port (pinned
    // from the first start), so the paused run's configured
    // mcp_server_url finds it — the restarted rp knows nothing of the
    // run; the engine's reconnect loop does all the resuming.
    start_rp_service(world).await;
}

#[then("the session-runner is still healthy and the blackboard is kept")]
async fn runner_healthy_blackboard_kept(world: &mut SessionRunnerWorld) {
    // With rp dead the tool transport is gone, so run progress is
    // physically impossible. What this step pins is the engine's
    // *reaction* to rp's loss: the process must survive and must not
    // tear down its persisted state. Both invariants are asserted
    // continuously across a window, so a crash or a blackboard deletion
    // is caught rather than slipping between a sleep and a single check.
    let handle = world
        .session_runner
        .as_ref()
        .expect("session-runner not started");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        let response = reqwest::get(format!("{}/health", handle.base_url))
            .await
            .expect("session-runner did not answer /health after rp died");
        assert!(response.status().is_success(), "{}", response.status());
        assert!(
            world.blackboard_path().exists(),
            "a paused run must keep its blackboard for the resume"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[then(expr = "the blackboard is deleted within {int} seconds")]
async fn blackboard_deleted_within(world: &mut SessionRunnerWorld, seconds: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline {
        if !world.blackboard_path().exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!(
        "the blackboard still exists after {seconds}s — the run did not end (record: {:?})",
        world.run_record().await
    );
}

#[then(
    expr = "the test webhook receiver should have received between {int} and {int} {string} events"
)]
async fn webhook_received_between(
    world: &mut SessionRunnerWorld,
    minimum: usize,
    maximum: usize,
    event_type: String,
) {
    // Wait for the floor, settle briefly so a straggler past the ceiling
    // would be observed, then bound the count.
    assert!(
        world.wait_for_events(&event_type, minimum).await,
        "expected at least {minimum} '{event_type}' event(s) within timeout"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    let events = world.received_events.read().await;
    let count = events.iter().filter(|e| e.event_type == event_type).count();
    assert!(
        (minimum..=maximum).contains(&count),
        "expected between {minimum} and {maximum} '{event_type}' events, saw {count}"
    );
}

#[then(expr = "the SSE stream should show between {int} and {int} {string} events")]
async fn sse_shows_between(
    world: &mut SessionRunnerWorld,
    minimum: usize,
    maximum: usize,
    event_type: String,
) {
    let count = settled_event_count(world, &event_type, minimum).await;
    assert!(
        (minimum..=maximum).contains(&count),
        "expected between {minimum} and {maximum} '{event_type}' events on the SSE \
         stream, saw {count}"
    );
}

#[then(expr = "the SSE stream should show only the remaining {string} events")]
async fn sse_shows_remaining(world: &mut SessionRunnerWorld, event_type: String) {
    let plan = world
        .run_request
        .as_ref()
        .and_then(|c| c.pointer("/params/plan"))
        .and_then(serde_json::Value::as_u64)
        .expect("the staged workflow carries no `plan` parameter");
    let frames = world
        .frames_before_resume
        .expect("no pre-resume frame count — add the 'the run reports \"paused\"' step first");
    let remaining = plan.checked_sub(frames).unwrap_or_else(|| {
        panic!("the blackboard records more frames ({frames}) than the plan ({plan})")
    });
    let remaining = usize::try_from(remaining).expect("plan fits usize");

    let count = settled_event_count(world, &event_type, remaining).await;
    assert_eq!(
        count, remaining,
        "expected exactly the {remaining} remaining '{event_type}' events \
         ({plan} planned, {frames} already recorded), saw {count}"
    );
}
