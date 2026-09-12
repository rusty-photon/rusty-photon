//! Phase 0a: `SharedTransport::start` / `shutdown` and the
//! `LazyAcquire`-vs-`ServiceLifetime` mode split.
//!
//! These tests verify the two new lifecycle modes coexist correctly:
//!
//! - In `LazyAcquire` (default, no `start()` called), the port opens on
//!   the 0→1 `acquire()` and closes on the 1→0 `Session::close()`.
//!   `Hooks::on_last_disconnect` runs on 1→0; `Hooks::shutdown` is
//!   never invoked.
//! - In `ServiceLifetime` (after `start()`), the port opens at `start()`
//!   and stays open until `shutdown()`. `Hooks::on_last_disconnect` runs
//!   on every 1→0 and the port stays open. `Hooks::shutdown` runs once
//!   from `shutdown()`.
//!
//! Reconnect-supervisor tests land in Phase 0b alongside that machinery.

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
use std::time::Duration;

use common::{
    build_with_factory_and_hooks, handshake_tolerating_a_wire_failure,
    last_disconnect_panicking_on, shutdown_failing_on_the_wire, shutdown_panicking,
    while_open_constructor_panicking_on, yield_briefly, CountingHooks, ExclusiveFactory,
    FactoryConfig, ParkingHandshake, ProgrammableFactory, SafetyStopHooks, WhileOpenHooks,
};
use rusty_photon_shared_transport::TransportFactory;

// ---------------------------------------------------------------------------
// start()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn start_opens_transport_and_runs_handshake() {
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();

    assert_eq!(
        cfg.opens(),
        1,
        "start() must open the transport exactly once"
    );
    assert_eq!(
        counting.handshake_calls.load(Ordering::SeqCst),
        1,
        "start() must run the handshake exactly once"
    );
    assert!(st.is_available());
}

#[tokio::test]
async fn start_is_idempotent_when_already_started() {
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    st.start().await.unwrap();
    st.start().await.unwrap();

    assert_eq!(
        cfg.opens(),
        1,
        "repeated start() must not re-open the transport"
    );
    assert_eq!(
        counting.handshake_calls.load(Ordering::SeqCst),
        1,
        "repeated start() must not re-run handshake"
    );
}

#[tokio::test]
async fn start_promotes_an_already_lazy_opened_transport() {
    // A client called acquire() first (LazyAcquire 0→1 open); then the
    // service binary called start(). The flag flips to ServiceLifetime
    // without re-opening or re-handshaking.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    let session = st.acquire().await.unwrap();
    assert_eq!(cfg.opens(), 1);
    assert_eq!(counting.handshake_calls.load(Ordering::SeqCst), 1);

    st.start().await.unwrap();
    assert_eq!(
        cfg.opens(),
        1,
        "start() on an already-open transport must not re-open"
    );
    assert_eq!(counting.handshake_calls.load(Ordering::SeqCst), 1);

    // Now in ServiceLifetime mode: closing the session must not close
    // the port.
    session.close().await.unwrap();
    assert!(
        st.is_available(),
        "transport must stay open across 1→0 after start() promoted it"
    );
}

// ---------------------------------------------------------------------------
// acquire() in ServiceLifetime mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn acquire_after_start_is_fast_path_no_reopen() {
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    let _s1 = st.acquire().await.unwrap();
    let _s2 = st.acquire().await.unwrap();
    let _s3 = st.acquire().await.unwrap();

    assert_eq!(
        cfg.opens(),
        1,
        "acquire() in ServiceLifetime mode must reuse the slot connection"
    );
    assert_eq!(
        counting.handshake_calls.load(Ordering::SeqCst),
        1,
        "handshake belongs to start(); per-client acquire() must not re-run it"
    );
}

