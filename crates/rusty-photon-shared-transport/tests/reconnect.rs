//! Phase 0b: reconnect supervisor and connection-cell swap.
//!
//! These tests verify the supervisor's transport-recovery contract:
//!
//! - `reconnect_now()` opens a fresh transport and runs the handshake
//!   against it, then atomically swaps it into the cell.
//! - Live `Session<C>` references handed out by `acquire()` automatically
//!   route through the new transport on their next `request()` call —
//!   no client-visible Session recreation is needed (the
//!   live-session-survival contract from the transport-lifecycle plan).
//! - `is_reconnecting()` flips false again once recovery succeeds; the
//!   supervisor task is wired so external observers can poll the state.
//! - `shutdown()` cancels the supervisor cleanly.
//!
//! Notify-driven reconnect (`Connection::request` firing the signal on a
//! real transport error) lands the moment a real service's mock factory
//! exercises a mid-stream drop in Phases 1-5; this file pins the
//! supervisor / cell / `reconnect_now` mechanics in isolation.
//!
//! What deliberately is **not** here yet (Phase 0b follow-up):
//!
//! - On-acquire eager reconnect (an `acquire()` mid-reconnect that
//!   triggers a synchronous attempt with `reconnect_acquire_timeout`).

#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::indexing_slicing
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

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common::{
    build_with_factory_and_hooks, CountingHooks, CountingWhileOpenHooks, FactoryConfig,
    ProgrammableFactory, WhileOpenHooks,
};
use rusty_photon_shared_transport::TransportFactory;

/// Poll `cond` every 10ms until it returns true or `timeout` elapses.
/// Used to wait on supervisor-driven async work that has no other
/// signal to await on (the supervisor task is internal to the crate).
async fn wait_until<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cond()
}

#[tokio::test]
async fn reconnect_now_opens_fresh_transport_and_runs_handshake() {
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    assert_eq!(cfg.opens(), 1);
    assert_eq!(counting.handshake_calls.load(Ordering::SeqCst), 1);

    st.reconnect_now().await.unwrap();
    assert_eq!(
        cfg.opens(),
        2,
        "reconnect_now must open a fresh transport via the factory"
    );
    assert_eq!(
        counting.handshake_calls.load(Ordering::SeqCst),
        2,
        "reconnect_now must run the handshake against the new transport"
    );
    assert!(
        !st.is_reconnecting(),
        "reconnecting flag must clear on successful reconnect"
    );
    assert!(
        st.is_available(),
        "available flag must be true after successful reconnect"
    );
}

#[tokio::test]
async fn live_session_survives_reconnect_via_cell_swap() {
    // The headline Phase 0b contract: a Session acquired before a
    // reconnect transparently picks up the new transport on its next
    // request, because the supervisor swaps the inner Arc<Connection>
    // inside the cell that both SharedTransport and Sessions share.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    let session = st.acquire().await.unwrap();

    let r1 = session.request(b"ping".to_vec()).await.unwrap();
    assert_eq!(r1, b"ping", "request must work before reconnect");

    st.reconnect_now().await.unwrap();
    assert_eq!(cfg.opens(), 2);

    // Critical assertion: the same Session reference, no recreation,
    // routes through the new transport.
    let r2 = session.request(b"pong".to_vec()).await.unwrap();
    assert_eq!(
        r2, b"pong",
        "live session must survive a reconnect via cell swap"
    );

    session.close().await.unwrap();
}

#[tokio::test]
async fn reconnect_now_does_not_change_refcount() {
    // Reconnect is a transport-level concern; the external client
    // refcount must not move. A session acquired before and a session
    // acquired after see the same fast path.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    let s1 = st.acquire().await.unwrap();
    assert_eq!(cfg.opens(), 1);

    st.reconnect_now().await.unwrap();
    assert_eq!(cfg.opens(), 2);
    assert_eq!(
        counting.handshake_calls.load(Ordering::SeqCst),
        2,
        "exactly one handshake per open"
    );

    // Acquire another after the reconnect — still the fast path
    // (slot's cell holds the new connection).
    let s2 = st.acquire().await.unwrap();
    assert_eq!(
        cfg.opens(),
        2,
        "post-reconnect acquire reuses the new connection — no third open"
    );

    s1.close().await.unwrap();
    s2.close().await.unwrap();
}

