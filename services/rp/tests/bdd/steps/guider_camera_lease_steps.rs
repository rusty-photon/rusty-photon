//! BDD step definitions for the guider's camera lease as rp holds it
//! (`guider_camera_lease.feature`): asserted on what the stub guider
//! recorded of its camera-lease endpoints.
//!
//! Shared steps live elsewhere: the stub guider's Givens, `the stub
//! guider restarts`, `the stub guider stops answering` and `the stub
//! guider should not have received a {string} request` in
//! `guider_steps.rs` and `guide_focus_steps.rs`, the guiding-train rigs
//! and the train lease calls in `train_lease_steps.rs`, the background
//! capture in `motion_gate_steps.rs`.

use std::time::Duration;

use cucumber::{given, then};

use bdd_infra::rp_harness::{GuiderStub, LeaseCall};

use crate::steps::guider_steps::guider_stub;
use crate::world::RpWorld;

#[given("the stub guider grants a camera lease and never answers the request")]
fn stub_grants_lease_and_never_answers(world: &mut RpWorld) {
    guider_stub(world).grant_lease_and_never_answer();
}

#[then(
    expr = "the stub guider should hold a camera lease under a token of 32 lowercase hex digits with a ttl of {string}"
)]
fn stub_holds_lease(world: &mut RpWorld, ttl: String) {
    let ttl = humantime::parse_duration(&ttl)
        .unwrap_or_else(|e| panic!("the scenario's ttl {ttl:?} is not a duration: {e}"));
    let stub = guider_stub(world);
    let lease = stub.lease().unwrap_or_else(|| {
        panic!(
            "the stub guider holds no camera lease; calls: {:?}",
            stub.lease_calls()
        )
    });
    assert!(
        lease.token.len() == 32
            && lease
                .token
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "the camera lease's token {:?} is not 32 lowercase hex digits",
        lease.token
    );
    assert_eq!(lease.ttl, ttl, "the camera lease's ttl");
}

/// The lease is given back once the train lease or the capture has
/// given everything back, which may follow the call's answer: the step
/// waits a few seconds for the stub to hold none.
#[then("the stub guider should hold no camera lease")]
async fn stub_holds_no_lease(world: &mut RpWorld) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Some(lease) = guider_stub(world).lease() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the stub guider still holds a camera lease under {:?}; calls: {:?}",
            lease.token,
            guider_stub(world).lease_calls()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[then("the stub guider's last camera lease release should name the token it granted")]
fn last_release_names_granted_token(world: &mut RpWorld) {
    let stub = guider_stub(world);
    let granted = last_granted_token(stub);
    let released = stub.released_tokens();
    assert_eq!(
        released.last(),
        Some(&granted),
        "the last camera lease release does not name the granted token; calls: {:?}",
        stub.lease_calls()
    );
}

#[then(
    expr = "the stub guider should have received a camera lease release naming the token it granted within {int} seconds"
)]
async fn release_of_granted_token_within(world: &mut RpWorld, seconds: u64) {
    let granted = last_granted_token(guider_stub(world));
    await_lease_call(
        world,
        seconds,
        "release naming the granted token",
        |call| matches!(call, LeaseCall::Release { token, .. } if *token == granted),
    )
    .await;
}

#[then(
    expr = "the stub guider should receive a renewal of the camera lease it granted within {int} seconds"
)]
async fn renewal_of_granted_lease_within(world: &mut RpWorld, seconds: u64) {
    let granted = last_granted_token(guider_stub(world));
    await_lease_call(
        world,
        seconds,
        "renewal of the granted lease",
        |call| matches!(call, LeaseCall::Renew { token, .. } if *token == granted),
    )
    .await;
}

/// Every `POST /camera/lease` counts, granted or refused.
#[then(expr = "the stub guider should have been asked for {int} camera lease(s)")]
fn stub_asked_for_leases(world: &mut RpWorld, count: usize) {
    let acquires = guider_stub(world).requests_to("/camera/lease");
    assert_eq!(
        acquires.len(),
        count,
        "expected {count} camera lease acquire(s), got {acquires:?}"
    );
}

// --- Helpers ---------------------------------------------------------

/// The token of the latest acquire the stub granted.
fn last_granted_token(stub: &GuiderStub) -> String {
    stub.granted_tokens().pop().unwrap_or_else(|| {
        panic!(
            "the stub guider granted no camera lease; calls: {:?}",
            stub.lease_calls()
        )
    })
}

/// Waits, at most `seconds`, for a camera-lease call that `wanted`
/// matches.
async fn await_lease_call(
    world: &RpWorld,
    seconds: u64,
    what: &str,
    wanted: impl Fn(&LeaseCall) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    while !guider_stub(world).lease_calls().iter().any(&wanted) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the stub guider received no {what} within {seconds} s; calls: {:?}",
            guider_stub(world).lease_calls()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
