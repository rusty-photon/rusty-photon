//! Sequential and nested acquires.
//!
//! Two sequential `acquire()`s share one open. First drop keeps the
//! transport open; second drop closes it. Re-acquiring after close
//! triggers a fresh open. Hooks fire exactly once per
//! connect/disconnect cycle.

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

mod common;

use std::sync::atomic::Ordering;

use common::{build_with_hooks, CountingHooks};

#[tokio::test]
async fn two_sequential_acquires_share_one_open() {
    let hooks = CountingHooks::default();
    let hs = hooks.handshake_calls.clone();
    let td = hooks.teardown_calls.clone();
    let (st, cfg) = build_with_hooks(hooks.hooks());

    let a = st.acquire().await.unwrap();
    let b = st.acquire().await.unwrap();
    assert_eq!(cfg.opens(), 1);
    assert_eq!(hs.load(Ordering::SeqCst), 1);

    // First close: refcount goes 2 → 1, transport stays open, teardown
    // does NOT run.
    a.close().await.unwrap();
    assert_eq!(td.load(Ordering::SeqCst), 0);
    assert!(st.is_available());
    assert_eq!(cfg.dropped_count().await, 0);

    // Second close: refcount goes 1 → 0, teardown runs, transport
    // closes.
    b.close().await.unwrap();
    assert_eq!(td.load(Ordering::SeqCst), 1);
    assert!(!st.is_available());
    assert_eq!(cfg.dropped_count().await, 1);
}

#[tokio::test]
async fn close_then_reacquire_runs_handshake_again() {
    let hooks = CountingHooks::default();
    let hs = hooks.handshake_calls.clone();
    let td = hooks.teardown_calls.clone();
    let (st, cfg) = build_with_hooks(hooks.hooks());

    {
        let session = st.acquire().await.unwrap();
        session.close().await.unwrap();
    }
    assert_eq!(hs.load(Ordering::SeqCst), 1);
    assert_eq!(td.load(Ordering::SeqCst), 1);
    assert_eq!(cfg.opens(), 1);

    {
        let session = st.acquire().await.unwrap();
        session.close().await.unwrap();
    }
    assert_eq!(hs.load(Ordering::SeqCst), 2);
    assert_eq!(td.load(Ordering::SeqCst), 2);
    assert_eq!(cfg.opens(), 2);
    // Both transports were dropped.
    assert_eq!(cfg.dropped_count().await, 2);
}

#[tokio::test]
async fn drop_path_decrements_count_and_closes_transport() {
    // Without an explicit close(), Drop's detached cleanup also brings
    // the count back to 0 and runs teardown — just on a spawned task,
    // not inline. The test pauses to let the spawned cleanup complete
    // before asserting.
    let hooks = CountingHooks::default();
    let td = hooks.teardown_calls.clone();
    let (st, cfg) = build_with_hooks(hooks.hooks());

    {
        let _session = st.acquire().await.unwrap();
    } // _session drops here; spawn() schedules cleanup

    common::yield_briefly().await;
    common::yield_briefly().await;

    assert_eq!(td.load(Ordering::SeqCst), 1);
    assert!(!st.is_available());
    assert_eq!(cfg.dropped_count().await, 1);
}

#[tokio::test]
async fn close_returns_immediately_when_not_last_session() {
    // Two sessions; close() on the first one should resolve quickly
    // without running teardown (the second session keeps the transport
    // open).
    let hooks = CountingHooks::default();
    let td = hooks.teardown_calls.clone();
    let (st, _cfg) = build_with_hooks(hooks.hooks());

    let a = st.acquire().await.unwrap();
    let b = st.acquire().await.unwrap();

    let before = std::time::Instant::now();
    a.close().await.unwrap();
    let elapsed = before.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(100),
        "non-last close should not block on teardown (took {elapsed:?})"
    );
    assert_eq!(td.load(Ordering::SeqCst), 0);
    drop(b);
}

#[tokio::test]
async fn is_available_reflects_lifecycle_phases() {
    let (st, _cfg) = build_with_hooks(common::CountingHooks::default().hooks());
    assert!(!st.is_available(), "fresh transport must not be available");

    let session = st.acquire().await.unwrap();
    assert!(st.is_available(), "open transport must be available");
    session.close().await.unwrap();
    assert!(!st.is_available(), "closed transport must not be available");
}
