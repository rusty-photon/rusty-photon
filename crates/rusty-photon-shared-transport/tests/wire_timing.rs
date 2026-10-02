//! `request_timed` through the public handles.
//!
//! What the timing means — stamped at the write and at the answer,
//! with the command-lock wait in neither — is pinned next to
//! `Connection` in `src/connection.rs`. These check that a `Session`
//! and a `WhileOpen` task hand back the timing of the exchange they
//! made.

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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{build_noop_transport, build_with_hooks, EchoCodec};
use rusty_photon_shared_transport::{Hooks, StateAssertion, WhileOpen};
use tokio::sync::oneshot;
use tokio::time::Instant;

#[tokio::test(start_paused = true)]
async fn a_session_reports_when_its_request_crossed_the_wire() {
    let (st, _cfg) = build_noop_transport();
    let session = st.acquire().await.unwrap();
    let before = Instant::now();

    let (resp, timing) = session.request_timed(b"hello".to_vec()).await.unwrap();

    assert_eq!(resp, b"hello");
    // The echo answers at once and the clock is paused, so both stamps
    // land on the instant the request was made.
    assert_eq!(timing.sent_at, before);
    assert_eq!(timing.received_at, before);
    session.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_while_open_task_reports_when_its_request_crossed_the_wire() {
    let (tx, rx) = oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let hooks = Hooks {
        handshake: Box::new(|_| Box::pin(async { Ok(()) })),
        on_last_disconnect: Box::new(|_| Box::pin(async { StateAssertion::Asserted })),
        shutdown: Box::new(|_| Box::pin(async {})),
        while_open: Some(Box::new(move |ctx: WhileOpen<EchoCodec>| {
            let tx = Arc::clone(&tx);
            Box::pin(async move {
                let before = Instant::now();
                let result = ctx.request_timed(b"poll".to_vec()).await;
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send((before, result));
                }
                ctx.cancelled().await;
            })
        })),
    };
    let (st, _cfg) = build_with_hooks(hooks);
    let session = st.acquire().await.unwrap();

    let (before, result) = rx.await.unwrap();
    let (resp, timing) = result.unwrap();

    assert_eq!(resp, b"poll");
    assert_eq!(timing.sent_at, before);
    assert_eq!(timing.round_trip(), Duration::ZERO);
    session.close().await.unwrap();
}
