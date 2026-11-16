//! BDD step definitions for the guide camera guard
//! (`guide_camera_guard.feature`): another client of the guider's
//! starting it, a start of guiding in flight on a second MCP session,
//! and what a refusal sent the stub guider.
//!
//! Shared steps live elsewhere: the stub guider's Givens, `the stub
//! guider stops answering` and the request assertions in
//! `guider_steps.rs` and `guide_focus_steps.rs`, `another client stops
//! the stub guider's run and leaves it in state {string}` in
//! `guide_run_watch_steps.rs`, the guiding-train rigs and the train
//! lease calls in `train_lease_steps.rs`, the background capture and its
//! join in `motion_gate_steps.rs`, the rotator rig and move in
//! `rotator_steps.rs`, and the event waits in `event_steps.rs`.

use cucumber::{then, when};
use serde_json::json;

use crate::steps::guider_steps::guider_stub;
use crate::steps::motion_gate_steps::spawn_background_call;
use crate::world::RpWorld;

/// A client of the guider's other than rp starts it guiding: the guard
/// binds rp's own calls only.
#[when("another client starts the stub guider guiding")]
fn another_client_starts_guiding(world: &mut RpWorld) {
    guider_stub(world).set_state("guiding");
}

/// `start_guiding` with no arguments on a second MCP session. A scenario
/// follows it with the `guide_started` barrier before it meets the
/// start in flight (see [`spawn_background_call`]).
#[when("a second MCP client starts guiding in the background")]
async fn start_guiding_in_background(world: &mut RpWorld) {
    spawn_background_call(world, "start_guiding", json!({})).await;
}

/// Every request the stub guider received is a `GET`: rp read it and
/// asked nothing of it.
#[then("the stub guider should have received no request other than a read")]
fn stub_received_only_reads(world: &mut RpWorld) {
    let requests = guider_stub(world).requests();
    let others: Vec<_> = requests
        .iter()
        .filter(|request| request.method != "GET")
        .collect();
    assert!(
        others.is_empty(),
        "expected only reads, got {others:?}; all: {requests:?}"
    );
}
