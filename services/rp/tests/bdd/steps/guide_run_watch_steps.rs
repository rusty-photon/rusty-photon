//! BDD step definitions for the Guide Run Watch
//! (`guide_run_watch.feature`): what happens to the stub guider's run
//! behind rp's back, a stop it answers with `stop_timeout`, and how the
//! run ended.
//!
//! Shared steps live elsewhere: the stub guider's Givens, `the stub
//! guider restarts`, `the stub guider stops answering` and the request
//! assertions in `guider_steps.rs`, the event waits and payload
//! assertions in `event_steps.rs`, `no {string} event arrives within
//! {int} ms` in `train_lease_steps.rs`, and `exactly {int} {string}
//! event(s) should have been received` in `focuser_temperature_steps.rs`.

use std::time::Duration;

use cucumber::{given, then, when};
use serde_json::Value;

use crate::steps::guider_steps::guider_stub;
use crate::world::RpWorld;

#[given("the stub guider ends its run on a stop and answers the stop with stop_timeout")]
fn stub_guider_stop_times_out(world: &mut RpWorld) {
    guider_stub(world).stop_times_out();
}

#[when(expr = "the stub guider stops the run on its own with reason {string}")]
fn stub_guider_stops_on_its_own(world: &mut RpWorld, reason: String) {
    guider_stub(world).stop_on_its_own(&reason);
}

#[when(expr = "another client stops the stub guider's run and leaves it in state {string}")]
fn another_client_stops_the_run(world: &mut RpWorld, state: String) {
    guider_stub(world).stopped_by_another_client(&state);
}

#[when("the stub guider loses its guide star")]
fn stub_guider_loses_its_star(world: &mut RpWorld) {
    guider_stub(world).set_state("star_lost");
}

#[when("the stub guider finds its guide star again")]
fn stub_guider_finds_its_star(world: &mut RpWorld) {
    guider_stub(world).set_state("guiding");
}

#[then(expr = "the stub guider's run should have ended with a last stop of reason {string}")]
fn stub_guider_run_ended(world: &mut RpWorld, reason: String) {
    let stub = guider_stub(world);
    assert_eq!(stub.state(), "idle", "the stub guider's run has not ended");
    assert_eq!(
        stub.last_stop_reason().as_deref(),
        Some(reason.as_str()),
        "the stub guider's last stop"
    );
}

/// Watches the whole window for an `event` whose payload `reason` is
/// `reason`, failing at the first sighting (testing.md § 6.9).
#[then(expr = "no {string} event of reason {string} arrives within {int} ms")]
async fn no_event_of_reason_within(
    world: &mut RpWorld,
    event_type: String,
    reason: String,
    window_ms: u64,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(window_ms);
    while tokio::time::Instant::now() < deadline {
        let seen = world.received_events.read().await.iter().any(|e| {
            e.event_type == event_type
                && e.payload.get("reason").and_then(Value::as_str) == Some(reason.as_str())
        });
        assert!(
            !seen,
            "a '{event_type}' event of reason '{reason}' arrived inside the {window_ms} ms window"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
