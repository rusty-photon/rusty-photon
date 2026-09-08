//! Step definitions for `sensor_readings.feature`
//!
//! The feature file reuses "Given a running UPBv2 server with the switch
//! connected", "When I wait for the switch data to be available" and
//! "Then switch N value should be ..." from `switch_control_steps`. This file
//! adds the tolerant comparison the scaled per-channel currents need, and the
//! one preset no driver command can produce: a tripped rail.

use crate::steps::infrastructure::default_test_config;
use crate::world::Upbv2World;
use cucumber::{given, then};

/// Environment variable the mock reads its overcurrent flags from.
///
/// Nothing in the driver's command set can ask a healthy box to trip a rail,
/// so a scenario that needs one preset has to hand it to the simulated device
/// before it starts.
const ENV_OVERCURRENT: &str = "UPBV2_MOCK_OVERCURRENT";

/// Width of the `PA` overcurrent field: the four 12 V outputs then dew A-C.
const OVERCURRENT_FLAG_COUNT: usize = 7;

/// How many 12 V outputs the box has. The first `OUTPUT_COUNT` characters of
/// the overcurrent field are theirs; the remaining three are dew A-C.
const OUTPUT_COUNT: usize = 4;

/// Tolerance for the scaled current readings.
///
/// The raw sense counts divide by 480 (or 700 for dew C), so an exact
/// comparison would assert the divisor's rounding rather than the scaling.
/// Tight enough that swapping the two divisors — 350/480 = 0.73 against
/// 350/700 = 0.5 — still fails.
const CURRENT_TOLERANCE: f64 = 0.01;

/// Build the seven-character `PA` overcurrent field with one 12 V output
/// tripped. Derived from the output number rather than spelled out so the
/// feature file names the channel and this stays the single place that knows
/// the field's layout.
///
/// # Panics
///
/// Panics if `output` names no 12 V output. Without the check an out-of-range
/// number produces an all-zeros field — a perfectly *healthy* device — and the
/// scenario then fails on whichever overcurrent switch it asserts, which says
/// nothing about the real mistake in the feature file.
fn overcurrent_flags_for_output(output: usize) -> String {
    assert!(
        (1..=OUTPUT_COUNT).contains(&output),
        "no 12V output {output}: the UPBv2 has {OUTPUT_COUNT}, numbered 1-{OUTPUT_COUNT}"
    );
    (0..OVERCURRENT_FLAG_COUNT)
        .map(|i| if i + 1 == output { '1' } else { '0' })
        .collect()
}

// ============================================================================
// Given steps
// ============================================================================

#[given(expr = "a running UPBv2 server reporting overcurrent on 12V output {int}")]
async fn running_server_reporting_overcurrent(world: &mut Upbv2World, output: usize) {
    world.config = default_test_config();
    world
        .start_upbv2_with_env(&[(ENV_OVERCURRENT, &overcurrent_flags_for_output(output))])
        .await;
    world.switch_ref().set_connected(true).await.unwrap();
}

// ============================================================================
// Then steps
// ============================================================================

#[then(expr = "switch {int} value should be approximately {float}")]
async fn switch_value_approximately(world: &mut Upbv2World, id: usize, expected: f64) {
    let value = world.switch_ref().get_switch_value(id).await.unwrap();
    assert!(
        (value - expected).abs() < CURRENT_TOLERANCE,
        "switch {id} value should be ~{expected} (within {CURRENT_TOLERANCE}), got {value}"
    );
}
