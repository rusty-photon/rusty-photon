//! Step definitions for `sensor_readings.feature`
//!
//! The feature file uses "Given a running PPBA server with the switch connected"
//! (defined in `switch_control_steps`) and "When I wait for the switch data to be available"
//! (also in `switch_control_steps`). This file only defines the Then assertions
//! specific to sensor value checks.

use crate::world::PpbaWorld;
use cucumber::then;

// ============================================================================
// Then steps
// ============================================================================

#[then(expr = "switch {int} value should be approximately {float}")]
async fn switch_value_approximately(world: &mut PpbaWorld, id: usize, expected: f64) {
    let value = world.switch_ref().get_switch_value(id).await.unwrap();
    assert!(
        (value - expected).abs() < 0.1,
        "switch {id} value should be ~{expected}, got {value}"
    );
}

#[then(expr = "switch {int} value should be in range {float} to {float}")]
async fn switch_value_in_range(world: &mut PpbaWorld, id: usize, min: f64, max: f64) {
    let value = world.switch_ref().get_switch_value(id).await.unwrap();
    assert!(
        (min..=max).contains(&value),
        "switch {id} value {value} should be in range {min}..={max}"
    );
}

#[then("every switch value should lie between its published minimum and maximum")]
async fn every_switch_value_within_published_range(world: &mut PpbaWorld) {
    let switch = world.switch_ref();
    let count = switch.max_switch().await.unwrap();
    let mut out_of_range = Vec::new();
    for id in 0..count {
        let min = switch.min_switch_value(id).await.unwrap();
        let max = switch.max_switch_value(id).await.unwrap();
        let value = switch.get_switch_value(id).await.unwrap();
        if !(min..=max).contains(&value) {
            out_of_range.push(format!("switch {id}: {value} outside {min}..={max}"));
        }
    }
    assert!(
        out_of_range.is_empty(),
        "switch values outside their published range: {out_of_range:?}"
    );
}

#[then(expr = "switch {int} value should be non-negative")]
async fn switch_value_non_negative(world: &mut PpbaWorld, id: usize) {
    let value = world.switch_ref().get_switch_value(id).await.unwrap();
    assert!(
        value >= 0.0,
        "switch {id} value should be >= 0, got {value}"
    );
}

#[then(expr = "switch {int} value should be 0.0 or 1.0")]
async fn switch_value_boolean_range(world: &mut PpbaWorld, id: usize) {
    let value = world.switch_ref().get_switch_value(id).await.unwrap();
    assert!(
        value == 0.0 || value == 1.0,
        "switch {id} should be 0.0 or 1.0, got {value}"
    );
}

#[then(expr = "switch {int} value should be positive")]
async fn switch_value_positive(world: &mut PpbaWorld, id: usize) {
    let value = world.switch_ref().get_switch_value(id).await.unwrap();
    assert!(
        value > 0.0,
        "switch {id} value should be positive, got {value}"
    );
}
