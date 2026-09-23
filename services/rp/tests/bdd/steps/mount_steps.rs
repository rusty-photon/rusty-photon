//! BDD step definitions for Mount MCP tools.
//!
//! Singular mount per rp deployment — there's no `mount_id` parameter
//! anywhere in this file (or in the `mount.feature` scenarios that
//! drive these steps).

use std::time::{Duration, Instant};

use cucumber::{given, then, when};
use serde_json::Value;

use bdd_infra::rp_harness::{MountConfig, OmniSimHandle};

use crate::steps::tool_steps::{ensure_mcp_client, start_rp};
use crate::world::RpWorld;

// --- Given steps ---

#[given("rp is running with a mount on the simulator")]
async fn rp_running_with_mount(world: &mut RpWorld) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
    let url = world.omnisim_url();
    world.mount = Some(MountConfig {
        alpaca_url: url,
        device_number: 0,
        settle_after_slew: None,
    });
    start_rp(world).await;
}

#[given("rp is running without a mount")]
async fn rp_running_without_mount(world: &mut RpWorld) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
    world.mount = None;
    start_rp(world).await;
}

#[given(expr = "rp is running with a mount at {string} device {int}")]
async fn rp_running_with_mount_at(world: &mut RpWorld, url: String, device_number: i32) {
    let device_number = u32::try_from(device_number)
        .expect("device_number in mount scenarios must be non-negative");
    world.mount = Some(MountConfig {
        alpaca_url: url,
        device_number,
        settle_after_slew: None,
    });
    start_rp(world).await;
}

#[given(expr = "rp is running with a mount on the simulator and {int}ms settle")]
async fn rp_running_with_mount_and_settle(world: &mut RpWorld, settle_ms: i64) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
    let url = world.omnisim_url();
    let settle_ms = u64::try_from(settle_ms).expect("settle_ms must be non-negative");
    world.mount = Some(MountConfig {
        alpaca_url: url,
        device_number: 0,
        settle_after_slew: Some(Duration::from_millis(settle_ms)),
    });
    start_rp(world).await;
}

/// Given step pinning the mount to `at_park == false`.
///
/// Used by slew-and-related scenarios as a self-documenting
/// precondition ("this scenario requires the mount to be unparked").
/// The per-scenario `before` hook in `bdd.rs` calls
/// `OmniSimHandle::reset_telescope`, which already leaves `OmniSim`'s
/// telescope at AtPark=false — so this step is functionally
/// redundant on the standard run path. It exists for readability and
/// as a safety net if the reset endpoint ever changes or is bypassed.
#[given("the mount is unparked")]
async fn mount_is_unparked(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    world
        .mcp()
        .call_tool("unpark", serde_json::json!({}))
        .await
        .expect("unpark should succeed in scenario setup");
}

#[given(expr = "the mount tracking is set to {word}")]
async fn mount_tracking_set_to(world: &mut RpWorld, value: String) {
    let enabled: bool = match value.as_str() {
        "true" => true,
        "false" => false,
        other => panic!("expected true|false for tracking, got {other}"),
    };
    ensure_mcp_client(world).await;
    // Given step: fail fast on setup problems per testing.md §3.3.
    // If `set_tracking` fails (e.g., misconfigured mount or
    // CanSetTracking == false in the simulator), the scenario should
    // surface that directly rather than failing later with a
    // less-direct symptom from the When step.
    world
        .mcp()
        .call_tool("set_tracking", serde_json::json!({ "enabled": enabled }))
        .await
        .expect("set_tracking should succeed in scenario setup");
}

// --- When steps ---

/// Slew step that interprets `MISSING` in either ra/dec column as
/// "omit that field from the JSON-RPC params" — drives the
/// missing-parameter Outline rows.
#[when(expr = "the MCP client calls \"slew\" with ra {string} dec {string}")]
async fn mcp_call_slew(world: &mut RpWorld, ra: String, dec: String) {
    ensure_mcp_client(world).await;
    let mut args = serde_json::Map::new();
    if ra != "MISSING" {
        let ra_val: f64 = ra
            .parse()
            .unwrap_or_else(|_| panic!("expected f64 or MISSING for ra, got {ra}"));
        args.insert("ra".to_string(), serde_json::json!(ra_val));
    }
    if dec != "MISSING" {
        let dec_val: f64 = dec
            .parse()
            .unwrap_or_else(|_| panic!("expected f64 or MISSING for dec, got {dec}"));
        args.insert("dec".to_string(), serde_json::json!(dec_val));
    }
    let result = world.mcp().call_tool("slew", Value::Object(args)).await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"sync_mount\" with ra {string} dec {string}")]
