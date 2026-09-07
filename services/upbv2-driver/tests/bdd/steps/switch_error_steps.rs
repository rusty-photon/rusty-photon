//! Step definitions for `switch_errors.feature`

use crate::steps::switch_control_steps::switch_state;
use crate::world::Upbv2World;
use cucumber::{then, when};

// ============================================================================
// When steps
// ============================================================================

#[when(expr = "I try to get switch {int} value")]
async fn try_get_switch_value(world: &mut Upbv2World, id: usize) {
    let result = world.switch_ref().get_switch_value(id).await;
    world.capture_result(result);
}

#[when(expr = "I try to get switch {int} boolean")]
async fn try_get_switch_boolean(world: &mut Upbv2World, id: usize) {
    let result = world.switch_ref().get_switch(id).await;
    world.capture_result(result);
}

#[when(expr = "I try to set switch {int} boolean to {word}")]
async fn try_set_switch_boolean(world: &mut Upbv2World, id: usize, state: String) {
    let result = world
        .switch_ref()
        .set_switch(id, switch_state(&state))
        .await;
    world.capture_result(result);
}

#[when(expr = "I try to query can_write for switch {int}")]
async fn try_query_can_write(world: &mut Upbv2World, id: usize) {
    let result = world.switch_ref().can_write(id).await;
    world.capture_result(result);
}

#[when(expr = "I try to query can_async for switch {int}")]
async fn try_query_can_async(world: &mut Upbv2World, id: usize) {
    let result = world.switch_ref().can_async(id).await;
    world.capture_result(result);
}

#[when(expr = "I try to query state_change_complete for switch {int}")]
async fn try_query_state_change_complete(world: &mut Upbv2World, id: usize) {
    let result = world.switch_ref().state_change_complete(id).await;
    world.capture_result(result);
}

#[when(expr = "I try to call cancel_async on switch {int}")]
async fn try_cancel_async(world: &mut Upbv2World, id: usize) {
    let result = world.switch_ref().cancel_async(id).await;
    world.capture_result(result);
}

#[when(expr = "I try to call set_async on switch {int}")]
async fn try_set_async(world: &mut Upbv2World, id: usize) {
    let result = world.switch_ref().set_async(id, true).await;
    world.capture_result(result);
}

#[when(expr = "I try to call set_async_value on switch {int}")]
async fn try_set_async_value(world: &mut Upbv2World, id: usize) {
    let result = world.switch_ref().set_async_value(id, 0.0).await;
    world.capture_result(result);
}

// ============================================================================
// Then steps
// ============================================================================

#[then(expr = "all operations on switch {int} should fail")]
async fn all_operations_on_switch_should_fail(world: &mut Upbv2World, id: usize) {
    assert_every_operation_fails(world, id).await;
}

#[then(expr = "switch {int} name query should fail")]
async fn switch_name_query_should_fail(world: &mut Upbv2World, id: usize) {
    world.switch_ref().get_switch_name(id).await.unwrap_err();
}

#[then(expr = "switch {int} description query should fail")]
async fn switch_description_query_should_fail(world: &mut Upbv2World, id: usize) {
    world
        .switch_ref()
        .get_switch_description(id)
        .await
        .unwrap_err();
}

#[then(expr = "switch {int} min value query should fail")]
async fn switch_min_value_query_should_fail(world: &mut Upbv2World, id: usize) {
    world.switch_ref().min_switch_value(id).await.unwrap_err();
}

#[then(expr = "switch {int} max value query should fail")]
async fn switch_max_value_query_should_fail(world: &mut Upbv2World, id: usize) {
    world.switch_ref().max_switch_value(id).await.unwrap_err();
}

#[then(expr = "switch {int} step query should fail")]
async fn switch_step_query_should_fail(world: &mut Upbv2World, id: usize) {
    world.switch_ref().switch_step(id).await.unwrap_err();
}

#[then(expr = "operations on invalid switch IDs {int}, {int}, {int}, {int} should all fail")]
async fn operations_on_invalid_ids_should_fail(
    world: &mut Upbv2World,
    id1: usize,
    id2: usize,
    id3: usize,
    id4: usize,
) {
    for id in [id1, id2, id3, id4] {
        assert_every_operation_fails(world, id).await;
    }
}

#[then(expr = "can_async should return false for all {int} switches")]
async fn can_async_returns_false_for_all(world: &mut Upbv2World, count: usize) {
    let switch = world.switch_ref();
    for id in 0..count {
        assert!(
            !switch.can_async(id).await.unwrap(),
            "switch {id} should not support async ops"
        );
    }
}

#[then(expr = "state_change_complete should return true for all {int} switches")]
async fn state_change_complete_returns_true_for_all(world: &mut Upbv2World, count: usize) {
    let switch = world.switch_ref();
    for id in 0..count {
        assert!(
            switch.state_change_complete(id).await.unwrap(),
            "switch {id} state change should be complete"
        );
    }
}

#[then(expr = "cancel_async should succeed for all {int} switches")]
async fn cancel_async_succeeds_for_all(world: &mut Upbv2World, count: usize) {
    let switch = world.switch_ref();
    for id in 0..count {
        switch.cancel_async(id).await.unwrap();
    }
}

/// Every read, write and metadata query the Switch interface offers, asserted
/// to fail for one id. Shared by the single-id and the four-id steps so the
/// two can never drift into checking different surfaces.
async fn assert_every_operation_fails(world: &Upbv2World, id: usize) {
    let switch = world.switch_ref();

    switch.can_write(id).await.unwrap_err();
    switch.get_switch(id).await.unwrap_err();
    switch.get_switch_value(id).await.unwrap_err();
    switch.set_switch(id, true).await.unwrap_err();
    switch.set_switch_value(id, 0.0).await.unwrap_err();
    switch.get_switch_name(id).await.unwrap_err();
    switch.get_switch_description(id).await.unwrap_err();
    switch.min_switch_value(id).await.unwrap_err();
    switch.max_switch_value(id).await.unwrap_err();
    switch.switch_step(id).await.unwrap_err();
}
