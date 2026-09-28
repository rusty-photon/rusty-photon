//! Step definitions for `status_switch.feature`
//!
//! Phase 3e wires every step body. The status switch exposes two read-only
//! switches: id 0 (raw voltage from `VS`) and id 1 (limit-hit boolean from
//! `FA.limit_detect`). Helpers seed mock device state, drive the Alpaca HTTP
//! surface, and assert on either the parsed value, the boolean projection,
//! or the captured ASCOM error code.

use crate::world::FalconRotatorWorld;
use cucumber::{given, then, when};

fn parse_bool(value: &str) -> bool {
    match value {
        "true" => true,
        "false" => false,
        other => panic!("expected 'true' or 'false', got '{other}'"),
    }
}

// The seed steps need both `#[given]` and `#[when]` annotations because
// they appear after a `When` in some scenarios — cucumber-rs maps `And`
// to the same keyword as the preceding step, so `And the device reports …`
// after a `When I connect …` is treated as a When and must match a When
// attribute. Same pattern as `the rotator reports mechanical position …`
// in `position_steps.rs`.
#[given(expr = "the device reports raw voltage {int}")]
#[when(expr = "the device reports raw voltage {int}")]
async fn given_voltage(world: &mut FalconRotatorWorld, raw: u32) {
    world.mock().set_voltage_raw(raw).await;
}

#[given(expr = "the device's limit_detect is {word}")]
#[when(expr = "the device's limit_detect is {word}")]
async fn given_limit_detect(world: &mut FalconRotatorWorld, value: String) {
    world.mock().set_limit_detect(parse_bool(&value)).await;
}

#[when("I read MaxSwitch")]
async fn read_max_switch(world: &mut FalconRotatorWorld) {
    let value = world.status_switch().max_switch().await.unwrap();
    world.max_switch_result = Some(value);
}

