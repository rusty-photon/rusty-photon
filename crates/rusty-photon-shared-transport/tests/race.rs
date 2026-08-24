//! Issue #250 / #251 (and the umbrella #257) dissolve here: two
//! concurrent `acquire()` calls on a brand-new transport must result in
//! exactly **one** call to `TransportFactory::open` and both callers
//! must end up with a valid `Session` whose `request` round-trips.
//!
//! This was the bug class where each per-service `SerialManager` had
//! its own copy of the open/handshake/refcount dance and a check-modify
//! window in `set_connected` let two callers both think they were the
//! first.

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

use common::{build_noop_transport, CountingHooks};
use rusty_photon_shared_transport::SharedTransport;

#[tokio::test]
async fn two_concurrent_acquires_open_exactly_once() {
    let (st, cfg) = build_noop_transport();
    let opens = cfg.open_calls.clone();

    let a = {
        let st = st.clone();
        tokio::spawn(async move { st.acquire().await })
    };
    let b = {
        let st = st.clone();
        tokio::spawn(async move { st.acquire().await })
    };

    let sa = a.await.unwrap().unwrap();
    let sb = b.await.unwrap().unwrap();
    assert_eq!(opens.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(st.is_available());

    // Both sessions can request through the connection — they share
    // the same Arc<Connection> internally and serialise on the
    // command lock.
    let r1 = sa.request(b"ping".to_vec()).await.unwrap();
    let r2 = sb.request(b"pong".to_vec()).await.unwrap();
    assert_eq!(r1, b"ping");
    assert_eq!(r2, b"pong");
}

#[tokio::test]
async fn many_concurrent_acquires_open_exactly_once() {
    // Stress the race with a wider fan-out. 32 tasks all racing to
    // acquire; still exactly one open() call.
    let (st, cfg) = build_noop_transport();
    let opens = cfg.open_calls.clone();

    let handles: Vec<_> = (0..32)
        .map(|_| {
            let st = st.clone();
            tokio::spawn(async move { st.acquire().await })
        })
        .collect();

    let mut sessions = Vec::new();
    for h in handles {
        sessions.push(h.await.unwrap().unwrap());
    }
    assert_eq!(opens.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(st.is_available());
    assert_eq!(sessions.len(), 32);
}

#[tokio::test]
async fn handshake_runs_exactly_once_for_concurrent_acquires() {
    // The race fix is structural — handshake fires on the 0→1 path,
    // and no second caller racing in can trigger a second handshake.
    let counting = CountingHooks::default();
    let hs_calls = counting.handshake_calls.clone();
    let factory: Arc<dyn rusty_photon_shared_transport::TransportFactory> = Arc::new(
        common::ProgrammableFactory::new(common::FactoryConfig::default()),
    );
    let st = SharedTransport::new(factory, common::EchoCodec, counting.hooks());

    let a = {
        let st = st.clone();
        tokio::spawn(async move { st.acquire().await })
    };
    let b = {
        let st = st.clone();
        tokio::spawn(async move { st.acquire().await })
    };
    let _sa = a.await.unwrap().unwrap();
    let _sb = b.await.unwrap().unwrap();
    assert_eq!(hs_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}