async fn mcp_call_sync_mount(world: &mut RpWorld, ra: String, dec: String) {
    ensure_mcp_client(world).await;
    let mut args = serde_json::Map::new();
    if ra != "MISSING" {
        let ra_val: f64 = ra
            .parse()
            .unwrap_or_else(|_| panic!("expected f64 or MISSING for ra, got {ra}"));
        args.insert("ra".to_string(), serde_json::json!(ra_val));
    }
    if dec != "MISSING" {
        let dec_val: f64 = dec
            .parse()
            .unwrap_or_else(|_| panic!("expected f64 or MISSING for dec, got {dec}"));
        args.insert("dec".to_string(), serde_json::json!(dec_val));
    }
    let result = world
        .mcp()
        .call_tool("sync_mount", Value::Object(args))
        .await;
    world.last_tool_result = Some(result);
}

/// Agreement two consecutive mount reads must reach before the mount
/// counts as settled, in degrees of RA.
///
/// `OmniSim` reports `RightAscension` frozen between its ~100 ms
/// `MoveAxes` ticks, and derives it from a freshly read sidereal time
/// against an axis built from a *cached* one. A sync therefore lands
/// offset by however stale that cache was, and the reported position
/// then settles over the next few ticks. Once settled, consecutive
/// reads agree to ~2e-6°; one settle step is three orders of magnitude
/// larger than that, so a bound between the two separates them without
/// being sensitive to either.
const MOUNT_SETTLED_AGREEMENT_DEG: f64 = 2e-5;

/// Gap between the two reads that have to agree. Longer than `OmniSim`'s
/// tick, so two samples cannot both read one frozen value and agree
/// without the mount having actually stopped moving.
const MOUNT_SETTLE_SAMPLE_GAP: Duration = Duration::from_millis(150);

/// Bound on the wait. A mount that never settles is a real fault, so
/// this fails the scenario rather than handing it a moving target.
const MOUNT_SETTLE_DEADLINE: Duration = Duration::from_secs(10);

/// Read the mount's RA through rp, in degrees — the same read and the
/// same ×15 conversion `plate_solve`'s `use_mount_hints` applies, so
/// what settles here is what a later assertion compares against.
async fn mount_ra_deg(world: &RpWorld) -> f64 {
    let result = world
        .mcp()
        .call_tool("get_mount_position", serde_json::json!({}))
        .await
        .expect("get_mount_position should succeed while settling the mount");
    let ra_hours = result.get("ra").and_then(Value::as_f64).unwrap_or_else(|| {
        panic!("expected ra field in get_mount_position result, got: {result:?}")
    });
    ra_hours * 15.0
}

/// Wait until the mount reports the position tracking will hold.
///
/// A sync leaves `OmniSim`'s reported RA settling for a few ticks (see
/// [`MOUNT_SETTLED_AGREEMENT_DEG`]). Two reads taken inside that window
/// fall on opposite sides of a tick and disagree by a whole correction
/// step — 3.7 arcsec measured on a loaded macOS runner, far past any
/// budget sized for the settled jitter. A scenario comparing one mount
/// read against another has to run this first, so both reads see the
/// held position and the comparison stops depending on runner speed.
///
/// Deliberately leaves `last_tool_result` alone: this is plumbing
/// between a `When` and its assertions, not a result under test.
#[when("the mount reading has settled")]
async fn mount_reading_has_settled(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let waiting_since = Instant::now();
    let mut previous = mount_ra_deg(world).await;
    loop {
        tokio::time::sleep(MOUNT_SETTLE_SAMPLE_GAP).await;
        let current = mount_ra_deg(world).await;
        if (current - previous).abs() < MOUNT_SETTLED_AGREEMENT_DEG {
            return;
        }
        assert!(
            waiting_since.elapsed() < MOUNT_SETTLE_DEADLINE,
            "mount RA never settled to within {MOUNT_SETTLED_AGREEMENT_DEG}° across              two reads {MOUNT_SETTLE_SAMPLE_GAP:?} apart; last pair {previous}° and {current}°"
        );
        previous = current;
    }
}

#[when("the MCP client calls \"get_mount_position\"")]
async fn mcp_call_get_mount_position(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("get_mount_position", serde_json::json!({}))
        .await;
    world.last_tool_result = Some(result);
}

#[when("the MCP client calls \"get_tracking\"")]
async fn mcp_call_get_tracking(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("get_tracking", serde_json::json!({}))
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"set_tracking\" with enabled {word}")]
async fn mcp_call_set_tracking(world: &mut RpWorld, enabled: String) {
    ensure_mcp_client(world).await;
    let enabled: bool = match enabled.as_str() {
        "true" => true,
        "false" => false,
        other => panic!("expected true|false for enabled, got {other}"),
    };
    let result = world
        .mcp()
        .call_tool("set_tracking", serde_json::json!({ "enabled": enabled }))
        .await;
    world.last_tool_result = Some(result);
}

#[when("the MCP client calls \"park\"")]
async fn mcp_call_park(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world.mcp().call_tool("park", serde_json::json!({})).await;
    world.last_tool_result = Some(result);
}

#[when("the MCP client calls \"unpark\"")]
async fn mcp_call_unpark(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world.mcp().call_tool("unpark", serde_json::json!({})).await;
    world.last_tool_result = Some(result);
}

