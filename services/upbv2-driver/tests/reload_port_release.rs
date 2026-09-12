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
//!
//! What they pin is the ordering: every holder is gone before the
//! rebuild asks. They deliberately do not model the other half of the
//! Windows story, where the OS handle outlives the value that owned it —
//! this double releases on drop, and a mock factory never reaches
//! `open_serial_port` anyway, so the bounded retry that rides that out
//! could not run here. Its tests are `open_with_retries_*` in
//! `rusty-photon-shared-transport`.

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
    opens: Arc<AtomicU32>,
    refusals: Arc<AtomicU32>,
}

impl ExclusiveMockFactory {
    /// Opens that handed out a transport. Load-bearing alongside
    /// `refusals`: zero refusals is also what a run that never reached
    /// this factory would report, so the count is what says the
    /// eager-open path ran at all.
    fn opens(&self) -> u32 {
        self.opens.load(Ordering::SeqCst)
    }

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
        // Give the port back if the mock underneath refuses: the flag
        // stands for a handle handed out, and on this path none was.
        // Left set it would refuse every later open and the test would
        // be measuring this double rather than the code.
        let inner = match self.inner.open().await {
            Ok(inner) => inner,
            Err(e) => {
                self.live.store(false, Ordering::SeqCst);
                return Err(e);
            }
        };
        self.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(ExclusiveTransport {
            inner,
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
        factory.opens(),
        3,
        "every run must have eagerly opened the port — without this the \
         refusal count below is vacuous"
    );
    assert_eq!(
        factory.refusals(),
        0,
        "each rebuild must find the port released by the run before it"
    );
}

#[tokio::test]
async fn a_reload_after_a_client_connected_still_re_opens_the_port() {
    // The rig case: an ASCOM client had the Switch device connected
    // when the reload arrived, so the teardown runs against a device
    // that holds a `Session` rather than a freshly built one.
    //
    // Note what this does *not* prove. `start()` moves the router into
    // its serve future, so the device — and with it the session — is
    // dropped when serving ends, before the transport shuts down. That
    // a `shutdown()` still holding a live session releases the port is
    // the shared crate's to pin, in
    // `shutdown_releases_the_conduit_while_a_session_is_still_alive`.
    let factory = Arc::new(ExclusiveMockFactory::default());
    let bound = ServerBuilder::new(test_config())
        .with_factory(shared(&factory))
        .build()
        .await
        .unwrap();
    // The listener binds the wildcard address; connect to the loopback
    // on the port it was given. Windows rejects a connect to 0.0.0.0
    // with `WSAEADDRNOTAVAIL` where Linux quietly reads it as localhost.
    let port = bound.listen_addr().port();

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        bound
            .start(async move {
                let _ = stopped.await;
            })
            .await
    });

    let connected = reqwest::Client::new()
        .put(format!("http://127.0.0.1:{port}/api/v1/switch/0/connected"))
        .form(&[
            ("Connected", "true"),
            ("ClientID", "1"),
            ("ClientTransactionID", "1"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(connected.status(), 200);
    // Alpaca reports a failed call as a non-zero `ErrorNumber` under
    // HTTP 200, so the status alone would let a refused connect
    // through and leave this exercising an idle device.
    let body: serde_json::Value = connected.json().await.unwrap();
    assert_eq!(
        body["ErrorNumber"], 0,
        "the device must actually have connected: {body}"
    );

    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();

    serve_once(&factory).await.unwrap();
    assert_eq!(
        factory.opens(),
        2,
        "both the served run and the rebuild must have opened the port"
    );
    assert_eq!(
        factory.refusals(),
        0,
        "a client connection must leave nothing holding the port"
    );
}
