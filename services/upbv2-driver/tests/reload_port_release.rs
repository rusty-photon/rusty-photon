//! The in-process reload must give the serial port back before it asks
//! for it again.
//!
//! `main.rs`'s reload loop awaits `start()` to completion so the old
//! server drains HTTP and the transport shuts down, and only then
//! rebuilds. Nothing tested that claim: the BDD mock hands out as many
//! concurrent transports as it is asked for, so a reload that leaked
//! the port would pass there and fail only on a rig, where a COM port
//! is exclusive per handle and the second open returns `Access is
//! denied`.
//!
//! These tests drive the same build → serve → rebuild sequence against
//! a factory that refuses to open while a transport it handed out
//! earlier is still alive.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    clippy::used_underscore_binding,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::similar_names,
    clippy::struct_excessive_bools
)]

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
use upbv2_driver::{AlpacaServerConfig, Config, MockUpbv2TransportFactory, ServerBuilder};

/// Wraps the UPBv2 mock in the one property the mock does not model: a
/// handle only one holder at a time may have. `open()` refuses with the
/// Windows wording while a previously handed-out transport is alive.
#[derive(Default)]
struct ExclusiveMockFactory {
    inner: MockUpbv2TransportFactory,
    live: Arc<AtomicBool>,
    refusals: Arc<AtomicU32>,
}

impl ExclusiveMockFactory {
    fn refusals(&self) -> u32 {
        self.refusals.load(Ordering::SeqCst)
    }
}

struct ExclusiveTransport {
    inner: Box<dyn FrameTransport>,
    live: Arc<AtomicBool>,
}

impl Drop for ExclusiveTransport {
    fn drop(&mut self) {
        self.live.store(false, Ordering::SeqCst);
    }
}

#[async_trait]
impl FrameTransport for ExclusiveTransport {
    async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.inner.send_frame(bytes).await
    }

    async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
        self.inner.recv_frame(buf).await
    }
}

#[async_trait]
impl TransportFactory for ExclusiveMockFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        if self.live.swap(true, Ordering::SeqCst) {
            self.refusals.fetch_add(1, Ordering::SeqCst);
            return Err(TransportError::Open(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Access is denied.",
            )));
        }
        Ok(Box::new(ExclusiveTransport {
            inner: self.inner.open().await?,
            live: Arc::clone(&self.live),
        }))
    }
}

/// Port 0 so the OS picks the listener; the mock ignores the serial one.
fn test_config() -> Config {
    Config {
        server: AlpacaServerConfig::new(0),
        ..Config::default()
    }
}

/// `Arc<ExclusiveMockFactory>` as the trait object the builder takes.
fn shared(factory: &Arc<ExclusiveMockFactory>) -> Arc<dyn TransportFactory> {
    factory.clone()
}

/// One iteration of the reload loop: build, serve, tear down. Mirrors
/// `main.rs`.
async fn serve_once(
    factory: &Arc<ExclusiveMockFactory>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bound = ServerBuilder::new(test_config())
        .with_factory(shared(factory))
        .build()
        .await?;
    bound.start(async {}).await
}

#[tokio::test]
async fn a_reload_can_re_open_the_port_the_previous_run_held() {
    let factory = Arc::new(ExclusiveMockFactory::default());

    serve_once(&factory).await.unwrap();
    serve_once(&factory).await.unwrap();
    serve_once(&factory).await.unwrap();

    assert_eq!(
        factory.refusals(),
        0,
        "each rebuild must find the port released by the run before it"
    );
}

#[tokio::test]
async fn a_reload_with_a_client_connected_still_re_opens_the_port() {
    // The rig case: an ASCOM client had the Switch device connected,
    // so a `Session` was outstanding when the reload arrived.
    let factory = Arc::new(ExclusiveMockFactory::default());
    let bound = ServerBuilder::new(test_config())
        .with_factory(shared(&factory))
        .build()
        .await
        .unwrap();
    let addr = bound.listen_addr();

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        bound
            .start(async move {
                let _ = stopped.await;
            })
            .await
    });

    let connected = reqwest::Client::new()
        .put(format!("http://{addr}/api/v1/switch/0/connected"))
        .form(&[
            ("Connected", "true"),
            ("ClientID", "1"),
            ("ClientTransactionID", "1"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(connected.status(), 200);

    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();

    serve_once(&factory).await.unwrap();
    assert_eq!(
        factory.refusals(),
        0,
        "a connected client must not keep the port past the reload"
    );
}
