//! Issue #258 dissolves here: when the 0→1 transition fails — at the
//! factory step, the handshake step, or a panic in either — the
//! transport rolls back fully. The refcount returns to 0,
//! `is_available()` stays false, and the next `acquire()` runs the full
//! open path again.
//!
//! In the per-service code on `main` (before PR #260), a handshake
//! error in `qhy-focuser` would leave the manager wedged because the
//! refcount was bumped before the handshake ran and never rolled back.
//! This test crate proves the new structure can't reproduce that bug.

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

use std::sync::Arc;

use common::{
    build_with_factory_and_hooks, build_with_hooks, failing_handshake_hooks,
    panicking_handshake_hooks, panicking_while_open_constructor_hooks, EchoCodec, FactoryConfig,
    ProgrammableFactory,
};
use rusty_photon_shared_transport::{
    Hooks, SessionError, SharedTransport, TransportError, TransportFactory,
};

#[tokio::test]
async fn factory_open_error_rolls_back_refcount() {
    let cfg = FactoryConfig::failing();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let st = SharedTransport::new(factory, EchoCodec, Hooks::noop());

    let err = st.acquire().await.unwrap_err();
    assert!(matches!(err, SessionError::Transport(TransportError::Eof)));
    assert!(!st.is_available());
    assert_eq!(cfg.opens(), 1);
}

#[tokio::test]
async fn factory_open_error_does_not_block_subsequent_acquire() {
    // The next attempt against a recovered factory must succeed cleanly.
    let cfg = FactoryConfig::failing();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));
    let st = SharedTransport::new(factory, EchoCodec, Hooks::noop());

    assert!(st.acquire().await.is_err());
    // Simulate the peer coming back.
    cfg.set_fail(false);
    let session = st.acquire().await.unwrap();
    assert!(st.is_available());
    assert_eq!(cfg.opens(), 2);
    session.close().await.unwrap();
    assert!(!st.is_available());
}

#[tokio::test]
async fn handshake_error_rolls_back_refcount_and_closes_transport() {
    let (st, cfg) = build_with_hooks(failing_handshake_hooks());
    let err = st.acquire().await.unwrap_err();
    assert!(matches!(err, SessionError::Codec(_)));
    assert!(!st.is_available());
    assert_eq!(cfg.opens(), 1);
    // The opened transport must have dropped when the rollback fired
    // — otherwise the underlying conduit is leaked.
    assert_eq!(cfg.dropped_count().await, 1);

    // Subsequent acquire goes through the full open path again.
    let err2 = st.acquire().await.unwrap_err();
    assert!(matches!(err2, SessionError::Codec(_)));
    assert_eq!(cfg.opens(), 2);
    assert_eq!(cfg.dropped_count().await, 2);
}

#[tokio::test]
async fn while_open_constructor_panic_rolls_back_state_fully() {
    // A panic in the while-open closure body (before it can return a
    // future) must roll back the same way a handshake panic does:
    // refcount returns to 0, `is_available()` stays false, and the
    // opened transport is dropped so a fresh acquire goes through
    // the full open path again. This guards the publish-ordering
    // invariant flagged in Copilot's review of PR #269: slot /
    // available must not be published before the user-supplied
    // while-open closure has been called without panicking.
    let (st, cfg) = build_with_hooks(panicking_while_open_constructor_hooks());
    let st_for_task = st.clone();
    let result = tokio::spawn(async move { st_for_task.acquire().await }).await;
    assert!(result.is_err(), "expected panic, got {result:?}");
    assert!(!st.is_available());
    assert_eq!(cfg.opens(), 1);
    assert_eq!(cfg.dropped_count().await, 1);
}

#[tokio::test]
async fn handshake_panic_rolls_back_refcount() {
    // A panic in the handshake closure has to be caught by the rollback
    // guard so the count returns to 0 and `is_available()` stays false.
    let (st, cfg) = build_with_hooks(panicking_handshake_hooks());
    let st_for_task = st.clone();
    let result = tokio::spawn(async move { st_for_task.acquire().await }).await;
    // The join handle reports the panic (the test process itself
    // doesn't unwind because the panic was caught by tokio's task
    // boundary).
    assert!(result.is_err(), "expected panic, got {result:?}");
    assert!(!st.is_available());
    assert_eq!(cfg.opens(), 1);
    // Transport dropped as part of the unwind cleanup.
    assert_eq!(cfg.dropped_count().await, 1);

    // The transport is not wedged — a subsequent (still-panicking)
    // attempt also goes through the full open path.
    let st_for_task = st.clone();
    let result2 = tokio::spawn(async move { st_for_task.acquire().await }).await;
    assert!(result2.is_err());
    assert_eq!(cfg.opens(), 2);
}

#[tokio::test]
async fn alternating_failure_and_success_does_not_leak_count() {
    // Open succeeds, handshake fails, open succeeds again. The second
    // success should produce a working session. This is the closest
    // approximation of the qhy-focuser #258 scenario (handshake fails
    // mid-sequence) and the test fails loudly if any path leaves count
    // > 0.
    let cfg = FactoryConfig::default();
    let factory: Arc<dyn TransportFactory> = Arc::new(ProgrammableFactory::new(cfg.clone()));

    let attempt = std::sync::atomic::AtomicU32::new(0);
    let attempt = Arc::new(attempt);
    let attempt_for_closure = attempt.clone();

    let hooks = Hooks {
        handshake: Box::new(move |_| {
            let attempt = attempt_for_closure.clone();
            Box::pin(async move {
                let n = attempt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    Err(common::EchoCodecError("first attempt refused".into()))
                } else {
                    Ok(())
                }
            })
        }),
        on_last_disconnect: Box::new(|_| Box::pin(async {})),
        shutdown: Box::new(|_| Box::pin(async {})),
        while_open: None,
    };
    let st = build_with_factory_and_hooks(factory, hooks);

    // First attempt: handshake fails, rollback.
    assert!(st.acquire().await.is_err());
    assert!(!st.is_available());
    assert_eq!(cfg.opens(), 1);
    assert_eq!(cfg.dropped_count().await, 1);

    // Second attempt: succeeds, session is valid, transport stays open.
    let session = st.acquire().await.unwrap();
    assert!(st.is_available());
    assert_eq!(cfg.opens(), 2);
    let resp = session.request(b"ping".to_vec()).await.unwrap();
    assert_eq!(resp, b"ping");
    session.close().await.unwrap();
    assert!(!st.is_available());
}
