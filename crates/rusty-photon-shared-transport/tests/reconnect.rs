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
    build_with_factory_and_hooks, handshake_panicking_on, handshake_tolerating_a_wire_failure,
    CountingHooks, CountingWhileOpenHooks, ExclusiveFactory, FactoryConfig, ProgrammableFactory,
    SafetyStopHooks, WhileOpenHooks,
};
use rusty_photon_shared_transport::SharedTransport;
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
    // attempt_reconnect's defensive "slot empty" arm: reconnect_now
    // flips reconnecting/available and calls attempt_reconnect
    // directly, which then sees an empty slot because start() never
    // populated it. The error is preferable to a panic in this
    // defensive path.
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
    // No open was attempted: the slot is read before the conduit is
    // opened, because the connection it names has to be closed first.
    assert_eq!(cfg.opens(), 0);
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

// ---------------------------------------------------------------------------
// Reconnecting onto an exclusive conduit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reconnect_releases_the_dead_conduit_before_opening_its_replacement() {
    // The reconnect supervisor's whole job is to re-open the *same*
    // port the dead transport was using. Where that port is exclusive
    // — a Windows COM port is — opening before dropping means the open
    // that would have released the old handle is the one that fails,
    // and no number of retries ever recovers: every attempt finds the
    // port held by the connection the failed attempt left in place.
    let (factory, ports) = ExclusiveFactory::new();
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(Arc::new(factory), counting.hooks());

    st.start().await.unwrap();
    assert_eq!(ports.opens(), 1);

    st.reconnect_now().await.unwrap();

    assert_eq!(
        ports.refusals(),
        0,
        "the replacement open must not be refused: the dead conduit is released first"
    );
    assert_eq!(ports.opens(), 2);
    assert!(st.is_available());
    assert!(!st.is_reconnecting());

    st.shutdown().await.unwrap();
    assert!(!ports.is_held());
}