#[tokio::test]
async fn reconnect_now_failure_leaves_supervisor_in_reconnecting() {
    // factory.open() fails: reconnect_now returns Err and the
    // supervisor stays in Reconnecting until a successful retry.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();

    // Flip the factory to fail mode; reconnect_now must surface the
    // failure and the state must reflect ongoing recovery.
    cfg.set_fail(true);
    let err = st.reconnect_now().await.unwrap_err();
    let display = format!("{err}");
    assert!(
        display.contains("EOF") || display.contains("eof") || display.contains("transport"),
        "expected a transport-loss-shaped error from a failing factory.open, got: {display}"
    );
    assert!(
        st.is_reconnecting(),
        "transport must stay in Reconnecting until a successful retry"
    );
    assert!(
        !st.is_available(),
        "available must be false during Reconnecting"
    );

    // Recovery: factory succeeds again, kick another attempt manually.
    cfg.set_fail(false);
    st.reconnect_now().await.unwrap();
    assert!(!st.is_reconnecting());
    assert!(st.is_available());
}

#[tokio::test]
async fn acquire_during_reconnect_returns_session_not_shutdown_error() {
    // Regression: `acquire()` checks `service_lifetime && !available` to
    // detect "shutdown already ran" and refuses. But `available` is also
    // false while the supervisor is in `Reconnecting`, so without
    // additionally gating on `!reconnecting`, a first-client acquire
    // during a reconnect window wrongly returns
    // `Io("transport has been shut down")` instead of letting the caller
    // get a session whose first `request()` short-circuits with
    // `TransportError::Reconnecting` (the documented contract).
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();

    // Drive the supervisor into Reconnecting with no active sessions
    // (refcount == 0): a failed reconnect_now flips both flags
    // (`available=false`, `reconnecting=true`) and leaves them that way
    // until a successful retry.
    cfg.set_fail(true);
    let _ = st.reconnect_now().await; // expected to fail
    assert!(st.is_reconnecting());
    assert!(!st.is_available());

    // The acquire must succeed and hand back a session — the request
    // path is the right place to surface `Reconnecting`, not the
    // acquire path.
    let session = st
        .acquire()
        .await
        .expect("acquire during reconnect must succeed (return a session), not return Io shutdown");

    // First request through the session surfaces the in-flight
    // reconnect via the existing short-circuit.
    let err = session.request(b"x".to_vec()).await.unwrap_err();
    let display = format!("{err}");
    assert!(
        display.contains("reconnecting"),
        "expected TransportError::Reconnecting from request, got: {display}"
    );

    // Cleanup: recover, then close + shutdown.
    cfg.set_fail(false);
    st.reconnect_now().await.unwrap();
    session.close().await.unwrap();
    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_request_short_circuits_to_reconnecting_during_failure() {
    // While the supervisor is in Reconnecting (a failed reconnect
    // attempt leaves us there), Session::request must short-circuit
    // with TransportError::Reconnecting rather than waiting on the
    // dying transport's command lock.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    let session = st.acquire().await.unwrap();

    // Induce a Reconnecting state via a failed reconnect_now.
    cfg.set_fail(true);
    let _ = st.reconnect_now().await; // expected to fail
    assert!(st.is_reconnecting());

    let err = session.request(b"x".to_vec()).await.unwrap_err();
    let display = format!("{err}");
    assert!(
        display.contains("reconnecting"),
        "expected TransportError::Reconnecting, got: {display}"
    );

    // Cleanup: recover, then close.
    cfg.set_fail(false);
    st.reconnect_now().await.unwrap();
    session.close().await.unwrap();
}

#[tokio::test]
async fn shutdown_cancels_supervisor_cleanly() {
    // After shutdown, no further reconnect attempts should happen —
    // the supervisor is cancelled. A subsequent acquire returns the
    // shut-down error.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    st.shutdown().await.unwrap();

    // The supervisor would normally react to factory.open() succeeding
    // again, but it's been cancelled — so additional opens shouldn't
    // happen on a periodic-timer tick.
    common::yield_briefly().await;
    assert_eq!(
        cfg.opens(),
        1,
        "no more opens after shutdown — supervisor must be cancelled"
    );

    // is_reconnecting cleared by shutdown.
    assert!(!st.is_reconnecting());
}

#[tokio::test]
async fn codec_error_does_not_trigger_reconnect() {
    // The supervisor only reacts to `TransportError`s — codec-level
    // mismatches (malformed JSON, skip-budget exhaustion, etc.) are
    // protocol bugs, not hardware loss, and reconnecting wouldn't fix
    // them. `Connection::request` enforces this by routing the
    // signal-fire only through the `Transport` arms (see
    // `connection.rs::signal_reconnect`); this test pins that contract
    // end-to-end.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    let session = st.acquire().await.unwrap();

    // EchoCodec's decoder rejects any frame starting with `b"BAD"` —
    // see the test-only sentinel in `common/mod.rs::EchoCodec::decode`.
    // The EchoTransport echoes whatever was sent, so the codec
    // failure fires on the response decode.
    let err = session.request(b"BAD".to_vec()).await.unwrap_err();
    let display = format!("{err}");
    assert!(
        display.contains("BAD prefix"),
        "expected the BAD-prefix codec error to bubble through SessionError::Codec, got: {display}"
    );

    // Give any (incorrect) supervisor reaction a chance to land
    // before asserting the negative.
    common::yield_briefly().await;

    assert!(
        !st.is_reconnecting(),
        "codec error must not advance the supervisor to Reconnecting"
    );
    assert_eq!(
        cfg.opens(),
        1,
        "no fresh factory.open() should have happened — reconnect is for transport loss, not codec mismatches"
    );
    assert_eq!(
        counting.handshake_calls.load(Ordering::SeqCst),
        1,
        "no fresh handshake should have run for the same reason"
    );

    // Confirm the transport is still usable: a clean payload still
    // round-trips through the same session, proving no spurious cell
    // swap happened.
    let r = session.request(b"ping".to_vec()).await.unwrap();
    assert_eq!(r, b"ping");

    session.close().await.unwrap();
}

#[tokio::test]
async fn supervisor_periodic_ticker_recovers_after_failed_reconnect() {
    // Cover the supervisor_loop's periodic-ticker branch (the
    // `tokio::time::sleep(interval) => {}` arm) plus the post-wake
    // attempt_reconnect success and failure log paths. A short
    // `set_reconnect_interval` shrinks the default 5s cadence so this
    // runs in real wall-clock time without paused-clock plumbing.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.set_reconnect_interval(Duration::from_millis(30)).await;
    st.start().await.unwrap();
    let opens_after_start = cfg.opens();
    assert_eq!(opens_after_start, 1);

    // Drive supervisor into Reconnecting via a failing reconnect_now.
    // attempt_reconnect runs again on each periodic tick; while
    // `fail` is still set, those retries log the warn at line 347-352
    // — let one tick land before recovery.
    cfg.set_fail(true);
    let _ = st.reconnect_now().await;
    assert!(st.is_reconnecting());

    // Wait for at least one periodic retry while still failing — this
    // covers the Err(e) warn-log arm. `opens` increments per
    // factory.open call (success or failure), so the count growing
    // proves the ticker fired and attempt_reconnect ran.
    let saw_periodic_retry = wait_until(
        || cfg.opens() >= opens_after_start + 2,
        Duration::from_millis(500),
    )
    .await;
    assert!(
        saw_periodic_retry,
        "supervisor's periodic ticker must trigger at least one retry attempt while reconnecting"
    );

    // Now let recovery succeed: the next ticker tick lands in the
    // Ok(()) arm and clears the flags.
    cfg.set_fail(false);
    let recovered = wait_until(|| !st.is_reconnecting(), Duration::from_secs(1)).await;
    assert!(
        recovered,
        "supervisor must recover once factory stops failing"
    );
    assert!(st.is_available());
}

#[tokio::test]
async fn transport_error_fires_notify_and_supervisor_recovers() {
    // End-to-end Notify path: Connection::request sees a TransportError,
    // calls signal_reconnect, the supervisor wakes from notified(),
    // flips the flags (signaled=true → reconnecting=true, available=false),
    // and runs a fresh attempt_reconnect that succeeds. Covers
    // supervisor_loop's notify arm (lines 323-325) and the
    // signaled-true flag-flip block (333-335).
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    let session = st.acquire().await.unwrap();

    // Arm a one-shot recv failure; the very next request's recv_frame
    // returns TransportError::Io, which signals the reconnect notify
    // and propagates the error back.
    cfg.fail_recvs.store(true, Ordering::SeqCst);
    let err = session.request(b"ping".to_vec()).await.unwrap_err();
    let display = format!("{err}");
    assert!(
        display.contains("test-injected") || display.contains("transport"),
        "expected the injected recv failure to bubble through, got: {display}"
    );

    // Supervisor wakes asynchronously. Wait until it has opened a
    // fresh transport (opens == 2) and cleared the reconnecting flag.
    let recovered = wait_until(
        || cfg.opens() >= 2 && !st.is_reconnecting(),
        Duration::from_secs(2),
    )
    .await;
    assert!(
        recovered,
        "supervisor must observe the notify, attempt_reconnect, and clear reconnecting"
    );
    assert!(st.is_available());
    assert_eq!(
        counting.handshake_calls.load(Ordering::SeqCst),
        2,
        "reconnect must re-run the handshake against the fresh transport"
    );

    // Live session survives the swap.
    let r = session.request(b"ping".to_vec()).await.unwrap();
    assert_eq!(r, b"ping");
    session.close().await.unwrap();
}

#[tokio::test]
async fn reconnect_cancels_old_while_open_and_respawns_against_new_connection() {
    // attempt_reconnect's while_open lifecycle: an existing task is
    // cancelled before the cell swap (lines 391-411) and a new task
    // is spawned against the fresh connection afterwards (lines
    // 430-436). Counted-spawn hooks make both halves observable.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let wo = CountingWhileOpenHooks::default();
    let st = build_with_factory_and_hooks(factory, wo.hooks());

    st.start().await.unwrap();
    common::yield_briefly().await;
    assert_eq!(
        wo.spawns.load(Ordering::SeqCst),
        1,
        "start() spawns the first while_open task"
    );
    assert_eq!(wo.cancelled.load(Ordering::SeqCst), 0);

    st.reconnect_now().await.unwrap();
    common::yield_briefly().await;
    assert_eq!(
        wo.spawns.load(Ordering::SeqCst),
        2,
        "reconnect must respawn while_open against the fresh connection"
    );
    assert_eq!(
        wo.cancelled.load(Ordering::SeqCst),
        1,
        "reconnect must cancel the prior while_open task"
    );

    // Shutdown cancels the second task too — final tally: 1 spawn +
    // 1 respawn, both cancelled exactly once.
    st.shutdown().await.unwrap();
    common::yield_briefly().await;
    assert_eq!(wo.cancelled.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn reconnect_now_before_start_returns_slot_empty_error() {
    // attempt_reconnect's defensive "slot empty" arm (lines 422-424):
    // reconnect_now flips reconnecting/available and calls
    // attempt_reconnect directly, which then sees an empty slot
    // because start() never populated it. The error is preferable
    // to a panic in this defensive path.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    // No start() — slot stays empty.
    let err = st.reconnect_now().await.unwrap_err();
    let display = format!("{err}");
    assert!(
        display.contains("slot empty"),
        "expected the defensive slot-empty error from attempt_reconnect, got: {display}"
    );
    // factory.open() ran once (attempt_reconnect always opens first)
    // before hitting the slot check.
    assert_eq!(cfg.opens(), 1);
}

#[tokio::test(start_paused = true)]
async fn stubborn_while_open_is_aborted_during_reconnect() {
    // attempt_reconnect's stubborn-task abort path (lines 404-409):
    // a while_open task that ignores its cancellation token gets the
    // bounded 5s join timeout treatment and is abort()-ed so the new
    // connection isn't shadowed by a zombie task hammering the dead
    // transport. Runs in paused time so the 5s wait is virtual.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let wo = WhileOpenHooks::default();
    let st = build_with_factory_and_hooks(factory, wo.stubborn_hooks());

    st.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(wo.started.load(Ordering::SeqCst));

    // reconnect_now waits the full WHILE_OPEN_TEARDOWN_TIMEOUT for
    // the stubborn task's join, then abort()s it. In paused time
    // this completes virtually instantly.
    st.reconnect_now().await.unwrap();
    assert_eq!(cfg.opens(), 2);
    assert!(st.is_available());
    assert!(!st.is_reconnecting());
    // Cooperative-exit flag must stay false — the stubborn task was
    // aborted, never given a chance to exit cleanly.
    assert!(!wo.exited.load(Ordering::SeqCst));

    st.shutdown().await.unwrap();
}