// ---------------------------------------------------------------------------
// Session::close() in ServiceLifetime mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn close_in_service_lifetime_runs_on_last_disconnect_and_keeps_port_open() {
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    let session = st.acquire().await.unwrap();
    session.close().await.unwrap();

    assert_eq!(
        counting.teardown_calls.load(Ordering::SeqCst),
        1,
        "on_last_disconnect must fire on the 1→0 transition"
    );
    assert_eq!(
        counting.shutdown_calls.load(Ordering::SeqCst),
        0,
        "shutdown must NOT fire on a client disconnect — only from SharedTransport::shutdown()"
    );
    assert!(
        st.is_available(),
        "port must stay open across 1→0 in ServiceLifetime mode"
    );
    assert_eq!(
        cfg.dropped_count().await,
        0,
        "FrameTransport must not have dropped"
    );
}

#[tokio::test]
async fn close_in_service_lifetime_fires_on_last_disconnect_each_cycle() {
    // Three full connect/disconnect cycles → on_last_disconnect fires
    // three times; the port stays open across all of them.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    for _ in 0..3 {
        let s = st.acquire().await.unwrap();
        s.close().await.unwrap();
    }

    assert_eq!(cfg.opens(), 1);
    assert_eq!(counting.handshake_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        counting.teardown_calls.load(Ordering::SeqCst),
        3,
        "on_last_disconnect must fire on every 1→0 transition"
    );
    assert_eq!(counting.shutdown_calls.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// shutdown()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_runs_shutdown_hook_and_closes_port() {
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    st.shutdown().await.unwrap();

    assert_eq!(
        counting.shutdown_calls.load(Ordering::SeqCst),
        1,
        "shutdown hook must fire exactly once"
    );
    assert_eq!(
        counting.teardown_calls.load(Ordering::SeqCst),
        0,
        "on_last_disconnect must NOT fire from shutdown() — it's only for client refcount 1→0"
    );
    assert!(!st.is_available());
    assert_eq!(
        cfg.dropped_count().await,
        1,
        "FrameTransport must drop when shutdown closes the port"
    );
}

#[tokio::test]
async fn shutdown_is_noop_in_lazy_acquire_mode() {
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    // No start() called — LazyAcquire mode.
    st.shutdown().await.unwrap();
    assert_eq!(
        counting.shutdown_calls.load(Ordering::SeqCst),
        0,
        "shutdown is a no-op when service_lifetime was never set"
    );

    // LazyAcquire still works normally afterwards.
    let s = st.acquire().await.unwrap();
    s.close().await.unwrap();
    assert_eq!(cfg.opens(), 1);
    assert_eq!(
        counting.teardown_calls.load(Ordering::SeqCst),
        1,
        "LazyAcquire 1→0 still runs on_last_disconnect after a no-op shutdown call"
    );
}

#[tokio::test]
async fn acquire_after_shutdown_returns_shut_down_error() {
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    st.shutdown().await.unwrap();

    let err = st.acquire().await.unwrap_err();
    let display = format!("{err}");
    assert!(
        display.contains("shut down"),
        "expected shut-down error, got: {display}"
    );
}

