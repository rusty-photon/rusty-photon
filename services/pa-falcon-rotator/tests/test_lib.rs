//! Tests for `lib.rs` — server startup and device registration.
//!
//! Verifies that `ServerBuilder` binds the ASCOM Alpaca server and registers
//! the Rotator and Status Switch devices according to the config's
//! `enabled` flags. The tests only exercise management endpoints — neither
//! device is `set_connected(true)`, so the mock transport's `open` path
//! is never reached.
//!
//! Gated on `feature = "mock"` because the test imports
//! `MockFalconTransportFactory` from the falcon-rotator crate.
//!
//! Tests run sequentially via `SERVER_LOCK` to mirror the qhy-focuser /
//! ppba-driver precedent — even though each test sets
//! `discovery_port = None`, serialising the binds keeps a future change to
//! that default from racing.
#![cfg(feature = "mock")]
#![allow(clippy::await_holding_lock)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    clippy::used_underscore_binding,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::float_cmp,
    clippy::similar_names,
    clippy::struct_excessive_bools
)]

use std::sync::{Arc, Mutex};

use pa_falcon_rotator::{Config, MockFalconTransportFactory, ServerBuilder};
use rusty_photon_shared_transport::TransportFactory;

static SERVER_LOCK: Mutex<()> = Mutex::new(());

fn test_config(switch_enabled: bool) -> Config {
    let mut config = Config::default();
    // Bind an ephemeral port instead of the 11118 default so parallel
    // CI shards (and re-runs against a leftover process) never collide.
    config.server.port = 0;
    // Skip the UDP discovery service — there is one fixed port per host
    // and the management endpoints are reachable without it.
    config.server.discovery_port = None;
    config.server.tls = None;
    config.server.auth = None;
    config.switch.enabled = switch_enabled;
    config
}

async fn spawn_server(config: Config) -> (u16, tokio::task::JoinHandle<()>) {
    let factory: Arc<dyn TransportFactory> = Arc::new(MockFalconTransportFactory::default());

    let bound = ServerBuilder::new()
        .with_config(config)
        .with_factory(factory)
        .build()
        .await
        .expect("server failed to bind");

    let port = bound.listen_addr().port();

    let handle = tokio::spawn(async move {
        let _ = bound.start(std::future::pending::<()>()).await;
    });

    (port, handle)
}

async fn get(port: u16, path: &str) -> reqwest::Response {
    let url = format!("http://127.0.0.1:{port}{path}");
    reqwest::get(&url).await.unwrap()
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn test_server_starts_with_mock_factory() {
    let _lock = SERVER_LOCK.lock().unwrap();
    let (port, handle) = spawn_server(test_config(true)).await;

    let status = get(port, "/management/v1/description")
        .await
        .status()
        .as_u16();
    assert_eq!(
        status, 200,
        "management description endpoint should respond"
    );

    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn test_server_registers_both_devices() {
    let _lock = SERVER_LOCK.lock().unwrap();
    let (port, handle) = spawn_server(test_config(true)).await;

    let resp = get(port, "/management/v1/configureddevices").await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let devices = body["Value"].as_array().expect("Value array in response");
    assert_eq!(
        devices.len(),
        2,
        "rotator + switch should both be registered: {body}"
    );
    let types: Vec<&str> = devices
        .iter()
        .map(|d| d["DeviceType"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"Rotator"), "missing Rotator: {types:?}");
    assert!(types.contains(&"Switch"), "missing Switch: {types:?}");

    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn test_rotator_only_when_switch_disabled() {
    let _lock = SERVER_LOCK.lock().unwrap();
    let (port, handle) = spawn_server(test_config(false)).await;

    let resp = get(port, "/management/v1/configureddevices").await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let devices = body["Value"].as_array().expect("Value array in response");
    assert_eq!(
        devices.len(),
        1,
        "only rotator should be registered: {body}"
    );
    assert_eq!(devices[0]["DeviceType"].as_str(), Some("Rotator"));

    handle.abort();
    let _ = handle.await;
}