#[when("the MCP client calls \"get_park_state\"")]
async fn mcp_call_get_park_state(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("get_park_state", serde_json::json!({}))
        .await;
    world.last_tool_result = Some(result);
}

#[when("the MCP client calls \"abort_slew\"")]
async fn mcp_call_abort_slew(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("abort_slew", serde_json::json!({}))
        .await;
    world.last_tool_result = Some(result);
}

// --- Then steps ---

/// `OmniSim`'s slew echo does not land exactly on the requested
/// coordinates, so we assert tolerance, not equality. It is a stale
/// clock, not float drift in a coordinate transform: the simulator
/// builds the mount axis from a sidereal time it refreshes only on its
/// 100 ms `MoveAxes` tick, and re-derives the echoed coordinates from a
/// freshly read one — see docs/skills/testing.md §5.14, which also
/// explains why the same mechanism can grow unbounded on a stalled
/// runner (issue #1252).
///
/// One constant, two units, because each assertion below takes the
/// tool's own: `actual_ra` is decimal hours, so `0.001` there is 3.6
/// seconds of RA — 54 arcsec of angle, not 3.6; `actual_dec` is
/// degrees, so `0.001` there really is 3.6 arcsec. Both sit well under
/// any centering workflow's tolerance and well above one quiet tick
/// (0.1 s of RA, 1.5 arcsec).
const SLEW_ECHO_TOLERANCE: f64 = 0.001;

#[then(expr = "the slew result actual_ra should be {float}")]
fn slew_actual_ra(world: &mut RpWorld, expected: f64) {
    let result = unwrap_ok(world);
    let actual = result
        .get("actual_ra")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("expected actual_ra field, got: {result:?}"));
    assert!(
        (actual - expected).abs() < SLEW_ECHO_TOLERANCE,
        "expected actual_ra ≈ {expected} (within {SLEW_ECHO_TOLERANCE}), got {actual}"
    );
}

#[then(expr = "the slew result actual_dec should be {float}")]
fn slew_actual_dec(world: &mut RpWorld, expected: f64) {
    let result = unwrap_ok(world);
    let actual = result
        .get("actual_dec")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("expected actual_dec field, got: {result:?}"));
    assert!(
        (actual - expected).abs() < SLEW_ECHO_TOLERANCE,
        "expected actual_dec ≈ {expected} (within {SLEW_ECHO_TOLERANCE}), got {actual}"
    );
}

#[then(expr = "the get_mount_position result ra should be {float}")]
fn get_mount_position_ra(world: &mut RpWorld, expected: f64) {
    let result = unwrap_ok(world);
    let actual = result
        .get("ra")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("expected ra field, got: {result:?}"));
    assert_eq!(actual, expected, "expected ra {expected}, got {actual}");
}

#[then(expr = "the get_mount_position result dec should be {float}")]
fn get_mount_position_dec(world: &mut RpWorld, expected: f64) {
    let result = unwrap_ok(world);
    let actual = result
        .get("dec")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("expected dec field, got: {result:?}"));
    assert_eq!(actual, expected, "expected dec {expected}, got {actual}");
}

#[then(expr = "the get_tracking result tracking should be {word}")]
fn get_tracking_tracking(world: &mut RpWorld, expected: String) {
    let result = unwrap_ok(world);
    let expected: bool = match expected.as_str() {
        "true" => true,
        "false" => false,
        other => panic!("expected true|false, got {other}"),
    };
    let actual = result
        .get("tracking")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| panic!("expected tracking bool field, got: {result:?}"));
    assert_eq!(
        actual, expected,
        "expected tracking={expected}, got {actual}"
    );
}

#[then(expr = "the get_tracking result can_set_tracking should be {word}")]
fn get_tracking_can_set_tracking(world: &mut RpWorld, expected: String) {
    let result = unwrap_ok(world);
    let expected: bool = match expected.as_str() {
        "true" => true,
        "false" => false,
        other => panic!("expected true|false, got {other}"),
    };
    let actual = result
        .get("can_set_tracking")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| panic!("expected can_set_tracking bool field, got: {result:?}"));
    assert_eq!(
        actual, expected,
        "expected can_set_tracking={expected}, got {actual}"
    );
}

#[then(expr = "the get_park_state result {word} should be {word}")]
fn get_park_state_field(world: &mut RpWorld, field: String, expected: String) {
    let result = unwrap_ok(world);
    let expected: bool = match expected.as_str() {
        "true" => true,
        "false" => false,
        other => panic!("expected true|false, got {other}"),
    };
    let actual = result
        .get(&field)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| panic!("expected {field} bool field, got: {result:?}"));
    assert_eq!(
        actual, expected,
        "expected {field}={expected}, got {actual}"
    );
}

// --- Helpers ---

fn unwrap_ok(world: &RpWorld) -> &Value {
    world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("tool call failed")
}