#[tokio::test]
async fn a_session_held_across_a_reconnect_follows_the_new_conduit() {
    // Closing the old connection before the open is only safe if a
    // live session recovers on the other side of the swap. It does:
    // the session reads the cell, and the cell now holds the fresh
    // connection.
    let (factory, ports) = ExclusiveFactory::new();
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(Arc::new(factory), counting.hooks());

    st.start().await.unwrap();
    let session = st.acquire().await.unwrap();
    session.request(b"before".to_vec()).await.unwrap();

    st.reconnect_now().await.unwrap();

    let echoed = session.request(b"after".to_vec()).await.unwrap();
    assert_eq!(
        echoed, b"after",
        "the session must resume on the replacement conduit"
    );
    assert_eq!(ports.refusals(), 0);

    session.close().await.unwrap();
    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_reconnect_that_loses_its_slot_closes_the_replacement() {
    // `attempt_reconnect` does not hold `acquire_lock` — it cannot,
    // because `shutdown()` holds it while joining the supervisor this
    // runs on. So a teardown can take the slot while an attempt is in
    // flight. Publishing anyway would leave the replacement in a cell
    // nothing reads again, holding the port nothing will close.
    let (factory, ports) = ExclusiveFactory::gated();
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(Arc::new(factory), counting.hooks());

    st.start().await.unwrap();

    let reconnecting = {
        let st = Arc::clone(&st);
        tokio::spawn(async move { st.reconnect_now().await })
    };
    ports.wait_inside_open().await;

    // Tear the transport down while the attempt sits inside open().
    st.shutdown().await.unwrap();
    ports.release_open();

    let err = reconnecting.await.unwrap().unwrap_err();
    assert!(
        err.to_string().contains("torn down"),
        "expected the attempt to report the teardown, got: {err}"
    );
    assert!(
        !ports.is_held(),
        "the replacement must be closed, not orphaned in a cell off the slot"
    );
}

#[tokio::test]
async fn a_lazy_acquire_after_a_failed_reconnect_is_usable() {
    // In `ServiceLifetime` a failed attempt keeps `reconnecting` set
    // for the supervisor to clear on its next success. `LazyAcquire`
    // has no supervisor, so the flag would outlive the failure and
    // short-circuit every later request; the failure clears it on its
    // own way out, and the 0→1 open clears it again for the acquire
    // that races an attempt still in flight.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    // No start(), so the slot is empty and the attempt cannot succeed.
    st.reconnect_now().await.unwrap_err();
    assert!(
        !st.is_reconnecting(),
        "nothing retries a LazyAcquire attempt, so the flag must not promise one"
    );

    let session = st.acquire().await.unwrap();
    assert!(
        !st.is_reconnecting(),
        "the lazy open is the recovery; it must clear the flag"
    );
    let echoed = session.request(b"ping".to_vec()).await.unwrap();
    assert_eq!(echoed, b"ping");

    session.close().await.unwrap();
}

#[tokio::test]
async fn a_failed_lazy_reconnect_does_not_strand_a_live_session() {
    // The 0→1 clear only covers an attempt that failed with no client
    // attached. A session held across the failure keeps the refcount
    // above zero, so every later `acquire()` takes the fast path and
    // that clear never runs — and `LazyAcquire` has no supervisor to
    // run it either. The attempt has already closed the conduit, so
    // without the clear on the failure path both the held session and
    // every new one answer `Reconnecting` for the life of the process.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    let held = st.acquire().await.unwrap();
    assert_eq!(held.request(b"ping".to_vec()).await.unwrap(), b"ping");

    // Fail the replacement open with the session still alive.
    cfg.set_fail(true);
    st.reconnect_now().await.unwrap_err();
    cfg.set_fail(false);

    assert!(
        !st.is_reconnecting(),
        "a failed LazyAcquire attempt must not leave a retry promise nobody keeps"
    );

    // The conduit really is gone: the held session gets the honest
    // error, not an indefinite "try again".
    let display = format!("{}", held.request(b"ping".to_vec()).await.unwrap_err());
    assert!(
        display.contains("closed"),
        "a held session must see the closed conduit, got: {display}"
    );

    // Releasing it is the documented recovery: the next 0→1 opens a
    // fresh conduit.
    held.close().await.unwrap();
    let recovered = st.acquire().await.unwrap();
    assert_eq!(recovered.request(b"ping".to_vec()).await.unwrap(), b"ping");

    recovered.close().await.unwrap();
}

#[tokio::test]
async fn a_reconnect_with_no_client_re_asserts_the_last_disconnect_state() {
    // A 1→0 that lands during a reconnect runs its safety hook against
    // a connection that is dead or already closed, so every command
    // fails and nothing replays it. For the mount that hook is the
    // halt, so the replacement has to get it too.
    //
    // Counting the calls is not enough: a hook invoked on the old,
    // closed connection would count the same and still leave the mount
    // moving. `SafetyStopHooks` issues a real request and counts only
    // the ones that came back `Ok`, so the second invocation's success
    // is what says it landed on the fresh conduit.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let stops = SafetyStopHooks::default();
    let st = build_with_factory_and_hooks(factory, stops.hooks());

    st.start().await.unwrap();
    let session = st.acquire().await.unwrap();
    session.close().await.unwrap();
    assert_eq!(
        stops.calls.load(Ordering::SeqCst),
        1,
        "the 1→0 fires it once"
    );
    assert_eq!(
        stops.reached_the_wire.load(Ordering::SeqCst),
        1,
        "the 1→0 runs against a healthy conduit"
    );

    st.reconnect_now().await.unwrap();

    assert_eq!(
        stops.calls.load(Ordering::SeqCst),
        2,
        "the replacement conduit must carry the no-client state too"
    );
    assert_eq!(
        stops.reached_the_wire.load(Ordering::SeqCst),
        2,
        "the re-assert must run against the replacement, not the conduit the attempt closed"
    );

    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_reconnect_whose_safety_stop_did_not_land_is_not_a_recovery() {
    // The hook returns `()`, so a stop that failed on the wire is
    // invisible in its result. Reporting the attempt as a success
    // anyway clears `reconnecting` and sets `available`, and the very
    // next client can then acquire and drive a mount that is still
    // moving — the halt that was meant to stop it never reached the
    // device. The attempt has to fail instead, leaving clients
    // short-circuited until one whose stop lands.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let stops = SafetyStopHooks::failing_first(1, cfg.fail_recvs.clone());
    let st = build_with_factory_and_hooks(factory, stops.hooks());
    // A long interval keeps the supervisor out of the way: this is
    // about the state the failed attempt leaves behind.
    st.set_reconnect_interval(Duration::from_secs(3600)).await;

    st.start().await.unwrap();

    st.reconnect_now().await.unwrap_err();

    assert_eq!(
        stops.calls.load(Ordering::SeqCst),
        1,
        "the replay ran on the fresh conduit"
    );
    assert_eq!(
        stops.reached_the_wire.load(Ordering::SeqCst),
        0,
        "and did not land, which is the case under test"
    );
    assert!(
        st.is_reconnecting(),
        "a stop that did not land must leave the transport reconnecting"
    );
    assert!(
        !st.is_available(),
        "and must not advertise the conduit as recovered"
    );

    // A client can still attach — that is deliberate — but it cannot
    // command the mount whose halt is outstanding.
    let client = st.acquire().await.unwrap();
    let display = format!("{}", client.request(b"slew".to_vec()).await.unwrap_err());
    assert!(
        display.contains("reconnecting"),
        "a client must not reach a conduit whose safety stop is outstanding, got: {display}"
    );

    client.close().await.unwrap();
    st.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_replay_that_fails_on_the_wire_does_not_outrun_the_retry_cadence() {
    // The safety replay runs inside the attempt and goes out through
    // `Connection::request`, which signals the supervisor on a wire
    // failure. `Notify` keeps that permit, so the loop's next
    // `notified()` returns at once: without a floor between attempts a
    // stop command that keeps failing drives open/handshake/replay
    // cycles back to back, cycling the port as fast as it can be
    // opened. Three failing replays must therefore cost three
    // intervals, not none.
    const INTERVAL: Duration = Duration::from_millis(200);
    const FAILURES: u32 = 3;

    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let stops = SafetyStopHooks::failing_first(FAILURES, cfg.fail_recvs.clone());
    let st = build_with_factory_and_hooks(factory, stops.hooks());
    st.set_reconnect_interval(INTERVAL).await;

    st.start().await.unwrap();
    let opens_after_start = cfg.opens();

    // No client attached, so every attempt runs the replay — and an
    // attempt whose replay does not land reports the failure rather
    // than advertising a recovered transport.
    let started = tokio::time::Instant::now();
    st.reconnect_now().await.unwrap_err();

    // Let the supervisor work through the replays that keep failing.
    assert!(
        wait_until(
            || stops.calls.load(Ordering::SeqCst) > FAILURES,
            INTERVAL * 20
        )
        .await,
        "the supervisor must keep retrying past the failing replays"
    );
    let elapsed = started.elapsed();
    let attempts = cfg.opens().saturating_sub(opens_after_start);

    // `reconnect_now` runs the first failing replay itself and
    // stamps the same clock the supervisor's floor reads, so every
    // attempt after it — including the first retry, which the failed
    // replay signals for immediately — waits out an interval.
    // Without the floor the whole sequence finishes in no time at all.
    let floored = INTERVAL * FAILURES;
    assert!(
        elapsed >= floored,
        "{attempts} attempts in {elapsed:?} — every attempt must wait out the cadence (expected at least {floored:?})"
    );

    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_last_disconnect_that_could_not_land_is_owed_to_the_next_reconnect() {
    // The refcount is a snapshot, so it cannot record that a halt was
    // missed. A 1→0 during a reconnect runs against a conduit that is
    // dead or closed and every command fails; if a client then acquires
    // before the attempt reads the count, the replay is skipped, the
    // attempt looks clean — the failures were on the *old* connection —
    // and that client's first command reaches a mount that is still
    // moving. The obligation has to outlive the refcount.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let stops = SafetyStopHooks::failing_first(1, cfg.fail_recvs.clone());
    let st = build_with_factory_and_hooks(factory, stops.hooks());

    st.start().await.unwrap();

    // A 1→0 whose safety stop does not reach the device.
    let departing = st.acquire().await.unwrap();
    departing.close().await.unwrap();
    assert_eq!(stops.calls.load(Ordering::SeqCst), 1, "the 1→0 fired it");
    assert_eq!(
        stops.reached_the_wire.load(Ordering::SeqCst),
        0,
        "and it did not land, which is the case under test"
    );

    // Recording the debt has to take the transport out of service too.
    // Otherwise the window between the failed stop and the replay is
    // one where the transport still says it is healthy.
    assert!(
        st.is_reconnecting(),
        "a stop that did not land leaves the transport's safety state unknown"
    );
    assert!(!st.is_available(), "so it must not read as available");

    // A client arrives before the reconnect, so the refcount no longer
    // says "nobody attached" — and it cannot command the mount whose
    // halt is outstanding.
    let arriving = st.acquire().await.unwrap();
    let display = format!("{}", arriving.request(b"slew".to_vec()).await.unwrap_err());
    assert!(
        display.contains("reconnecting"),
        "no command may pass before the owed stop is replayed, got: {display}"
    );

    st.reconnect_now().await.unwrap();

    assert_eq!(
        stops.calls.load(Ordering::SeqCst),
        2,
        "the owed stop must be replayed even though a client is attached"
    );
    assert_eq!(
        stops.reached_the_wire.load(Ordering::SeqCst),
        1,
        "and it must land on the fresh conduit"
    );

    // Discharged: a second reconnect under the same client does not
    // replay it again.
    st.reconnect_now().await.unwrap();
    assert_eq!(
        stops.calls.load(Ordering::SeqCst),
        2,
        "a discharged obligation must not keep firing under a live client"
    );

    arriving.close().await.unwrap();
    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_owed_stop_does_not_cross_a_shutdown_into_the_next_lifecycle() {
    // The obligation belongs to the lifecycle that incurred it.
    // Carrying it past a shutdown would leave it outstanding while the
    // next `start()` publishes a fresh conduit and serves clients — and
    // then fire a stale halt at the first reconnect, under a live
    // client, which is exactly what the replay must never do.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let stops = SafetyStopHooks::failing_first(1, cfg.fail_recvs.clone());
    let st = build_with_factory_and_hooks(factory, stops.hooks());

    st.start().await.unwrap();

    // Incur the debt: a 1→0 whose stop does not reach the device. The
    // factory refuses from here so the supervisor cannot recover and
    // discharge it on its own — the debt has to still be outstanding
    // when the shutdown arrives, which is the case under test.
    let departing = st.acquire().await.unwrap();
    cfg.set_fail(true);
    departing.close().await.unwrap();
    assert_eq!(stops.calls.load(Ordering::SeqCst), 1);
    assert!(
        st.is_reconnecting(),
        "the failed stop took it out of service"
    );

    st.shutdown().await.unwrap();
    assert!(
        !st.is_available(),
        "the attempt the failed stop woke must not undo the shutdown that interrupted it"
    );

    cfg.set_fail(false);
    st.start().await.unwrap();

    // A fresh lifecycle with a client attached. The debt from the old
    // one must not be collected here.
    let client = st.acquire().await.unwrap();
    st.reconnect_now().await.unwrap();

    assert_eq!(
        stops.calls.load(Ordering::SeqCst),
        1,
        "an obligation from a finished lifecycle must not halt a live client's mount"
    );

    client.close().await.unwrap();
    st.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_recovery_during_the_cadence_wait_cancels_the_attempt_it_was_waiting_for() {
    // The state that sends the supervisor into the floor is read before
    // the sleep. A `reconnect_now()` landing during that sleep can
    // recover the transport, and attempting anyway would close the
    // connection that call just published and open another for nothing.
    //
    // Paused time makes the interleaving deterministic: the runtime
    // advances to the nearest deadline, so this test's own sleeps land
    // inside the supervisor's.
    const INTERVAL: Duration = Duration::from_secs(1);

    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());
    st.set_reconnect_interval(INTERVAL).await;

    st.start().await.unwrap();

    // Part-way through the supervisor's own wait, fail an attempt. That
    // stamps the cadence clock, so when the supervisor wakes it has to
    // wait out the remainder before it may try.
    tokio::time::sleep(INTERVAL * 6 / 10).await;
    cfg.set_fail(true);
    st.reconnect_now().await.unwrap_err();
    assert!(st.is_reconnecting());

    // The supervisor is now in the floor. Recover underneath it.
    tokio::time::sleep(INTERVAL * 6 / 10).await;
    cfg.set_fail(false);
    st.reconnect_now().await.unwrap();
    assert!(!st.is_reconnecting(), "the manual attempt recovered it");
    let opens_at_recovery = cfg.opens();

    // Its wait expires here. It must notice the recovery rather than
    // spend the attempt it was holding.
    tokio::time::sleep(INTERVAL * 3).await;

    assert_eq!(
        cfg.opens(),
        opens_at_recovery,
        "the supervisor must not re-open a transport recovered during its wait"
    );
    assert!(
        st.is_available(),
        "and must leave the recovered one in place"
    );

    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_lazy_open_discharges_a_stop_the_previous_conduit_could_not_carry() {
    // `LazyAcquire` has no supervisor, so the debt a failed stop leaves
    // can only be paid by the next 0→1 open. The handshake that open
    // runs is not a substitute — it is not the safety hook — so without
    // an explicit discharge the stop is lost inside a single lifecycle.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let stops = SafetyStopHooks::failing_first(1, cfg.fail_recvs.clone());
    let st = build_with_factory_and_hooks(factory, stops.hooks());

    // No `start()`: this is the lazy mode throughout.
    let first = st.acquire().await.unwrap();
    first.close().await.unwrap();
    assert_eq!(stops.calls.load(Ordering::SeqCst), 1, "the 1→0 fired it");
    assert_eq!(
        stops.reached_the_wire.load(Ordering::SeqCst),
        0,
        "and it did not land, which is the case under test"
    );

    // The next open is the only chance to pay it.
    let second = st.acquire().await.unwrap();
    assert_eq!(
        stops.calls.load(Ordering::SeqCst),
        2,
        "the owed stop must be replayed on the conduit this open produced"
    );
    assert_eq!(
        stops.reached_the_wire.load(Ordering::SeqCst),
        1,
        "and it must land there"
    );

    // Discharged: this client's own 1→0 is an ordinary one, and the
    // open after it replays nothing.
    second.close().await.unwrap();
    let third = st.acquire().await.unwrap();
    assert_eq!(
        stops.calls.load(Ordering::SeqCst),
        3,
        "the ordinary 1→0 fired once and the open added nothing"
    );

    third.close().await.unwrap();
}

#[tokio::test]
async fn a_lazy_open_whose_discharge_fails_hands_back_no_session() {
    // The debt is the reason the conduit is not safe to expose, so an
    // open that cannot pay it must not return a session — otherwise the
    // caller that triggered the open is handed the very transport whose
    // mount may still be moving.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    // Two failures: the 1→0 that incurs the debt, then the open that
    // first tries to pay it.
    let stops = SafetyStopHooks::failing_first(2, cfg.fail_recvs.clone());
    let st = build_with_factory_and_hooks(factory, stops.hooks());

    let first = st.acquire().await.unwrap();
    first.close().await.unwrap();
    assert_eq!(stops.reached_the_wire.load(Ordering::SeqCst), 0);

    let refused = st.acquire().await.unwrap_err();
    assert!(
        refused.to_string().contains("did not land"),
        "the acquire must report the undischarged stop, got: {refused}"
    );
    assert_eq!(
        stops.calls.load(Ordering::SeqCst),
        2,
        "the open tried to pay it"
    );

    // Still owed, so the next open tries again — and this time lands.
    let recovered = st.acquire().await.unwrap();
    assert_eq!(stops.calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        stops.reached_the_wire.load(Ordering::SeqCst),
        1,
        "the third attempt is the one that reaches the device"
    );
    assert_eq!(recovered.request(b"ping".to_vec()).await.unwrap(), b"ping");

    recovered.close().await.unwrap();
}

#[tokio::test]
async fn a_start_after_a_failed_lazy_cleanup_pays_the_debt_before_serving() {
    // A debt outlives the mode it was incurred in. A lazy 1→0 whose
    // stop did not land, followed by `start()`, would otherwise reach
    // the point of serving clients with the stop still outstanding:
    // the cold start opens and handshakes, and a handshake is not the
    // safety hook.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    // The 1→0 that incurs the debt, then the first start's attempt to
    // pay it, both fail.
    let stops = SafetyStopHooks::failing_first(2, cfg.fail_recvs.clone());
    let st = build_with_factory_and_hooks(factory, stops.hooks());

    let lazy = st.acquire().await.unwrap();
    lazy.close().await.unwrap();
    assert_eq!(stops.calls.load(Ordering::SeqCst), 1);
    assert_eq!(stops.reached_the_wire.load(Ordering::SeqCst), 0);

    // A start that cannot pay it must not come up.
    let refused = st.start().await.unwrap_err();
    assert!(
        refused.to_string().contains("did not land"),
        "the start must report the undischarged stop, got: {refused}"
    );
    assert!(
        !st.is_available(),
        "and must not have exposed the conduit it opened"
    );

    // The next start pays it and comes up.
    st.start().await.unwrap();
    assert_eq!(stops.calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        stops.reached_the_wire.load(Ordering::SeqCst),
        1,
        "the stop reached the device before any client could"
    );
    assert!(st.is_available());

    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_hook_that_panics_mid_attempt_does_not_take_the_supervisor_with_it() {
    // The attempt awaits the service's own hooks. A panic inside one
    // of their futures — not just in a closure that builds one — used
    // to unwind the supervisor, which is the only task that retries:
    // the transport was left saying a retry was coming with nothing
    // left to make one.
    const INTERVAL: Duration = Duration::from_millis(50);

    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    // Call 1 is `start()`. Call 2 is the supervisor's first retry.
    let st = build_with_factory_and_hooks(factory, handshake_panicking_on(2));
    st.set_reconnect_interval(INTERVAL).await;

    st.start().await.unwrap();

    // Put it into recovery with an attempt the factory refuses, so the
    // supervisor — not this task — runs the one that panics.
    cfg.set_fail(true);
    st.reconnect_now().await.unwrap_err();
    assert!(st.is_reconnecting());
    cfg.set_fail(false);

    assert!(
        wait_until(|| st.is_available(), INTERVAL * 100).await,
        "the supervisor must survive the panicking handshake and recover on a later tick"
    );

    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_manual_reconnect_that_panics_does_not_strand_a_lazy_session() {
    // `reconnect_now` sets `reconnecting` before the attempt and
    // answers for it afterwards. A panic in a service's hook skips
    // that answer, and in `LazyAcquire` — no supervisor to clear the
    // flag, a live session holding the refcount off zero so no 0→1
    // ever runs — every later request short-circuits on a retry nobody
    // is going to make.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    // Call 1 is the lazy open below; call 2 is the reconnect's.
    let st: Arc<SharedTransport<_>> =
        build_with_factory_and_hooks(factory, handshake_panicking_on(2));

    let held = st.acquire().await.unwrap();

    let attempting = Arc::clone(&st);
    tokio::spawn(async move { attempting.reconnect_now().await })
        .await
        .expect_err("the handshake panic must surface as a failed task");

    assert!(
        !st.is_reconnecting(),
        "nothing would ever clear this, so the panic must not leave it set"
    );

    // The session gets the honest terminal error rather than an
    // indefinite "try again".
    let display = format!("{}", held.request(b"ping".to_vec()).await.unwrap_err());
    assert!(
        display.contains("closed"),
        "the held session must see the closed conduit, got: {display}"
    );

    held.close().await.unwrap();
}

#[tokio::test]
async fn a_handshake_that_tolerates_a_failed_probe_does_not_re_trigger_recovery() {
    // A handshake may treat a probe as optional and ignore its error.
    // The request still fires the reconnect signal, and that permit
    // outlives the handshake — so the supervisor spends it the moment
    // the attempt reports success, putting the conduit it just
    // recovered straight back into recovery, and again every cadence
    // for as long as the probe keeps failing.
    const INTERVAL: Duration = Duration::from_millis(20);

    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let st = build_with_factory_and_hooks(
        factory,
        handshake_tolerating_a_wire_failure(cfg.fail_recvs.clone()),
    );
    st.set_reconnect_interval(INTERVAL).await;

    st.start().await.unwrap();
    st.reconnect_now().await.unwrap();
    let opens_after_recovery = cfg.opens();

    // Long enough for several cadences to have cycled the port.
    tokio::time::sleep(INTERVAL * 8).await;

    assert_eq!(
        cfg.opens(),
        opens_after_recovery,
        "a tolerated probe must not put the recovered conduit back into recovery"
    );
    assert!(st.is_available());
    assert!(!st.is_reconnecting());

    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_reconnect_under_a_live_client_leaves_the_hook_alone() {
    // The re-assert is for the no-client case only. A client is
    // attached here, so the state the hook asserts is not the state
    // the transport should be in, and firing it would halt a mount
    // mid-session.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let stops = SafetyStopHooks::default();
    let st = build_with_factory_and_hooks(factory, stops.hooks());

    st.start().await.unwrap();
    let session = st.acquire().await.unwrap();

    st.reconnect_now().await.unwrap();

    assert_eq!(
        stops.calls.load(Ordering::SeqCst),
        0,
        "a reconnect under a live client must not run the last-disconnect hook"
    );

    session.close().await.unwrap();
    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_client_arriving_during_the_re_assert_cannot_command_the_conduit() {
    // The refcount is read without `acquire_lock`, so a client can
    // acquire while the re-assert's stop commands are still going out.
    // What makes that safe is not the hook being best-effort: it is
    // that `reconnecting` stays set until the attempt returns, so the
    // session this client is handed cannot put anything on the wire
    // until the halt has already landed. A client inside this window
    // therefore cannot be mid-slew, because it has not been able to
    // command one.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    // The 1→0 below has to run straight through: parking it would
    // wedge the `close()` that triggers it. The reconnect's re-assert
    // is the second invocation, and that is the one to hold open.
    let stops = SafetyStopHooks::parking_after(1);
    let st = build_with_factory_and_hooks(factory, stops.hooks());

    st.start().await.unwrap();
    // Drop to zero clients so the reconnect below takes the re-assert
    // path.
    let session = st.acquire().await.unwrap();
    session.close().await.unwrap();

    let reconnecting = Arc::clone(&st);
    let attempt = tokio::spawn(async move { reconnecting.reconnect_now().await });

    // Park inside the re-assert, with the replacement already published.
    stops.wait_inside_hook().await;

    let racer = st
        .acquire()
        .await
        .expect("a first client during a reconnect is handed a session, not a shutdown error");
    let err = racer.request(b"slew".to_vec()).await.unwrap_err();
    let display = format!("{err}");
    assert!(
        display.contains("reconnecting"),
        "a client that arrives during the re-assert must not reach the wire, got: {display}"
    );

    stops.release_hook();
    attempt.await.unwrap().unwrap();

    // The racer's own 1→0 parks too; hand it its release up front.
    stops.release_hook();
    racer.close().await.unwrap();
    st.shutdown().await.unwrap();
}
