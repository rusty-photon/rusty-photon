//! Test helpers shared across `rusty-photon-shared-transport` integration tests.
//!
//! Each integration test file (`race.rs`, `rollback.rs`, etc.) declares
//! `mod common;` to pull these in. Helpers here are deliberately minimal
//! — just enough for the lifecycle tests to construct a [`SharedTransport`]
//! over a stub codec and a programmable factory.

// The failure-injection helpers panic *inside closures* rather than inside a
// `#[test]` fn, which is what `allow-panic-in-tests` recognises.
#![allow(dead_code, clippy::panic)]

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_photon_shared_transport::{
    BoxFuture, Codec, FrameTransport, Hooks, SharedTransport, TransportError, TransportFactory,
    WhileOpen,
};
use thiserror::Error;
use tokio::sync::Mutex;

/// Codec used by tests: command and response are both `Vec<u8>`, decode
/// is identity, and `matches` is always true by default (override in
/// individual tests if needed).
#[derive(Clone, Default)]
pub struct EchoCodec;

#[derive(Debug, Error)]
#[error("echo codec error: {0}")]
pub struct EchoCodecError(pub String);

impl Codec for EchoCodec {
    type Command = Vec<u8>;
    type Response = Vec<u8>;
    type Error = EchoCodecError;

    fn encode(&self, cmd: &Self::Command) -> Vec<u8> {
        cmd.clone()
    }

    /// Identity decode, with one poke-able failure path: bytes
    /// starting with the `b"BAD"` prefix decode to `Err(EchoCodecError)`,
    /// exercising the `SessionError::Codec` arm. Tests that need to
    /// hit codec-error paths (e.g. `tests/reconnect.rs::codec_error_does_not_trigger_reconnect`)
    /// send a `b"BAD..."` payload; the `EchoTransport` echoes it back
    /// and decode fails on the response.
    fn decode(&self, bytes: &[u8]) -> Result<Self::Response, Self::Error> {
        if bytes.starts_with(b"BAD") {
            return Err(EchoCodecError(
                "decode rejected BAD prefix (test-only sentinel)".into(),
            ));
        }
        Ok(bytes.to_vec())
    }
}

/// An in-memory [`FrameTransport`] that echoes any sent frame back on
/// the next `recv_frame`. Sufficient for tests that only need to verify
/// connection setup / teardown — the request/response semantics are
/// trivial.
///
/// An optional `fail_recvs` flag lets a test inject a one-shot recv
/// failure (cleared on observation) to simulate mid-stream transport
/// loss without tearing down the underlying socket. The supervisor-
/// recovery tests use this to drive the end-to-end flow where
/// `Connection::request` fires the reconnect signal in response to a
/// real `TransportError`.
pub struct EchoTransport {
    last_sent: Option<Vec<u8>>,
    /// Marks whether `drop` has run. Useful to assert teardown closed
    /// the transport.
    pub dropped_flag: Option<Arc<AtomicBool>>,
    /// If `Some(_)`, `recv_frame` checks the flag at the top of the
    /// next call. When set, the flag is cleared and the call returns
    /// `TransportError::Io` — exactly the shape `Connection::request`
    /// pattern-matches on to fire the reconnect signal.
    fail_recvs: Option<Arc<AtomicBool>>,
}

impl EchoTransport {
    pub const fn new() -> Self {
        Self {
            last_sent: None,
            dropped_flag: None,
            fail_recvs: None,
        }
    }

    pub fn with_drop_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.dropped_flag = Some(flag);
        self
    }

    pub fn with_fail_recvs(mut self, flag: Arc<AtomicBool>) -> Self {
        self.fail_recvs = Some(flag);
        self
    }
}

#[async_trait]
impl FrameTransport for EchoTransport {
    async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.last_sent = Some(bytes.to_vec());
        Ok(())
    }

    async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
        buf.clear();
        // Test-injected failure: simulates a mid-stream transport
        // drop. `swap(false, …)` makes the failure one-shot — the
        // next `recv` after recovery succeeds.
        if let Some(flag) = self.fail_recvs.as_ref() {
            if flag.swap(false, Ordering::SeqCst) {
                return Err(TransportError::Io(io::Error::other(
                    "test-injected recv failure",
                )));
            }
        }
        if let Some(b) = self.last_sent.take() {
            buf.extend_from_slice(&b);
            Ok(())
        } else {
            Err(TransportError::Eof)
        }
    }
}

