//! Step definitions for `health.feature`.

use crate::world::{HttpResponse, PlateSolverWorld};
use cucumber::{given, when};
use std::time::Instant;

#[given("the wrapper is running with a temp-dir copy of mock_astap as its binary path")]
async fn given_wrapper_running_with_temp_copy(world: &mut PlateSolverWorld) {
    world.start_wrapper_with_mock_copy().await;
}

#[given("the wrapper is running with a temp astap_db_directory")]
async fn given_wrapper_running_with_temp_db(world: &mut PlateSolverWorld) {
    // Same as the above — the temp-dir copy already uses a temp db.
    world.start_wrapper_with_mock_copy().await;
}

#[when("I delete the configured astap_binary_path")]
async fn when_delete_configured_binary(world: &mut PlateSolverWorld) {
    let p = world
        .astap_binary_path
        .as_ref()
        .expect("astap_binary_path not set");
    std::fs::remove_file(p).expect("remove configured binary");
}

#[when("I delete the configured astap_db_directory")]
async fn when_delete_configured_db(world: &mut PlateSolverWorld) {
    let p = world
        .astap_db_directory
        .as_ref()
        .expect("astap_db_directory not set");
    std::fs::remove_dir_all(p).expect("remove configured db dir");
}

#[when("I GET /health")]
async fn when_get_health(world: &mut PlateSolverWorld) {
    // Lazy spawn for scenarios whose only "Given" was the marker
    // "the wrapper is running with mock_astap as its solver"
    // (defined in solve_steps.rs as a no-op). Health scenarios that
    // use `start_wrapper_with_mock_copy` already have a handle.
    if world.service_handle.is_none() {
        world.start_wrapper_with_mock().await;
    }
    let url = format!("{}/health", world.wrapper_url());
    let started = Instant::now();
    let resp = PlateSolverWorld::http_client()
        .get(&url)
        .send()
        .await
        .expect("GET /health");
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.expect("read body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    world.last_response_elapsed = Some(started.elapsed());
    world.last_response = Some(HttpResponse { status, body });
}
