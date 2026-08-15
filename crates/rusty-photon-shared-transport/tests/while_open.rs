//! While-open task lifecycle.
//!
//! The poll-task teardown leak is the third bug class the plan calls
//! out (speculative on `main`, but the same copy-paste shape that
//! produced the race and the refcount leak). After this crate, the
//! while-open task is owned by `SharedTransport`: it is spawned on the
//! 0→1 transition, sees a cancellation signal on the 1→0 transition,
//! and is joined (with a bounded timeout + abort) before teardown runs.

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
use std::time::Duration;

use common::{build_with_hooks, yield_briefly, WhileOpenHooks};

#[tokio::test]
async fn while_open_starts_after_handshake_and_can_request() {
    let wo = WhileOpenHooks::default();
    let started = wo.started.clone();
    let (st, _cfg) = build_with_hooks(wo.cooperative_hooks());

    let session = st.acquire().await.unwrap();
    // The task may not have ticked yet — give it a moment.
    yield_briefly().await;
    assert!(
        started.load(Ordering::SeqCst),
        "while_open task should have started"
    );

    // Foreground request still works while the poll task is alive.
    let resp = session.request(b"hello".to_vec()).await.unwrap();
    assert_eq!(resp, b"hello");
    session.close().await.unwrap();
}

#[tokio::test]
async fn while_open_task_exits_when_last_session_closes() {
    let wo = WhileOpenHooks::default();
    let started = wo.started.clone();
    let exited = wo.exited.clone();
    let (st, _cfg) = build_with_hooks(wo.cooperative_hooks());

    let session = st.acquire().await.unwrap();
    yield_briefly().await;
    assert!(started.load(Ordering::SeqCst));
    assert!(!exited.load(Ordering::SeqCst));

    session.close().await.unwrap();
    // close() awaits the join inline, so by the time this returns the
    // task has either exited cooperatively or been aborted. The
    // cooperative task here exits cleanly.
    assert!(
        exited.load(Ordering::SeqCst),
        "while_open task should have exited"
    );
    assert!(!st.is_available());
}

#[tokio::test]
async fn while_open_task_persists_across_multiple_sessions() {
    // Two sessions sharing one transport: the while_open task spans
    // both. Dropping one session does not stop the task; dropping the
    // last one does.
    let wo = WhileOpenHooks::default();
    let exited = wo.exited.clone();
    let (st, _cfg) = build_with_hooks(wo.cooperative_hooks());

    let a = st.acquire().await.unwrap();
    let b = st.acquire().await.unwrap();
    yield_briefly().await;

    a.close().await.unwrap();
    assert!(
        !exited.load(Ordering::SeqCst),
        "while_open must keep running while b is alive"
    );
    b.close().await.unwrap();
    assert!(exited.load(Ordering::SeqCst));
}

#[tokio::test]
async fn while_open_task_respawns_after_full_close_reopen_cycle() {
    let wo = WhileOpenHooks::default();
    let started = wo.started.clone();
    let exited = wo.exited.clone();
    let (st, cfg) = build_with_hooks(wo.cooperative_hooks());

    {
        let session = st.acquire().await.unwrap();
        yield_briefly().await;
        assert!(started.load(Ordering::SeqCst));
        session.close().await.unwrap();
        assert!(exited.load(Ordering::SeqCst));
    }

    // Reset the started/exited flags so the second cycle's task is
    // distinguishable from the first.
    started.store(false, Ordering::SeqCst);
    exited.store(false, Ordering::SeqCst);

    let session = st.acquire().await.unwrap();
    yield_briefly().await;
    assert!(
        started.load(Ordering::SeqCst),
        "second while_open task should have started"
    );
    assert_eq!(
        cfg.opens(),
        2,
        "factory.open() should have been called twice"
    );
    session.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn stubborn_while_open_task_is_aborted_after_timeout() {
    // A while_open task that ignores cancellation gets `abort()`-ed
    // after the bounded 5s join timeout. The test runs in paused time
    // so the 5s wait completes in virtual time.
    let wo = WhileOpenHooks::default();
    let started = wo.started.clone();
    let exited = wo.exited.clone();
    let (st, _cfg) = build_with_hooks(wo.stubborn_hooks());

    let session = st.acquire().await.unwrap();
    // Yield in paused time so the spawned task gets a tick.
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(started.load(Ordering::SeqCst));

    // Close awaits the 5s join — in paused time this advances quickly.
    session.close().await.unwrap();
    // The stubborn task never set `exited` cooperatively; it was
    // aborted, not joined.
    assert!(!exited.load(Ordering::SeqCst));
    assert!(!st.is_available());

    // The transport is fully torn down — a fresh acquire opens cleanly.
    let next = st.acquire().await.unwrap();
    drop(next);
}
