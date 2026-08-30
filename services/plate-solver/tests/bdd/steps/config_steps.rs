//! Step definitions for `configuration.feature`.

use crate::world::PlateSolverWorld;
use cucumber::{given, then, when};
use std::path::PathBuf;

#[given("a config without astap_binary_path")]
async fn given_config_without_binary_path(world: &mut PlateSolverWorld) {
    let dir = world.temp_dir_path();
    let db = make_db_dir(&dir);
    world
        .pending_config
        .insert("astap_db_directory".into(), db.to_string_lossy().into());
}

#[given("a config without astap_db_directory")]
async fn given_config_without_db_directory(world: &mut PlateSolverWorld) {
    let dir = world.temp_dir_path();
    let bin = copy_mock_astap(&dir);
    world
        .pending_config
        .insert("astap_binary_path".into(), bin.to_string_lossy().into());
}

#[given(expr = "a config with astap_binary_path {string}")]
async fn given_config_with_binary_path(world: &mut PlateSolverWorld, path: String) {
    world
        .pending_config
        .insert("astap_binary_path".into(), path.into());
}

#[given(expr = "a config with astap_db_directory {string}")]
async fn given_config_with_db_directory(world: &mut PlateSolverWorld, path: String) {
    world
        .pending_config
        .insert("astap_db_directory".into(), path.into());
}

#[given("a config with astap_binary_path pointing at a non-executable file")]
async fn given_non_executable_binary_path(world: &mut PlateSolverWorld) {
    let dir = world.temp_dir_path();
    let bin = dir.join("not_executable");
    std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).expect("chmod 644");
    }
    world
        .pending_config
        .insert("astap_binary_path".into(), bin.to_string_lossy().into());
}

#[given("a valid astap_binary_path")]
async fn given_valid_binary_path(world: &mut PlateSolverWorld) {
    let dir = world.temp_dir_path();
    let bin = copy_mock_astap(&dir);
    world
        .pending_config
        .insert("astap_binary_path".into(), bin.to_string_lossy().into());
}

#[given("a valid astap_db_directory")]
async fn given_valid_db_directory(world: &mut PlateSolverWorld) {
    let dir = world.temp_dir_path();
    let db = make_db_dir(&dir);
    world
        .pending_config
        .insert("astap_db_directory".into(), db.to_string_lossy().into());
}

#[given("a config with mock_astap as the binary path")]
async fn given_config_with_mock_astap(world: &mut PlateSolverWorld) {
    let dir = world.temp_dir_path();
    let bin = copy_mock_astap(&dir);
    world
        .pending_config
        .insert("astap_binary_path".into(), bin.to_string_lossy().into());
}

#[when("the wrapper starts")]
async fn when_wrapper_starts(world: &mut PlateSolverWorld) {
    // Inject a server block with port: 0 so concurrent test runs don't
    // collide on the default port; ServiceHandle parses the actual bound
    // port from the wrapper's stdout.
    world
        .pending_config
        .entry("server".to_string())
        .or_insert_with(|| serde_json::json!({ "port": 0 }));

    let dir = world.temp_dir_path();
    let cfg_path = dir.join("config.json");
    let body = serde_json::Value::Object(world.pending_config.clone()).to_string();
    std::fs::write(&cfg_path, body).expect("write config");

    // Try to start the wrapper non-fatally. If it boots and prints
    // bound_addr, ServiceHandle is held and Then-steps probe /health.
    // If the wrapper exits during validation (the failure scenarios),
    // try_start returns Err and we fall back to running it to exit
    // again so we can capture stderr / exit code for the assertions.
    let cfg_str = cfg_path.to_string_lossy().into_owned();
    match bdd_infra::ServiceHandle::try_start(env!("CARGO_PKG_NAME"), &cfg_str).await {
        Ok(handle) => {
            world.service_handle = Some(handle);
        }
        Err(_) => {
            world.run_wrapper_to_exit(cfg_path).await;
        }
    }
}

#[then("the wrapper exits non-zero")]
async fn then_wrapper_exits_nonzero(world: &mut PlateSolverWorld) {
    let code = world
        .last_wrapper_exit_code
        .expect("wrapper exit code missing — did the When step run?");
    assert_ne!(code, 0, "expected non-zero exit, got {code}");
}

#[then(expr = "the wrapper stderr names {string}")]
async fn then_wrapper_stderr_names(world: &mut PlateSolverWorld, needle: String) {
    let stderr = world
        .last_wrapper_stderr
        .as_deref()
        .expect("wrapper stderr missing");
    assert!(
        stderr.contains(&needle),
        "expected stderr to contain {needle:?}, got:\n{stderr}"
    );
}

#[then("the wrapper stderr references the README")]
async fn then_wrapper_stderr_references_readme(world: &mut PlateSolverWorld) {
    let stderr = world
        .last_wrapper_stderr
        .as_deref()
        .expect("wrapper stderr missing");
    assert!(
        stderr.contains("README") || stderr.contains("install"),
        "expected stderr to reference README/install, got:\n{stderr}"
    );
}

#[then("the wrapper prints bound_addr to stdout")]
async fn then_wrapper_prints_bound_addr(world: &mut PlateSolverWorld) {
    // ServiceHandle::start only returns when bound_addr was parsed
    // from stdout — its presence in world.service_handle is the
    // assertion. The .port field is a u16 set by parse_bound_port.
    let handle = world
        .service_handle
        .as_ref()
        .expect("service handle missing — wrapper didn't start?");
    assert!(handle.port > 0, "expected non-zero bound port");
}

#[then("the wrapper /health returns 200")]
async fn then_wrapper_health_returns_200(world: &mut PlateSolverWorld) {
    let url = format!("{}/health", world.wrapper_url());
    let resp = PlateSolverWorld::http_client()
        .get(&url)
        .send()
        .await
        .expect("GET /health");
    assert_eq!(resp.status(), 200);
}

fn copy_mock_astap(dir: &std::path::Path) -> PathBuf {
    let src = PlateSolverWorld::mock_astap_path();
    let dst = dir.join("astap_cli_mock");
    std::fs::copy(&src, &dst).expect("copy mock_astap");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755)).expect("chmod 755");
    }
    dst
}

fn make_db_dir(dir: &std::path::Path) -> PathBuf {
    let p = dir.join("db");
    std::fs::create_dir_all(&p).expect("mkdir db");
    p
}
