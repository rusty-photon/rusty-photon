//! The in-test Alpaca server rp's unit tests drive devices against:
//! [`spawn_stub`] binds an axum router on `127.0.0.1:0`, and each test
//! gives it the routes its case needs. The `ascom_alpaca` device traits
//! are too wide for `mockall`, so the code that drives a device through
//! them is tested against the wire format instead
//! (docs/skills/testing.md § 6.7, docs/services/rp.md § I/O
//! Abstractions).

use std::net::SocketAddr;

use axum::Router;

pub struct AlpacaStub {
    addr: SocketAddr,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl AlpacaStub {
    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for AlpacaStub {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

pub async fn spawn_stub(router: Router) -> AlpacaStub {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    AlpacaStub {
        addr,
        shutdown_tx: Some(tx),
        handle: Some(handle),
    }
}
