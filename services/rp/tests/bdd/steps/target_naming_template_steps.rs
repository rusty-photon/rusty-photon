//! BDD step definitions for the round-trippable file-naming template's
//! config-load validation (`target_naming_template.feature`, rp.md §
//! Persistence + rp-targets.md § File-naming template).
//!
//! Scoped to the config-load contract only ("the pattern is parsed and
//! checked at startup; a bad pattern fails the load, not a session" —
//! rp-targets.md), covering both `file_naming_pattern` and
//! `directory_pattern`. `capture`'s actual render/parse use of these
//! patterns is covered by `capture_target_linkage.feature` instead.

use cucumber::{given, then, when};

use bdd_infra::ServiceHandle;

use crate::world::RpWorld;

fn write_naming_pattern_config(world: &mut RpWorld, pattern: &str) {
    let dir = tempfile::tempdir().expect("create temp dir for rp config");
    let config = serde_json::json!({
        "session": {
            "data_directory": dir.path().join("data").to_string_lossy(),
            "file_naming_pattern": pattern
        },
        "equipment": {},
        "server": { "port": 0, "bind_address": "127.0.0.1" }
    });
    let path = dir.path().join("rp.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&config).expect("serialize rp config"),
    )
    .expect("write rp config file");
    world.config_rest_path = Some(path);
    world.config_rest_dir = Some(dir);
}

// ---------------------------------------------------------------------------
// Given
// ---------------------------------------------------------------------------

#[given(expr = "an rp config with file_naming_pattern {string}")]
fn given_naming_pattern_config(world: &mut RpWorld, pattern: String) {
    write_naming_pattern_config(world, &pattern);
}

#[given(expr = "an rp config with directory_pattern {string}")]
fn given_directory_pattern_config(world: &mut RpWorld, pattern: String) {
    let dir = tempfile::tempdir().expect("create temp dir for rp config");
    let config = serde_json::json!({
        "session": {
            "data_directory": dir.path().join("data").to_string_lossy(),
            "directory_pattern": pattern
        },
        "equipment": {},
        "server": { "port": 0, "bind_address": "127.0.0.1" }
    });
    let path = dir.path().join("rp.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&config).expect("serialize rp config"),
    )
    .expect("write rp config file");
    world.config_rest_path = Some(path);
    world.config_rest_dir = Some(dir);
}

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

#[when("rp attempts to start")]
async fn rp_attempts_to_start(world: &mut RpWorld) {
    let path = world
        .config_rest_path
        .clone()
        .expect("no config written — add a 'Given an rp config with file_naming_pattern' step");
    match ServiceHandle::try_start(env!("CARGO_PKG_NAME"), path.to_str().expect("utf-8 path")).await
    {
        Ok(handle) => {
            world.rp = Some(handle);
            world.rp_start_error = None;
        }
        Err(e) => {
            world.rp_start_error = Some(e);
        }
    }
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

#[then("rp should fail to start")]
fn rp_should_fail_to_start(world: &mut RpWorld) {
    assert!(
        world.rp_start_error.is_some(),
        "expected rp to fail to start on this pattern, but it started successfully"
    );
}

#[then("rp should start successfully")]
async fn rp_should_start_successfully(world: &mut RpWorld) {
    assert!(
        world.rp_start_error.is_none(),
        "expected rp to start, but it failed: {:?}",
        world.rp_start_error
    );
    assert!(
        world.wait_for_rp_healthy().await,
        "rp did not become healthy within timeout"
    );
}