// `I read GetSwitchValue for id N` is referenced by a Phase 3d scenario in
// `connection_lifecycle.feature` (NOT_CONNECTED for disconnected switch),
// so its body landed earlier — the `ensure_connected!` guard on the switch
// getter handles that case without needing Phase 3e's value plumbing. The
// other read steps follow the same shape: Ok → populate the result slot
// and clear `last_error_code`; Err → record the code for the failure-path
// assertions.
#[when(expr = "I read GetSwitchValue for id {int}")]
async fn read_switch_value(world: &mut FalconRotatorWorld, id: usize) {
    match world.status_switch().get_switch_value(id).await {
        Ok(v) => {
            world.switch_value_result = Some(v);
            world.last_error_code = None;
        }
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[when(expr = "I read GetSwitch for id {int}")]
async fn read_switch_bool(world: &mut FalconRotatorWorld, id: usize) {
    match world.status_switch().get_switch(id).await {
        Ok(v) => {
            world.switch_bool_result = Some(v);
            world.last_error_code = None;
        }
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[when(expr = "I read GetSwitchName for id {int}")]
async fn read_switch_name(world: &mut FalconRotatorWorld, id: usize) {
    match world.status_switch().get_switch_name(id).await {
        Ok(v) => {
            world.switch_string_result = Some(v);
            world.last_error_code = None;
        }
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[when(expr = "I read GetSwitchDescription for id {int}")]
async fn read_switch_description(world: &mut FalconRotatorWorld, id: usize) {
    match world.status_switch().get_switch_description(id).await {
        Ok(v) => {
            world.switch_string_result = Some(v);
            world.last_error_code = None;
        }
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[when(expr = "I read MinSwitchValue for id {int}")]
async fn read_min_switch_value(world: &mut FalconRotatorWorld, id: usize) {
    match world.status_switch().min_switch_value(id).await {
        Ok(v) => {
            world.switch_value_result = Some(v);
            world.last_error_code = None;
        }
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[when(expr = "I read MaxSwitchValue for id {int}")]
async fn read_max_switch_value(world: &mut FalconRotatorWorld, id: usize) {
    match world.status_switch().max_switch_value(id).await {
        Ok(v) => {
            world.switch_value_result = Some(v);
            world.last_error_code = None;
        }
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[when(expr = "I read SwitchStep for id {int}")]
async fn read_switch_step(world: &mut FalconRotatorWorld, id: usize) {
    match world.status_switch().switch_step(id).await {
        Ok(v) => {
            world.switch_value_result = Some(v);
            world.last_error_code = None;
        }
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[when(expr = "I read CanWrite for id {int}")]
async fn read_can_write(world: &mut FalconRotatorWorld, id: usize) {
    match world.status_switch().can_write(id).await {
        Ok(v) => {
            world.switch_bool_result = Some(v);
            world.last_error_code = None;
        }
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[when(expr = "I call SetSwitch on id {int} with {word}")]
async fn call_set_switch(world: &mut FalconRotatorWorld, id: usize, value: String) {
    let state = parse_bool(&value);
    match world.status_switch().set_switch(id, state).await {
        Ok(()) => world.last_error_code = None,
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

/// Drive one `ISwitchV3` async member. `CanAsync` and `StateChangeComplete`
/// capture their boolean; the writers send fixed arguments, which never
/// matter because both switches refuse them.
#[when(expr = "I call {word} on switch id {int}")]
async fn call_async_member(world: &mut FalconRotatorWorld, member: String, id: usize) {
    let switch = world.status_switch();
    let result = match member.as_str() {
        "CanAsync" => switch.can_async(id).await.map(Some),
        "StateChangeComplete" => switch.state_change_complete(id).await.map(Some),
        "SetAsync" => switch.set_async(id, true).await.map(|()| None),
        "SetAsyncValue" => switch.set_async_value(id, 0.0).await.map(|()| None),
        "CancelAsync" => switch.cancel_async(id).await.map(|()| None),
        other => panic!("unknown async member '{other}'"),
    };
    match result {
        Ok(value) => {
            world.switch_bool_result = value;
            world.last_error_code = None;
        }
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[then("the async call should succeed")]
async fn async_call_should_succeed(world: &mut FalconRotatorWorld) {
    assert_eq!(
        world.last_error_code, None,
        "expected success, got ASCOM error code {:?}",
        world.last_error_code
    );
}

#[then(expr = "the switch name should be {string}")]
async fn switch_name_should_be(world: &mut FalconRotatorWorld, expected: String) {
    let actual = world
        .switch_string_result
        .as_deref()
        .expect("no switch name captured");
    assert_eq!(actual, expected);
}

#[then(expr = "the switch description should mention {string}")]
async fn switch_description_should_mention(world: &mut FalconRotatorWorld, substring: String) {
    let actual = world
        .switch_string_result
        .as_deref()
        .expect("no switch description captured");
    assert!(
        actual.to_lowercase().contains(&substring.to_lowercase()),
        "expected description to mention '{substring}', got: {actual}"
    );
}

#[then(expr = "MaxSwitch should be {int}")]
async fn max_switch_should_be(world: &mut FalconRotatorWorld, expected: usize) {
    let actual = world
        .max_switch_result
        .expect("no MaxSwitch value captured");
    assert_eq!(actual, expected);
}

#[then(expr = "the switch value should be {float}")]
async fn switch_value_should_be(world: &mut FalconRotatorWorld, expected: f64) {
    let actual = world.switch_value_result.expect("no switch value captured");
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

#[then(expr = "the switch boolean should be {word}")]
async fn switch_boolean_should_be(world: &mut FalconRotatorWorld, expected: String) {
    let expected = parse_bool(&expected);
    let actual = world
        .switch_bool_result
        .expect("no switch boolean captured");
    assert_eq!(actual, expected);
}

#[then(expr = "the set should fail with code {int}")]
async fn set_should_fail_with(world: &mut FalconRotatorWorld, code: u16) {
    let actual = world
        .last_error_code
        .expect("no error captured — SetSwitch succeeded unexpectedly");
    assert_eq!(actual, code);
}

#[then(expr = "CanWrite for id {int} should be {word}")]
async fn can_write_should_be(world: &mut FalconRotatorWorld, id: usize, value: String) {
    let expected = parse_bool(&value);
    // The Then step performs both the read and the comparison so the feature
    // file doesn't need a separate When line for every CanWrite assertion.
    let actual = world.status_switch().can_write(id).await.unwrap();
    assert_eq!(
        actual, expected,
        "CanWrite({id}) expected {expected}, got {actual}"
    );
}