#[tokio::test]
async fn start_after_shutdown_reopens_cleanly() {
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    st.shutdown().await.unwrap();

    // Second cycle: full cold-start again.
    st.start().await.unwrap();
    let s = st.acquire().await.unwrap();
    s.close().await.unwrap();
    st.shutdown().await.unwrap();

    assert_eq!(
        cfg.opens(),
        2,
        "each start() after a shutdown() must re-open the port"
    );
    assert_eq!(
        counting.handshake_calls.load(Ordering::SeqCst),
        2,
        "each start() must re-run handshake"
    );
    assert_eq!(counting.shutdown_calls.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------------------
// LazyAcquire mode preservation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lazy_acquire_close_still_closes_port_and_does_not_call_shutdown() {
    // No start() / shutdown() calls at all — verifies the pre-Phase-0a
    // behavior is preserved for unmigrated services.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    let s = st.acquire().await.unwrap();
    s.close().await.unwrap();

    assert_eq!(
        counting.teardown_calls.load(Ordering::SeqCst),
        1,
        "LazyAcquire 1→0 runs on_last_disconnect"
    );
    assert_eq!(
        counting.shutdown_calls.load(Ordering::SeqCst),
        0,
        "shutdown hook is never invoked in LazyAcquire mode"
    );
    assert!(!st.is_available());
    assert_eq!(
        cfg.dropped_count().await,
        1,
        "LazyAcquire 1→0 still drops the FrameTransport"
    );
}

// ---------------------------------------------------------------------------
// while_open task lifecycle under ServiceLifetime
// ---------------------------------------------------------------------------

#[tokio::test]
async fn while_open_task_survives_client_disconnect_in_service_lifetime() {
    // The while_open task is tied to transport-open, not client refcount.
    // In ServiceLifetime mode it keeps running through a full
    // connect/disconnect cycle.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let wo = WhileOpenHooks::default();
    let st = build_with_factory_and_hooks(factory, wo.cooperative_hooks());

    st.start().await.unwrap();
    common::yield_briefly().await;
    assert!(
        wo.started.load(Ordering::SeqCst),
        "while_open should start as soon as start() returns"
    );

    let s = st.acquire().await.unwrap();
    s.close().await.unwrap();
    common::yield_briefly().await;
    assert!(
        !wo.exited.load(Ordering::SeqCst),
        "while_open must NOT exit on client 1→0 in ServiceLifetime mode"
    );

    st.shutdown().await.unwrap();
    common::yield_briefly().await;
    assert!(
        wo.exited.load(Ordering::SeqCst),
        "while_open exits on shutdown()'s cancellation"
    );
}

// ---------------------------------------------------------------------------
// while_open task fault handling at teardown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lazy_close_with_panicking_while_open_completes_cleanly() {
    // A while_open task that panics surfaces as `Ok(Err(join_err))`
    // when run_cleanup_locked awaits its JoinHandle. The teardown
    // path must log and continue rather than propagate or wedge.
    // Covers the LazyAcquire variant of that arm
    // (`run_cleanup_locked`, lines 745-749).
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let wo = WhileOpenHooks::default();
    let st = build_with_factory_and_hooks(factory, wo.panicking_hooks());

    // tokio::spawn isolates any panic-propagation surprise to the spawned
    // task so the test process itself doesn't unwind. The acquire +
    // close flow itself must not panic.
    let st_for_task = st.clone();
    let result = tokio::spawn(async move {
        let s = st_for_task.acquire().await.unwrap();
        common::yield_briefly().await;
        s.close().await
    })
    .await
    .unwrap();
    result.unwrap();

    assert!(wo.started.load(Ordering::SeqCst));
    assert!(!st.is_available(), "LazyAcquire 1→0 closes the port");
    assert_eq!(
        cfg.dropped_count().await,
        1,
        "transport must drop even though while_open panicked"
    );
}

