//! Tests for lib.rs — server startup and device registration.
//!
//! Verifies that `ServerBuilder` correctly registers the focuser device based on
//! configuration flags and starts the ASCOM Alpaca server.
//!
//! Requires the `mock` feature. All tests are skipped under Miri since it cannot
//! call socket syscalls. Tests run sequentially behind `SERVER_LOCK` so the
//! spawned server + transport lifecycles never overlap (discovery is disabled
//! in these configs, so there is no shared discovery port to contend for).
#![allow(clippy::await_holding_lock)]
#![cfg(feature = "mock")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
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
use std::time::Duration;

use pa_scops_oag::config::{AlpacaServerConfig, FocuserConfig, SerialConfig};
use pa_scops_oag::{Config, MockScopsTransportFactory};
use rusty_photon_shared_transport::TransportFactory;

static SERVER_LOCK: Mutex<()> = Mutex::new(());

fn test_config(focuser_enabled: bool) -> Config {
    Config {
        serial: SerialConfig {
            port: "/dev/mock".to_string(),
            polling_interval: Duration::from_mins(1),
            ..Default::default()
        },
        server: AlpacaServerConfig::new(0),
        focuser: FocuserConfig {
            enabled: focuser_enabled,
            ..Default::default()
        },
    }
}

async fn spawn_server(
    config: Config,
) -> (
    u16,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let factory: Arc<dyn TransportFactory> = Arc::new(MockScopsTransportFactory::default());

    let bound = pa_scops_oag::ServerBuilder::new()
        .with_config(config)
        .with_factory(factory)
        .build()
        .await
        .expect("Server failed to bind");

    let port = bound.listen_addr().port();

    // Graceful stop channel: firing it lets `start()` run its teardown
    // (transport shutdown) instead of leaking the supervisor/poll tasks for
    // the rest of the test process, as aborting the task would.
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        let _ = bound
            .start(async move {
                let _ = stop_rx.await;
            })
            .await;
    });

    (port, stop_tx, handle)
}

async fn get_status(port: u16, path: &str) -> u16 {
    let url = format!("http://127.0.0.1:{port}{path}");
    reqwest::get(&url).await.unwrap().status().as_u16()
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn test_server_starts_with_focuser_enabled() {
    let _lock = SERVER_LOCK.lock().unwrap();
    let (port, stop, handle) = spawn_server(test_config(true)).await;

    let status = get_status(port, "/api/v1/focuser/0/name").await;
    assert_eq!(status, 200, "Focuser name endpoint should respond");

    stop.send(()).expect("server task should still be running");
    handle.await.expect("server task should join cleanly");
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn test_server_starts_with_focuser_disabled() {
    let _lock = SERVER_LOCK.lock().unwrap();
    let (port, stop, handle) = spawn_server(test_config(false)).await;

    let status = get_status(port, "/api/v1/focuser/0/name").await;
    assert_ne!(status, 200, "Focuser should not be registered");

    stop.send(()).expect("server task should still be running");
    handle.await.expect("server task should join cleanly");
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn test_server_binds_to_os_assigned_port() {
    let _lock = SERVER_LOCK.lock().unwrap();
    let (port, stop, handle) = spawn_server(test_config(true)).await;

    assert_ne!(port, 0, "OS should have assigned a real port");

    let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await;
    assert!(stream.is_ok(), "Server should be reachable on bound port");

    stop.send(()).expect("server task should still be running");
    handle.await.expect("server task should join cleanly");
}
