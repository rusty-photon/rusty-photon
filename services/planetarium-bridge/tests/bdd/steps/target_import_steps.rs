//! Steps for `target_import.feature`: what the bridge submits to rp, and what
//! it never submits. rp-side semantics (naming, dedup, slugs) are covered in
//! rp's own suite.

use std::time::{Duration, Instant};

use cucumber::then;

use crate::world::{BridgeWorld, POLL_INTERVAL, WAIT_BUDGET};

/// JSON round-trips of exact wire floats.
const COORD_TOLERANCE: f64 = 1e-6;

#[then(expr = "the stub rp should receive {int} add_target call(s)")]
async fn stub_receives(world: &mut BridgeWorld, expected: usize) {
    if expected == 0 {
        // Absence check: give a would-be import ample time to arrive first.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let calls = world.stub().add_target_calls();
        assert!(
            calls.is_empty(),
            "expected no add_target calls, got {calls:?}"
        );
        return;
    }
    // Replay scenarios legitimately take seconds (reconnect backoff).
    let start = Instant::now();
    while start.elapsed() < WAIT_BUDGET {
        if world.stub().add_target_calls().len() >= expected {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    // Short grace so an over-delivery right behind the expected calls fails.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let calls = world.stub().add_target_calls();
    assert_eq!(
        calls.len(),
        expected,
        "add_target calls after {:?}: {calls:?}",
        start.elapsed()
    );
}

fn last_import(world: &BridgeWorld) -> serde_json::Value {
    world
        .stub()
        .add_target_calls()
        .last()
        .expect("no add_target calls recorded")
        .arguments
        .clone()
}

#[then(expr = "the import should carry ra_hours {float} and dec_degrees {float}")]
async fn import_carries(world: &mut BridgeWorld, ra_hours: f64, dec_degrees: f64) {
    let args = last_import(world);
    let actual_ra = args["ra_hours"].as_f64().expect("ra_hours missing");
    let actual_dec = args["dec_degrees"].as_f64().expect("dec_degrees missing");
    assert!(
        (actual_ra - ra_hours).abs() < COORD_TOLERANCE,
        "ra_hours {actual_ra}, expected {ra_hours}"
    );
    assert!(
        (actual_dec - dec_degrees).abs() < COORD_TOLERANCE,
        "dec_degrees {actual_dec}, expected {dec_degrees}"
    );
}

#[then(
    expr = "the import source kind should be {string} with a client address and an RFC 3339 receipt time"
)]
async fn import_source(world: &mut BridgeWorld, kind: String) {
    let args = last_import(world);
    let source = &args["source"];
    assert_eq!(
        source["kind"].as_str(),
        Some(kind.as_str()),
        "source: {source:?}"
    );
    let client = source["client"].as_str().expect("source.client missing");
    assert!(!client.is_empty(), "source.client is empty");
    let received_at = source["received_at"]
        .as_str()
        .expect("source.received_at missing");
    chrono::DateTime::parse_from_rfc3339(received_at)
        .unwrap_or_else(|e| panic!("source.received_at {received_at:?} is not RFC 3339: {e}"));
}

#[then("the import should carry no catalog_ref and no display_name")]
async fn import_carries_no_naming(world: &mut BridgeWorld) {
    let args = last_import(world);
    let object = args
        .as_object()
        .expect("add_target arguments are not an object");
    assert!(
        !object.contains_key("catalog_ref"),
        "import must not name itself: {object:?}"
    );
    assert!(
        !object.contains_key("display_name"),
        "import must not name itself: {object:?}"
    );
}

/// Small-angle separation (arcminutes) between the last import and a
/// reference coordinate.
fn separation_arcmin(args: &serde_json::Value, ra_hours: f64, dec_degrees: f64) -> f64 {
    let actual_ra = args["ra_hours"].as_f64().expect("ra_hours missing");
    let actual_dec = args["dec_degrees"].as_f64().expect("dec_degrees missing");
    let east = (actual_ra - ra_hours) * 15.0 * dec_degrees.to_radians().cos() * 60.0;
    let north = (actual_dec - dec_degrees) * 60.0;
    east.hypot(north)
}

#[then(
    expr = "the imported coordinates should be within {float} arcminutes of ra {float} hours dec {float} degrees"
)]
async fn import_within(world: &mut BridgeWorld, arcmin: f64, ra_hours: f64, dec_degrees: f64) {
    let separation = separation_arcmin(&last_import(world), ra_hours, dec_degrees);
    assert!(
        separation <= arcmin,
        "import is {separation:.2} arcmin from ra {ra_hours}h dec {dec_degrees} deg (allowed {arcmin})"
    );
}

#[then(
    expr = "the imported coordinates should be at least {float} arcminutes away from ra {float} hours dec {float} degrees"
)]
async fn import_away(world: &mut BridgeWorld, arcmin: f64, ra_hours: f64, dec_degrees: f64) {
    let separation = separation_arcmin(&last_import(world), ra_hours, dec_degrees);
    assert!(
        separation >= arcmin,
        "import is only {separation:.2} arcmin from ra {ra_hours}h dec {dec_degrees} deg (expected at least {arcmin} — was the epoch conversion skipped?)"
    );
}