#[tokio::test]
async fn shutdown_with_panicking_while_open_completes_cleanly() {
    // ServiceLifetime variant of the panic-handling arm in shutdown()
    // (lines 506-510). shutdown() awaits the while_open JoinHandle,
    // observes the panic, logs, and proceeds with hook + drop.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let wo = WhileOpenHooks::default();
    let st = build_with_factory_and_hooks(factory, wo.panicking_hooks());

    let st_for_task = st.clone();
    let result = tokio::spawn(async move {
        st_for_task.start().await.unwrap();
        common::yield_briefly().await;
        st_for_task.shutdown().await
    })
    .await
    .unwrap();
    result.unwrap();

    assert!(wo.started.load(Ordering::SeqCst));
    assert!(!st.is_available());
    assert_eq!(
        cfg.dropped_count().await,
        1,
        "shutdown must still close the port after a panicking while_open task"
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_with_stubborn_while_open_aborts_after_timeout() {
    // Cover the timeout/abort arm of shutdown()'s while_open join
    // (lines 511-518). A stubborn task ignores cancellation; shutdown
    // waits WHILE_OPEN_TEARDOWN_TIMEOUT (5s, virtualised) then
    // abort()s it. The hook + drop path still runs.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let wo = WhileOpenHooks::default();
    let st = build_with_factory_and_hooks(factory, wo.stubborn_hooks());

    st.start().await.unwrap();
    // Tick once in paused time so the spawned task gets a chance to
    // mark itself started before shutdown asks it to stop.
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    assert!(wo.started.load(Ordering::SeqCst));

    st.shutdown().await.unwrap();
    // exited never flips — the stubborn task was aborted, not joined.
    assert!(!wo.exited.load(Ordering::SeqCst));
    assert!(!st.is_available());
    assert_eq!(
        cfg.dropped_count().await,
        1,
        "shutdown must still drop the transport after abort()ing while_open"
    );
}

// ---------------------------------------------------------------------------
// Releasing an exclusive conduit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_releases_the_conduit_while_a_session_is_still_alive() {
    // A device that never called `Session::close` still holds a clone
    // of the connection cell when the service shuts down. Its requests
    // already refuse, but its *reference* does not, so leaving the
    // conduit to the last `Arc` drop leaves the port open — and the
    // rebuilt server's open then fails on any OS that holds a serial
    // port exclusively.
    let (factory, ports) = ExclusiveFactory::new();
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(std::sync::Arc::new(factory), counting.hooks());

    st.start().await.unwrap();
    let session = st.acquire().await.unwrap();

    st.shutdown().await.unwrap();
    assert!(
        !ports.is_held(),
        "shutdown must release the conduit even while a session is alive"
    );

    // What the reload loop does next: open the same port again.
    st.start().await.unwrap();
    assert_eq!(
        ports.refusals(),
        0,
        "the re-open must not be refused: the previous handle is gone"
    );

    drop(session);
    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_is_not_undone_by_the_attempt_it_interrupts() {
    // `shutdown()` clears `available` and then *joins* the supervisor,
    // so an attempt already in flight runs to completion inside that
    // join — and a successful one ends by setting `available = true`,
    // after the store. A later `start()` then sees an available
    // transport and promotes in place instead of cold-starting, leaving
    // the slot this shutdown emptied, and every `acquire()` failing on
    // an empty slot for the rest of the process.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    // First hook call fails on the wire, which owes a replay; the
    // replay that answers it parks, so an attempt is in flight when the
    // shutdown below arrives.
    let stops = std::sync::Arc::new(
        SafetyStopHooks::failing_first(1, cfg.fail_recvs.clone()).parking_from(1),
    );
    let st = build_with_factory_and_hooks(factory, stops.hooks());

    st.start().await.unwrap();
    let departing = st.acquire().await.unwrap();
    departing.close().await.unwrap();

    // The supervisor takes the owed stop and parks inside the replay.
    stops.wait_inside_hook().await;

    let shutting_down = std::sync::Arc::clone(&st);
    let shutdown = tokio::spawn(async move { shutting_down.shutdown().await });

    // Let it reach the supervisor join, then let the attempt finish
    // underneath it.
    yield_briefly().await;
    stops.release_hook();
    shutdown.await.unwrap().unwrap();

    assert!(
        !st.is_available(),
        "an attempt completing inside the shutdown join must not re-advertise the transport"
    );

    // The proof that it matters: the next start has to cold-start and
    // repopulate the slot, not promote an empty one.
    st.start().await.unwrap();
    let client = st.acquire().await.unwrap();
    assert_eq!(client.request(b"ping".to_vec()).await.unwrap(), b"ping");

    // That client's own 1→0 parks like any invocation past the first.
    stops.release_hook();
    client.close().await.unwrap();
    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_panicking_poll_constructor_does_not_strand_the_replacement_port() {
    // The `while_open` closure is user-supplied and can panic. The lazy
    // 0→1 path builds its future before publishing for exactly that
    // reason; the reconnect path has to do the same, or a panic leaves
    // the replacement installed in the slot with no poll task, nothing
    // to retry — the supervisor is what unwound — and the port held for
    // the life of the process.
    let (factory, ports) = ExclusiveFactory::new();
    let st = build_with_factory_and_hooks(
        std::sync::Arc::new(factory),
        while_open_constructor_panicking_on(2),
    );

    st.start().await.unwrap();
    assert!(ports.is_held(), "the first conduit is open");

    // The reconnect's respawn is the second constructor call.
    // The panic is caught and reported as a failed attempt rather than
    // unwinding the caller: on the supervisor that caller is the only
    // task that retries, and killing it leaves `reconnecting` set with
    // nothing left to clear it.
    let err = st.reconnect_now().await.unwrap_err();
    assert!(
        err.to_string().contains("while_open constructor panicked"),
        "expected the attempt to report the panic, got: {err}"
    );

    assert!(
        !ports.is_held(),
        "the replacement must be released, not left installed with nothing watching it"
    );

    // And the transport is still usable: a failed attempt is a state
    // the retry path knows how to leave, where an unwound supervisor is
    // not.
    st.reconnect_now().await.unwrap();
    assert!(st.is_available());
    let client = st.acquire().await.unwrap();
    assert_eq!(client.request(b"ping".to_vec()).await.unwrap(), b"ping");

    client.close().await.unwrap();
    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_shutdown_hook_failing_on_the_wire_does_not_disturb_the_next_start() {
    // `Hooks::shutdown` runs after the supervisor has been joined, so
    // a wire error in it fires the reconnect signal with nobody
    // waiting and the permit outlives the lifecycle. The next
    // `start()` installs a supervisor that consumes it immediately,
    // marks the transport it has just opened as reconnecting, and
    // tears that conduit down to open another.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let st = build_with_factory_and_hooks(
        factory,
        shutdown_failing_on_the_wire(cfg.fail_recvs.clone()),
    );
    st.set_reconnect_interval(Duration::from_millis(20)).await;

    st.start().await.unwrap();
    st.shutdown().await.unwrap();

    st.start().await.unwrap();
    let opens_after_start = cfg.opens();

    // Give a supervisor acting on a stale permit time to do it.
    yield_briefly().await;
    tokio::time::sleep(Duration::from_millis(120)).await;

    assert_eq!(
        cfg.opens(),
        opens_after_start,
        "a notification from the previous lifecycle must not cycle the conduit this start opened"
    );
    assert!(st.is_available(), "and must not leave it unavailable");
    assert!(!st.is_reconnecting());

    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_does_not_leave_a_reconnect_attempt_running() {
    // The supervisor runs each attempt as a task so a panicking hook
    // cannot unwind it. Dropping a `JoinHandle` does not stop the task
    // though, so a slow attempt — a hook that blocks, a factory that
    // does — would go on running after `shutdown()` aborted the
    // supervisor waiting on it, still holding the conduit it had just
    // opened and able to publish into a lifecycle that is gone.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    // The attempt parks in its *handshake*, before the replacement is
    // published. That is the case only the attempt can release:
    // `shutdown()` closes whatever is in the cell, so a published one
    // would be let go whether or not the task is still running.
    let handshakes = std::sync::Arc::new(ParkingHandshake::after(1));
    let st = build_with_factory_and_hooks(factory, handshakes.hooks());
    st.set_reconnect_interval(Duration::from_millis(20)).await;

    st.start().await.unwrap();

    // Put the transport into recovery with an attempt the factory
    // refuses, so the *supervisor* owns the one that parks.
    cfg.set_fail(true);
    st.reconnect_now().await.unwrap_err();
    cfg.set_fail(false);

    // Wait until the supervisor's attempt is parked in the handshake,
    // holding a conduit nothing else has a reference to.
    handshakes.wait_inside_handshake().await;

    st.shutdown().await.unwrap();

    // Against the number of transports actually handed out, not the
    // open count: a refused open bumps the counter without producing
    // one.
    let handed_out = cfg.drop_flags.lock().await.len();
    assert_eq!(
        cfg.dropped_count().await,
        handed_out,
        "every conduit the factory handed out must be gone once shutdown returns"
    );

    // Nothing should be waiting on this; if the abort worked it is a
    // permit nobody collects.
    handshakes.release_handshake();
}

#[tokio::test]
async fn a_start_after_an_undischarged_stop_releases_the_conduit_it_left_open() {
    // A `ServiceLifetime` 1→0 whose safety stop does not land keeps
    // the conduit open on purpose and only marks the transport
    // unavailable — which is exactly the state `start()` reads as
    // "cold open". Opening there asks the factory for a port this
    // process is still holding, and a serial port answers
    // `Access is denied`.
    let (factory, ports) = ExclusiveFactory::new();
    let stops = SafetyStopHooks::failing_first(1, ports.fail_recvs());
    let st = build_with_factory_and_hooks(std::sync::Arc::new(factory), stops.hooks());
    // The failed stop signals the supervisor, whose cadence clock is
    // unset after `start()`, so its first retry would be immediate.
    // This interval plus the exclusive factory refusing it — the
    // conduit is still held — leaves the clock stamped and the
    // supervisor out of the way for the rest of the test.
    st.set_reconnect_interval(Duration::from_secs(3600)).await;

    st.start().await.unwrap();
    let departing = st.acquire().await.unwrap();
    departing.close().await.unwrap();

    assert_eq!(stops.reached_the_wire.load(Ordering::SeqCst), 0);
    assert!(!st.is_available(), "the failed stop took it out of service");
    assert!(ports.is_held(), "and left the conduit open");

    st.start().await.unwrap();

    assert_eq!(
        ports.refusals(),
        0,
        "the open must release the conduit it inherited rather than ask for the port twice"
    );
    // Releasing the port is half of it. The debt the failed stop left
    // is the other half, and an open that published a fresh conduit
    // without replaying would pass every assertion above.
    assert_eq!(
        stops.calls.load(Ordering::SeqCst),
        2,
        "the start must also pay the stop the previous cleanup could not"
    );
    assert_eq!(
        stops.reached_the_wire.load(Ordering::SeqCst),
        1,
        "and it must land on the conduit this start opened"
    );
    assert!(st.is_available());

    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_lazy_open_after_a_panicking_cleanup_releases_the_conduit_it_left_open() {
    // The cleanup awaits `on_last_disconnect` inline, so a panic there
    // unwinds it before the close that would release the conduit and
    // before the slot is emptied. In `LazyAcquire` the next 0→1 is
    // what opens, and it would open alongside a port still held.
    let (factory, ports) = ExclusiveFactory::new();
    let (hooks, stops) = last_disconnect_panicking_on(1);
    let st = build_with_factory_and_hooks(std::sync::Arc::new(factory), hooks);

    // No `start()`: lazy throughout.
    let departing = st.acquire().await.unwrap();
    let closing = tokio::spawn(async move { departing.close().await });
    closing
        .await
        .expect_err("the hook panic must surface as a failed task");
    assert!(ports.is_held(), "the conduit really is still held");

    let reopened = st.acquire().await.unwrap();
    assert_eq!(
        ports.refusals(),
        0,
        "the open must release the conduit it inherited rather than ask for the port twice"
    );
    assert_eq!(reopened.request(b"ping".to_vec()).await.unwrap(), b"ping");

    // And the stop the panicking hook never delivered is owed, so that
    // open replayed it. A hook that does not return is no more
    // evidence the state landed than one whose commands failed.
    assert_eq!(
        stops.load(Ordering::SeqCst),
        2,
        "the open must replay a stop whose hook never returned"
    );

    reopened.close().await.unwrap();
}

#[tokio::test]
async fn a_cleanup_hook_that_panics_takes_the_transport_out_of_service() {
    // The bookkeeping that decides whether the stop landed runs after
    // the hook returns. One that panics never reaches it, and leaving
    // `available` true there lets the next client command a mount
    // whose halt did not complete.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let (hooks, _stops) = last_disconnect_panicking_on(1);
    let st = build_with_factory_and_hooks(factory, hooks);

    st.start().await.unwrap();
    let departing = st.acquire().await.unwrap();

    let closing = tokio::spawn(async move { departing.close().await });
    closing
        .await
        .expect_err("the hook panic must surface as a failed task");

    assert!(
        !st.is_available(),
        "a stop that did not complete leaves the transport's safety state unknown"
    );
    assert!(
        st.is_reconnecting(),
        "and the supervisor has to be told to go and re-assert it"
    );
}

#[tokio::test]
async fn a_second_client_after_shutdown_is_told_the_service_is_going_down() {
    // `shutdown()` does not force-close live sessions, so a client
    // arriving after it finds a refcount above zero and an empty slot.
    // The terminal check used to be gated on that refcount being zero,
    // which sent this caller to the reuse path and the defensive
    // "refcount > 0 but slot empty" error — a message about a bug in
    // this crate, for a service that is simply going down.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    let outliving = st.acquire().await.unwrap();
    st.shutdown().await.unwrap();

    let refused = st.acquire().await.unwrap_err();
    assert!(
        refused.to_string().contains("shut down"),
        "a client arriving after shutdown must be told that, got: {refused}"
    );

    drop(outliving);
}

#[tokio::test]
async fn a_cold_start_stops_the_supervisor_before_opening() {
    // `start()`'s cold path also runs with `service_lifetime` true and
    // `available` false, which is what a failed last-disconnect stop
    // leaves — and there the supervisor is alive and recovering. Parked
    // in its handshake it is holding the port, so a cold open that does
    // not stop it first asks the factory for a port this process has.
    let (factory, ports) = ExclusiveFactory::new();
    let handshakes =
        std::sync::Arc::new(ParkingHandshake::after(1).with_a_failing_stop(ports.fail_recvs()));
    let st = build_with_factory_and_hooks(std::sync::Arc::new(factory), handshakes.hooks());
    st.set_reconnect_interval(Duration::from_millis(5)).await;

    st.start().await.unwrap();
    let departing = st.acquire().await.unwrap();
    departing.close().await.unwrap();
    assert!(st.is_reconnecting(), "the failed stop put it into recovery");

    // The supervisor's attempt opens, then parks in the handshake
    // holding the conduit it just opened.
    handshakes.wait_inside_handshake().await;
    assert!(ports.is_held());

    let starting = std::sync::Arc::clone(&st);
    let start = tokio::spawn(async move { starting.start().await });

    // Let it reach the supervisor it has to stop, then let that
    // supervisor's attempt finish so the join returns promptly.
    yield_briefly().await;
    handshakes.release_handshake();

    start.await.unwrap().unwrap();

    assert_eq!(
        ports.refusals(),
        0,
        "the cold open must not ask for a port the supervisor is holding"
    );
    assert!(st.is_available());

    handshakes.release_handshake();
    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_start_that_fails_after_taking_the_supervisor_leaves_no_false_promise() {
    // The cold open cancels the supervisor before asking for the
    // conduit. If the open then fails, `reconnecting` would be left
    // saying a retry is coming with nothing alive to make one — and
    // every later acquire would answer with the defensive empty-slot
    // error rather than saying the transport is not serving.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let stops = SafetyStopHooks::failing_first(1, cfg.fail_recvs.clone());
    let st = build_with_factory_and_hooks(factory, stops.hooks());
    // Long enough that the supervisor is not the one recovering here.
    st.set_reconnect_interval(Duration::from_secs(3600)).await;

    st.start().await.unwrap();
    let departing = st.acquire().await.unwrap();
    departing.close().await.unwrap();
    assert!(st.is_reconnecting(), "the failed stop put it into recovery");

    cfg.set_fail(true);
    st.start().await.unwrap_err();

    assert!(
        !st.is_reconnecting(),
        "nothing is left to retry, so the flag must not promise one"
    );

    let refused = st.acquire().await.unwrap_err();
    assert!(
        refused.to_string().contains("shut down"),
        "a client must be told the transport is not serving, got: {refused}"
    );
}

#[tokio::test]
async fn a_tolerated_probe_at_start_does_not_wake_the_supervisor_it_spawns() {
    // The reconnect drains the permit a tolerated probe leaves, but the
    // cold start handshakes too — and spawns the supervisor moments
    // later, which spends that permit on the conduit the handshake has
    // just accepted.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let st = build_with_factory_and_hooks(
        factory,
        handshake_tolerating_a_wire_failure(cfg.fail_recvs.clone()),
    );
    st.set_reconnect_interval(Duration::from_millis(20)).await;

    st.start().await.unwrap();
    let opens_after_start = cfg.opens();

    tokio::time::sleep(Duration::from_millis(120)).await;

    assert_eq!(
        cfg.opens(),
        opens_after_start,
        "the supervisor must not tear down the conduit its own handshake accepted"
    );
    assert!(st.is_available());
    assert!(!st.is_reconnecting());

    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_tolerated_probe_on_a_lazy_open_does_not_outlive_it() {
    // `LazyAcquire` has no supervisor to spend the permit, so it simply
    // waits — until a `start()` promotes this live slot and spawns one,
    // which then tears down a conduit that has been healthy all along.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let st = build_with_factory_and_hooks(
        factory,
        handshake_tolerating_a_wire_failure(cfg.fail_recvs.clone()),
    );
    st.set_reconnect_interval(Duration::from_millis(20)).await;

    // Lazy open first, then promote it.
    let session = st.acquire().await.unwrap();
    st.start().await.unwrap();
    let opens_after_promotion = cfg.opens();

    tokio::time::sleep(Duration::from_millis(120)).await;

    assert_eq!(
        cfg.opens(),
        opens_after_promotion,
        "a permit from the lazy handshake must not reach the promoted supervisor"
    );
    assert!(st.is_available());

    session.close().await.unwrap();
    st.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_panicking_shutdown_hook_does_not_keep_the_port_from_the_next_start() {
    // The teardown awaits `Hooks::shutdown` inline, so a panic there
    // unwinds past the explicit close. A live `Session` keeps its own
    // `Arc` to the conduit, so the port stays held — and if the slot
    // had already been emptied, no later open could find it to close.
    // That is the reload meeting `Access is denied`, from inside the
    // teardown meant to prevent it.
    let (factory, ports) = ExclusiveFactory::new();
    let st = build_with_factory_and_hooks(std::sync::Arc::new(factory), shutdown_panicking());

    st.start().await.unwrap();
    let outliving = st.acquire().await.unwrap();

    let tearing_down = std::sync::Arc::clone(&st);
    tokio::spawn(async move { tearing_down.shutdown().await })
        .await
        .expect_err("the hook panic must surface as a failed task");

    assert!(ports.is_held(), "the conduit really is still held");

    // The session is still alive, so nothing else will release it. The
    // next start has to.
    st.start().await.unwrap();
    assert_eq!(
        ports.refusals(),
        0,
        "the open must find and release the conduit the panicking teardown left"
    );

    drop(outliving);
}

#[tokio::test]
async fn a_reconnect_after_shutdown_does_not_wedge_the_transport() {
    // `shutdown()` leaves `service_lifetime` true on purpose, so a
    // `reconnect_now()` afterwards used to read as "a ServiceLifetime
    // transport whose supervisor will retry" — when the supervisor is
    // exactly what the shutdown took away. The flag stayed set with
    // nobody to clear it: live sessions short-circuit forever, and the
    // next acquire skips the terminal check and reports the defensive
    // empty-slot error instead of saying the service is going down.
    let cfg = FactoryConfig::default();
    let factory: std::sync::Arc<dyn TransportFactory> =
        std::sync::Arc::new(ProgrammableFactory::new(cfg.clone()));
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(factory, counting.hooks());

    st.start().await.unwrap();
    st.shutdown().await.unwrap();

    st.reconnect_now().await.unwrap_err();
    assert!(
        !st.is_reconnecting(),
        "the shutdown took the retrier, so nothing must be left promising a retry"
    );

    let refused = st.acquire().await.unwrap_err();
    assert!(
        refused.to_string().contains("shut down"),
        "a client must be told the transport is not serving, got: {refused}"
    );
}

#[tokio::test]
async fn a_session_that_outlives_shutdown_cannot_reach_the_closed_conduit() {
    // The flip side of closing the conduit out from under a live
    // session: the session must report the closure, not panic and not
    // reach hardware.
    let (factory, _ports) = ExclusiveFactory::new();
    let counting = CountingHooks::default();
    let st = build_with_factory_and_hooks(std::sync::Arc::new(factory), counting.hooks());

    st.start().await.unwrap();
    let session = st.acquire().await.unwrap();
    st.shutdown().await.unwrap();

    let err = session.request(b"ping".to_vec()).await.unwrap_err();
    assert!(
        err.to_string().contains("shut down"),
        "expected the shut-down error, got: {err}"
    );

    // Close rather than drop: `Session::drop` spawns the cleanup
    // detached, so letting it fall off the end here would leave the
    // teardown running against a runtime already going away.
    session.close().await.unwrap();
}