impl Drop for EchoTransport {
    fn drop(&mut self) {
        if let Some(flag) = self.dropped_flag.take() {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

/// Configurable behaviour of [`ProgrammableFactory::open`].
#[derive(Clone, Default)]
pub struct FactoryConfig {
    /// While `true`, every `open` call fails with [`TransportError::Eof`]
    /// (chosen because it can't be confused with a real network failure).
    /// Tests flip this from outside to simulate a recovering peer.
    pub fail: Arc<AtomicBool>,
    /// Number of times `open` was called.
    pub open_calls: Arc<AtomicU32>,
    /// Each opened transport's drop flag — pushed onto this when the
    /// factory creates a transport.
    pub drop_flags: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
    /// One-shot recv-failure flag shared with every [`EchoTransport`]
    /// the factory builds. Tests set this to inject a mid-stream
    /// `TransportError::Io` into the very next `Session::request`,
    /// which routes through `Connection::request`'s signal-fire path
    /// and notifies the supervisor.
    pub fail_recvs: Arc<AtomicBool>,
}

impl FactoryConfig {
    pub fn opens(&self) -> u32 {
        self.open_calls.load(Ordering::SeqCst)
    }

    pub async fn dropped_count(&self) -> usize {
        let flags = self.drop_flags.lock().await;
        flags.iter().filter(|f| f.load(Ordering::SeqCst)).count()
    }

    pub fn set_fail(&self, value: bool) {
        self.fail.store(value, Ordering::SeqCst);
    }

    pub fn failing() -> Self {
        Self {
            fail: Arc::new(AtomicBool::new(true)),
            ..Self::default()
        }
    }
}

/// [`TransportFactory`] with configurable success/error behaviour and an
/// open-call counter. Used by every integration test in this crate.
pub struct ProgrammableFactory {
    config: FactoryConfig,
    fail_after_succeeds: Option<u32>,
}

impl ProgrammableFactory {
    pub const fn new(config: FactoryConfig) -> Self {
        Self {
            config,
            fail_after_succeeds: None,
        }
    }

    /// After `n` successful opens, every subsequent `open` fails.
    pub const fn fail_after(mut self, n: u32) -> Self {
        self.fail_after_succeeds = Some(n);
        self
    }
}

#[async_trait]
impl TransportFactory for ProgrammableFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        let prior = self.config.open_calls.fetch_add(1, Ordering::SeqCst);
        if self.config.fail.load(Ordering::SeqCst) {
            return Err(TransportError::Eof);
        }
        if let Some(n) = self.fail_after_succeeds {
            if prior >= n {
                return Err(TransportError::Eof);
            }
        }
        let drop_flag = Arc::new(AtomicBool::new(false));
        self.config.drop_flags.lock().await.push(drop_flag.clone());
        let transport = EchoTransport::new()
            .with_drop_flag(drop_flag)
            .with_fail_recvs(self.config.fail_recvs.clone());
        Ok(Box::new(transport))
    }
}

/// Build a [`SharedTransport`] with the [`EchoCodec`], no while-open
/// task, and an infallible no-op handshake/teardown. Returns the
/// transport plus a handle to the factory config so tests can read
/// `opens()`/`dropped_count()`.
pub fn build_noop_transport() -> (Arc<SharedTransport<EchoCodec>>, FactoryConfig) {
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let st = SharedTransport::new(factory, EchoCodec, Hooks::noop());
    (st, cfg)
}

/// Hooks builder with one counter per lifecycle hook. Useful to assert
/// the right hook fired the right number of times across N connect /
/// disconnect / start / shutdown cycles.
///
/// `teardown_calls` is the historical name retained for the `on_last_disconnect`
/// counter — old tests pre-date the hook split.
#[expect(
    clippy::struct_field_names,
    reason = "the shared `_calls` postfix is the point: one counter per hook"
)]
pub struct CountingHooks {
    pub handshake_calls: Arc<AtomicU32>,
    pub teardown_calls: Arc<AtomicU32>,
    pub shutdown_calls: Arc<AtomicU32>,
}

