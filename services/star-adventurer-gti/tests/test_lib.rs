//! Tests for `lib.rs` — server startup and device registration.
//!
//! Verifies that `ServerBuilder` correctly registers the telescope device
//! based on configuration flags and starts the ASCOM Alpaca server.
//!
//! Requires the `mock` feature; all tests are skipped under Miri because it
//! cannot call socket syscalls. Tests run sequentially because the ASCOM
//! Alpaca discovery service binds to a fixed address, so only one server
//! can run at a time.
#![allow(clippy::await_holding_lock)]
#![cfg(feature = "mock")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    clippy::panic
)]
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
use std::time::Duration;

use star_adventurer_gti::{
    AlpacaServerConfig, Config, MockTransportFactory, MountConfig, ServerBuilder, TransportFactory,
};

static SERVER_LOCK: Mutex<()> = Mutex::new(());

fn test_config(mount_enabled: bool) -> Config {
    let mut cfg = Config::default();
    cfg.server = AlpacaServerConfig::new(0);
    cfg.mount = MountConfig {
        enabled: mount_enabled,
        ..cfg.mount
    };
    cfg
}

async fn spawn_server(config: Config) -> (u16, tokio::task::JoinHandle<()>) {
    let factory: Arc<dyn TransportFactory> = Arc::new(MockTransportFactory);
    let bound = ServerBuilder::new()
        .with_config(config)
        .with_transport_factory(factory)
        .build()
        .await
        .expect("server failed to bind");

    let port = bound.listen_addr().port();
    let handle = tokio::spawn(async move {
        let _ = bound.start(std::future::pending::<()>()).await;
    });
    (port, handle)
}

/// Poll the endpoint until it responds (status code returned, regardless
/// of value) or the deadline elapses. Replaces a fixed `sleep(50ms)` so
/// the tests are robust to slow CI runners.
async fn poll_status(port: u16, path: &str, deadline: Duration) -> u16 {
    let url = format!("http://127.0.0.1:{port}{path}");
    let start = std::time::Instant::now();
    loop {
        match reqwest::get(&url).await {
            Ok(resp) => return resp.status().as_u16(),
            Err(_) if start.elapsed() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("server did not respond on {url} within {deadline:?}: {e}"),
        }
    }
}

const READY_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn test_server_starts_with_mount_enabled() {
    let _lock = SERVER_LOCK.lock().unwrap();
    let (port, handle) = spawn_server(test_config(true)).await;
    let status = poll_status(port, "/api/v1/telescope/0/name", READY_TIMEOUT).await;
    assert_eq!(status, 200, "Telescope name endpoint should respond");
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn test_server_starts_with_mount_disabled() {
    let _lock = SERVER_LOCK.lock().unwrap();
    let (port, handle) = spawn_server(test_config(false)).await;
    let status = poll_status(port, "/api/v1/telescope/0/name", READY_TIMEOUT).await;
    assert_ne!(status, 200, "Telescope should not be registered");
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
#[cfg_attr(miri, ignore)]
async fn test_server_binds_to_os_assigned_port() {
    let _lock = SERVER_LOCK.lock().unwrap();
    let (port, handle) = spawn_server(test_config(true)).await;
    assert_ne!(port, 0, "OS should have assigned a real port");
    let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await;
    assert!(stream.is_ok(), "Server should be reachable on bound port");
    handle.abort();
    let _ = handle.await;
}
