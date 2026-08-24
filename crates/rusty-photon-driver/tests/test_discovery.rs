//! Wire-level tests of the shared Alpaca discovery responder: the config
//! contract is `discovery_port: Some(port)` answers the spec's
//! `alpacadiscovery1` datagram with the serving Alpaca port, and the
//! default `None` binds nothing.

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

use std::net::SocketAddr;
use std::time::Duration;

#[tokio::test]
async fn responder_answers_discovery_with_the_alpaca_port() {
    // Loopback v4: no multicast joins involved, stable in CI.
    let alpaca_addr: SocketAddr = "127.0.0.1:11119".parse().unwrap();
    // Port 0: OS-assigned, so parallel tests never collide on 32227.
    let bound = rusty_photon_driver::discovery::bind(alpaca_addr, Some(0))
        .await
        .unwrap()
        .expect("Some(port) must bind a responder");
    let discovery_addr = bound.listen_addr();
    // A never-resolving serve future: the responder answers until the task
    // is dropped at test end, mirroring how services run it via serve_with.
    let server_task = tokio::spawn(rusty_photon_driver::discovery::serve_with(
        Some(bound),
        std::future::pending::<()>(),
    ));

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(b"alpacadiscovery1", discovery_addr)
        .await
        .unwrap();

    let mut buf = [0u8; 128];
    let (len, _) = tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut buf))
        .await
        .expect("discovery response within 5s")
        .unwrap();

    let reply: serde_json::Value = serde_json::from_slice(&buf[..len]).unwrap();
    assert_eq!(reply["AlpacaPort"], 11119);
    server_task.abort();
}

#[tokio::test]
async fn responder_closes_with_serving_so_a_reload_can_rebind() {
    let alpaca_addr: SocketAddr = "127.0.0.1:11119".parse().unwrap();
    let bound = rusty_photon_driver::discovery::bind(alpaca_addr, Some(0))
        .await
        .unwrap()
        .unwrap();
    let port = bound.listen_addr().port();

    // The serve future resolving is exactly what shutdown / SIGHUP reload
    // looks like from serve_with's perspective.
    rusty_photon_driver::discovery::serve_with(Some(bound), async {}).await;

    // The reload's rebuilt server must be able to rebind the same port —
    // a detached responder task would still hold it.
    let rebound = rusty_photon_driver::discovery::bind(alpaca_addr, Some(port))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rebound.listen_addr().port(), port);
}

#[tokio::test]
async fn no_responder_when_discovery_port_is_absent() {
    let alpaca_addr: SocketAddr = "127.0.0.1:11119".parse().unwrap();
    let bound = rusty_photon_driver::discovery::bind(alpaca_addr, None)
        .await
        .unwrap();
    assert!(bound.is_none());
}
