//! Step definitions for `sync_offset.feature`

use crate::world::FalconRotatorWorld;
use cucumber::{given, then, when};

#[given("a pa-falcon-rotator process whose sync-offset record cannot be written")]
async fn process_with_unwritable_record(world: &mut FalconRotatorWorld) {
    world.start_process_with_unwritable_sync_record().await;
}

#[when("the process stops and starts again on the same config")]
async fn restart_on_same_config(world: &mut FalconRotatorWorld) {
    world.restart_process().await;
}

#[then(expr = "the record {word} beside the config holds sync_offset_deg {float}")]
async fn record_holds_offset(world: &mut FalconRotatorWorld, name: String, expected: f64) {
    let path = world.process_file(&name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let record: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{} does not parse: {e}\n{text}", path.display()));
    let actual = record["sync_offset_deg"]
        .as_f64()
        .unwrap_or_else(|| panic!("{} has no numeric sync_offset_deg:\n{text}", path.display()));
    assert!(
        (actual - expected).abs() < 1e-6,
        "expected sync_offset_deg {expected}, got {actual}"
    );
}

#[when(expr = "I call Sync with {float}")]
async fn call_sync(world: &mut FalconRotatorWorld, position: f64) {
    match world.rotator().sync(position).await {
        Ok(()) => world.last_error_code = None,
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

// Cucumber's `{float}` regex matches "NaN", so the parameterised step
// already covers that case (and a dedicated step would be ambiguous).
// "Infinity" / "-Infinity" need explicit steps because the regex only
// recognises `inf`.

#[when("I call Sync with Infinity")]
async fn call_sync_infinity(world: &mut FalconRotatorWorld) {
    capture_sync(world, f64::INFINITY).await;
}

#[when("I call Sync with -Infinity")]
async fn call_sync_neg_infinity(world: &mut FalconRotatorWorld) {
    capture_sync(world, f64::NEG_INFINITY).await;
}

async fn capture_sync(world: &mut FalconRotatorWorld, value: f64) {
    match world.rotator().sync(value).await {
        Ok(()) => world.last_error_code = None,
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[then(expr = "Sync should fail with code {int}")]
async fn sync_should_fail_with(world: &mut FalconRotatorWorld, code: u16) {
    let actual = world
        .last_error_code
        .expect("no error captured — Sync succeeded unexpectedly");
    assert_eq!(
        actual, code,
        "expected ASCOM error code {code}, got {actual}"
    );
}

#[then("no SD command should have been sent")]
async fn no_sd_command(world: &mut FalconRotatorWorld) {
    // The Falcon's SD: command rewrites the device's stored position
    // counter, which would change MechanicalPosition. The driver's
    // Sync uses a driver-side offset instead — see the design doc's
    // [Sync semantics] section. SD must never appear on the wire.
    let log = world.mock().command_log().await;
    assert!(
        !log.iter().any(|c| c.starts_with("SD")),
        "unexpected SD in wire log: {log:?}"
    );
}

#[then("MechanicalPosition should be unchanged")]
async fn mechanical_position_unchanged(world: &mut FalconRotatorWorld) {
    // ASCOM Sync must leave MechanicalPosition unchanged. Compare the
    // post-Sync MechanicalPosition over Alpaca to the mock's current
    // mech_position_deg — they should match exactly because no MD or
    // SD wire command was issued.
    let mock_value = world.mock().mech_position_deg().await;
    let alpaca_value = world.rotator().mechanical_position().await.unwrap();
    assert!(
        (alpaca_value - mock_value).abs() < 1e-6,
        "MechanicalPosition after Sync ({alpaca_value}°) differs from mock state ({mock_value}°)"
    );
}
