//! BDD step definitions for the Focuser Temperature Watch (rp.md §
//! Focuser Temperature Watch): rp polls every connected focuser's
//! probe and emits `temperature_changed` on a delta — an event, never
//! an action.
//!
//! The probe is the harness's restartable [`AlpacaDeviceStub`] in its
//! focuser shape: the scenario scripts its reading and waits on the
//! reads it has served, so no step sleeps through a poll interval
//! hoping the watch ran.

use std::time::Duration;

use cucumber::{given, then, when};

use bdd_infra::rp_harness::{AlpacaDeviceStub, FocuserConfig, FocuserProbe, StubDevice};

use crate::world::RpWorld;

/// Config id of the stub-backed focuser.
const STUB_FOCUSER_ID: &str = "stub-focuser";

/// How long a poll-until-observed step waits before failing the
/// scenario — a floor, not an estimate: every loop below exits as
/// soon as the condition holds.
const OBSERVATION_BUDGET: Duration = Duration::from_secs(15);

// --- Given steps ---

#[given(expr = "a stub Alpaca service hosting a focuser reporting {float} °C")]
fn stub_hosting_focuser_reporting(world: &mut RpWorld, temperature_c: f64) {
    let stub = AlpacaDeviceStub::start(StubDevice::Focuser);
    stub.set_focuser_probe(FocuserProbe::Reading(temperature_c));
    world.alpaca_stub = Some(stub);
}

#[given("a stub Alpaca service hosting a focuser without a temperature probe")]
fn stub_hosting_focuser_without_probe(world: &mut RpWorld) {
    let stub = AlpacaDeviceStub::start(StubDevice::Focuser);
    stub.set_focuser_probe(FocuserProbe::NotImplemented);
    world.alpaca_stub = Some(stub);
}

#[given("rp is configured with a focuser on the stub service")]
fn configured_with_stub_focuser(world: &mut RpWorld) {
    let url = stub(world).url();
    world.focusers.push(FocuserConfig {
        id: STUB_FOCUSER_ID.to_string(),
        alpaca_url: url,
        device_number: 0,
        min_position: None,
        max_position: None,
        backlash: None,
    });
}

#[given(
    expr = "a focuser temperature poll interval of {int} milliseconds and an event delta of {float} °C"
)]
const fn temperature_watch_knobs(world: &mut RpWorld, millis: u64, delta_c: f64) {
    world.temperature_watch = Some((Duration::from_millis(millis), delta_c));
}

// --- When steps ---

#[when(expr = "the stub focuser reports {float} °C")]
fn stub_focuser_reports(world: &mut RpWorld, temperature_c: f64) {
    script_probe(world, FocuserProbe::Reading(temperature_c));
}

#[when("the stub focuser's probe starts failing")]
fn stub_focuser_probe_fails(world: &mut RpWorld) {
    script_probe(world, FocuserProbe::Fault);
}

/// Barrier on the watch having polled: wait until the stub has served
/// `more` reads beyond the count recorded when the probe was last
/// scripted (or beyond zero after a restart reset the counter).
#[when(expr = "the watch has sampled the stub focuser at least {int} more times")]
#[then(expr = "the watch has sampled the stub focuser at least {int} more times")]
async fn watch_has_sampled(world: &mut RpWorld, more: u32) {
    let deadline = std::time::Instant::now() + OBSERVATION_BUDGET;
    loop {
        let reads = stub(world).focuser_probe_reads();
        // A restarted stub counts from zero again; a mark taken on the
        // old incarnation would then never be reached.
        let mark = if reads < world.stub_probe_reads_mark {
            0
        } else {
            world.stub_probe_reads_mark
        };
        if reads >= mark.saturating_add(more) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the temperature watch did not read the stub focuser {more} more times \
             within {OBSERVATION_BUDGET:?} (reads {reads}, mark {mark})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// --- Then steps ---

#[then(expr = "the first {string} event should name sensor {string} at {float} °C")]
async fn first_event_names_sensor_at(
    world: &mut RpWorld,
    event_type: String,
    sensor: String,
    value_c: f64,
) {
    assert!(
        world.wait_for_events(&event_type, 1).await,
        "expected to receive '{event_type}' event within timeout"
    );
    let events = world.received_events.read().await;
    let first = events
        .iter()
        .filter(|e| e.event_type == event_type)
        .min_by_key(|e| e.event_seq)
        .unwrap_or_else(|| panic!("no '{event_type}' event received"));
    assert_eq!(
        first
            .payload
            .get("sensor")
            .and_then(serde_json::Value::as_str),
        Some(sensor.as_str()),
        "unexpected sensor in the first '{event_type}' payload: {:?}",
        first.payload
    );
    let value = first
        .payload
        .get("value")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("no numeric value in the payload: {:?}", first.payload));
    assert!(
        (value - value_c).abs() < 1e-9,
        "expected value {value_c} in the first '{event_type}' payload, got {value}"
    );
}

/// Exactly `count` events of the type have arrived. Callers put a
/// "has sampled N more times" barrier ahead of this step; the short
/// grace here only covers a webhook POST still in flight from one of
/// those polls.
#[then(expr = "exactly {int} {string} event(s) should have been received")]
async fn exactly_n_events(world: &mut RpWorld, count: usize, event_type: String) {
    tokio::time::sleep(Duration::from_millis(250)).await;
    let events = world.received_events.read().await;
    let matching: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == event_type)
        .map(|e| &e.payload)
        .collect();
    assert_eq!(
        matching.len(),
        count,
        "expected exactly {count} '{event_type}' event(s), got {matching:?}"
    );
}

// --- helpers ---

fn script_probe(world: &mut RpWorld, probe: FocuserProbe) {
    let stub = stub(world);
    stub.set_focuser_probe(probe);
    world.stub_probe_reads_mark = stub.focuser_probe_reads();
}

const fn stub(world: &RpWorld) -> &AlpacaDeviceStub {
    world
        .alpaca_stub
        .as_ref()
        .expect("no stub Alpaca service — add a 'Given a stub Alpaca service ...' step")
}