impl Default for CountingHooks {
    fn default() -> Self {
        Self {
            handshake_calls: Arc::new(AtomicU32::new(0)),
            teardown_calls: Arc::new(AtomicU32::new(0)),
            shutdown_calls: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl CountingHooks {
    pub fn hooks(&self) -> Hooks<EchoCodec> {
        let hs = self.handshake_calls.clone();
        let td = self.teardown_calls.clone();
        let sd = self.shutdown_calls.clone();
        Hooks {
            handshake: Box::new(move |_conn| {
                let hs = hs.clone();
                Box::pin(async move {
                    hs.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }),
            on_last_disconnect: Box::new(move |_conn| {
                let td = td.clone();
                Box::pin(async move {
                    td.fetch_add(1, Ordering::SeqCst);
                })
            }),
            shutdown: Box::new(move |_conn| {
                let sd = sd.clone();
                Box::pin(async move {
                    sd.fetch_add(1, Ordering::SeqCst);
                })
            }),
            while_open: None,
        }
    }
}

/// Convenience: short delay used in async tests to let spawned tasks
/// make progress without the test having to know exactly when.
pub async fn yield_briefly() {
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
}

/// Hooks where the handshake always errors. Useful for rollback tests.
pub fn failing_handshake_hooks() -> Hooks<EchoCodec> {
    Hooks {
        handshake: Box::new(|_conn| {
            Box::pin(async { Err(EchoCodecError("handshake refused".into())) })
        }),
        on_last_disconnect: Box::new(|_| Box::pin(async {})),
        shutdown: Box::new(|_| Box::pin(async {})),
        while_open: None,
    }
}

/// Hooks where the handshake panics. Used to verify the rollback guard
/// covers unwind paths, not just `Err` returns.
pub fn panicking_handshake_hooks() -> Hooks<EchoCodec> {
    Hooks {
        handshake: Box::new(|_conn| Box::pin(async { panic!("handshake panic for test") })),
        on_last_disconnect: Box::new(|_| Box::pin(async {})),
        shutdown: Box::new(|_| Box::pin(async {})),
        while_open: None,
    }
}

/// Hooks where the while-open closure panics during construction
/// (before it can return a future). Verifies the rollback guard stays
/// armed until after `slot` / `available` would be published — the
/// failure mode flagged in Copilot's review of PR #269.
pub fn panicking_while_open_constructor_hooks() -> Hooks<EchoCodec> {
    Hooks {
        handshake: Box::new(|_| Box::pin(async { Ok(()) })),
        on_last_disconnect: Box::new(|_| Box::pin(async {})),
        shutdown: Box::new(|_| Box::pin(async {})),
        while_open: Some(Box::new(|_ctx| panic!("while_open closure panic for test"))),
    }
}

/// Hooks with a configurable while-open closure. The closure is given
/// access to `started` (set once the task body begins) and `exited`
/// (set when the task body returns).
pub struct WhileOpenHooks {
    pub started: Arc<AtomicBool>,
    pub exited: Arc<AtomicBool>,
}

impl Default for WhileOpenHooks {
    fn default() -> Self {
        Self {
            started: Arc::new(AtomicBool::new(false)),
            exited: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl WhileOpenHooks {
    /// Hook where the while-open task immediately marks `started`, then
    /// loops on `select! { cancel.cancelled() | tick }` and exits when
    /// cancelled.
    pub fn cooperative_hooks(&self) -> Hooks<EchoCodec> {
        let started = self.started.clone();
        let exited = self.exited.clone();
        Hooks {
            handshake: Box::new(|_| Box::pin(async { Ok(()) })),
            on_last_disconnect: Box::new(|_| Box::pin(async {})),
            shutdown: Box::new(|_| Box::pin(async {})),
            while_open: Some(Box::new(move |ctx: WhileOpen<EchoCodec>| {
                let started = started.clone();
                let exited = exited.clone();
                Box::pin(async move {
                    started.store(true, Ordering::SeqCst);
                    let mut interval = tokio::time::interval(Duration::from_millis(20));
                    loop {
                        tokio::select! {
                            () = ctx.cancelled() => break,
                            _ = interval.tick() => {},
                        }
                    }
                    exited.store(true, Ordering::SeqCst);
                })
            })),
        }
    }

    /// Hook where the while-open task ignores cancellation entirely.
    /// Used to verify the bounded-timeout abort path. `exited` is
    /// intentionally never set — the test asserts that `abort()` is
    /// what stops this task, so the cooperative-exit flag must stay
    /// false.
    pub fn stubborn_hooks(&self) -> Hooks<EchoCodec> {
        let started = self.started.clone();
        Hooks {
            handshake: Box::new(|_| Box::pin(async { Ok(()) })),
            on_last_disconnect: Box::new(|_| Box::pin(async {})),
            shutdown: Box::new(|_| Box::pin(async {})),
            while_open: Some(Box::new(move |_ctx: WhileOpen<EchoCodec>| {
                let started = started.clone();
                Box::pin(async move {
                    started.store(true, Ordering::SeqCst);
                    // Sleep forever, ignoring cancellation.
                    loop {
                        tokio::time::sleep(Duration::from_hours(1)).await;
                    }
                })
            })),
        }
    }

    /// Hook where the while-open task marks `started`, yields once so
    /// it's observably alive, then panics. Used to exercise the
    /// `Ok(Err(join_err))` join-error arms in `shutdown()` and
    /// `run_cleanup_locked()` — tokio catches the panic at the task
    /// boundary and surfaces it via `JoinHandle::await`, which both
    /// teardown paths log and continue past.
    pub fn panicking_hooks(&self) -> Hooks<EchoCodec> {
        let started = self.started.clone();
        Hooks {
            handshake: Box::new(|_| Box::pin(async { Ok(()) })),
            on_last_disconnect: Box::new(|_| Box::pin(async {})),
            shutdown: Box::new(|_| Box::pin(async {})),
            while_open: Some(Box::new(move |_ctx: WhileOpen<EchoCodec>| {
                let started = started.clone();
                Box::pin(async move {
                    started.store(true, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    panic!("while_open panic for test");
                })
            })),
        }
    }
}

/// Hooks with a counter for while-open spawn invocations. Distinct from
/// [`WhileOpenHooks`] because the cooperative variant uses an
/// `AtomicBool` for `started` — switching to `AtomicU32` everywhere
/// would churn existing tests for no benefit. Used by the reconnect
/// respawn test to assert the closure was invoked twice (once at
/// start, once at `attempt_reconnect`).
pub struct CountingWhileOpenHooks {
    pub spawns: Arc<AtomicU32>,
    pub cancelled: Arc<AtomicU32>,
}

impl Default for CountingWhileOpenHooks {
    fn default() -> Self {
        Self {
            spawns: Arc::new(AtomicU32::new(0)),
            cancelled: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl CountingWhileOpenHooks {
    /// Cooperative task that bumps `spawns` on entry and `cancelled` on
    /// exit-via-cancellation. Used to verify reconnect cancels the old
    /// `while_open` task and spawns a fresh one.
    pub fn hooks(&self) -> Hooks<EchoCodec> {
        let spawns = self.spawns.clone();
        let cancelled = self.cancelled.clone();
        Hooks {
            handshake: Box::new(|_| Box::pin(async { Ok(()) })),
            on_last_disconnect: Box::new(|_| Box::pin(async {})),
            shutdown: Box::new(|_| Box::pin(async {})),
            while_open: Some(Box::new(move |ctx: WhileOpen<EchoCodec>| {
                let spawns = spawns.clone();
                let cancelled = cancelled.clone();
                Box::pin(async move {
                    spawns.fetch_add(1, Ordering::SeqCst);
                    let mut interval = tokio::time::interval(Duration::from_millis(20));
                    loop {
                        tokio::select! {
                            () = ctx.cancelled() => {
                                cancelled.fetch_add(1, Ordering::SeqCst);
                                break;
                            }
                            _ = interval.tick() => {},
                        }
                    }
                })
            })),
        }
    }
}

/// Build a shared transport using the supplied hooks; reuse the
/// no-op factory.
pub fn build_with_hooks(
    hooks: Hooks<EchoCodec>,
) -> (Arc<SharedTransport<EchoCodec>>, FactoryConfig) {
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    (SharedTransport::new(factory, EchoCodec, hooks), cfg)
}

/// Use a custom factory + hooks combo.
pub fn build_with_factory_and_hooks(
    factory: Arc<dyn TransportFactory>,
    hooks: Hooks<EchoCodec>,
) -> Arc<SharedTransport<EchoCodec>> {
    SharedTransport::new(factory, EchoCodec, hooks)
}

// Silence the "BoxFuture is unused" warning on test files that don't
// reference it directly — keeping the import here so per-test `use`
// statements stay tidy.
const _: fn() -> BoxFuture<'static, ()> = || Box::pin(async {});
