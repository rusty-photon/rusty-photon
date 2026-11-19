//! In-process HTTP webhook receiver that acts as an rp event plugin.
//!
//! Tests register the receiver's URL as a webhook in rp's plugin config
//! and then assert against captured events.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use serde_json::Value;
use tokio::sync::RwLock;

use super::basic_auth::basic_auth_header;

/// A single event captured by [`WebhookReceiver`].
///
/// `event_id`, `event_type` (the envelope's `event`), `timestamp` and
/// `payload` are on every envelope. The operation-lifecycle fields
/// below are `Option`: a point event omits them and an operation event
/// carries them. See `services/rp/src/events.rs::EventEnvelope`.
#[derive(Debug, Clone)]
pub struct ReceivedEvent {
    pub event_id: String,
    pub event_type: String,
    pub timestamp: String,
    /// Monotonic per-emission sequence number. Present on every
    /// envelope; `None` only if a malformed body omits it.
    pub event_seq: Option<u64>,
    /// Correlation key shared by an operation's `*_started`,
    /// `*_complete`, and `*_failed` events. Absent on point events.
    pub operation_id: Option<String>,
    /// RFC-3339 operation start, on the `*_started`/`*_complete`/`*_failed` triple.
    pub started_at: Option<String>,
    /// RFC-3339 operation end, on `*_complete`/`*_failed` only.
    pub ended_at: Option<String>,
    /// Wall-clock duration in ms, on `*_complete`/`*_failed` only.
    pub elapsed_ms: Option<u64>,
    /// Predicted duration, on the `*_started` event of an operation rp
    /// sizes a deadline for; absent on every other event.
    pub predicted_duration_ms: Option<u64>,
    /// Ceiling past which the operation is overrun, on the same
    /// `*_started` events as `predicted_duration_ms`.
    pub max_duration_ms: Option<u64>,
    pub payload: Value,
    pub received_at: std::time::Instant,
}

/// Shared state for the webhook receiver.
#[derive(Debug, Clone)]
struct WebhookReceiverState {
    events: Arc<RwLock<Vec<ReceivedEvent>>>,
    ack_estimated: Duration,
    ack_max: Duration,
    /// The exact `Authorization` header value `/webhook` demands, for an
    /// auth-enabled plugin. `None` accepts every request.
    required_auth: Option<Arc<String>>,
}

/// In-process HTTP server that acts as an event plugin.
#[derive(Debug)]
pub struct WebhookReceiver {
    pub url: String,
    pub port: u16,
    pub events: Arc<RwLock<Vec<ReceivedEvent>>>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl WebhookReceiver {
    /// Start the webhook receiver on a random port.
    ///
    /// The caller provides the shared events vector so test assertions can
    /// read from the same `Arc` the receiver writes to.
    pub async fn start(
        events: Arc<RwLock<Vec<ReceivedEvent>>>,
        ack_estimated: Duration,
        ack_max: Duration,
    ) -> Self {
        Self::start_inner(events, ack_estimated, ack_max, None).await
    }

    /// Start the receiver with HTTP Basic Auth on `/webhook`: anything but
    /// `username`/`password` is answered `401`, the way a plugin serving
    /// behind `rp_auth`'s middleware does. rp reaches it only when its
    /// registration carries a matching `auth` block (rp.md § Delivery:
    /// Webhooks).
    pub async fn start_requiring_auth(
        events: Arc<RwLock<Vec<ReceivedEvent>>>,
        ack_estimated: Duration,
        ack_max: Duration,
        username: &str,
        password: &str,
    ) -> Self {
        let expected = basic_auth_header(username, password);
        Self::start_inner(events, ack_estimated, ack_max, Some(Arc::new(expected))).await
    }

    async fn start_inner(
        events: Arc<RwLock<Vec<ReceivedEvent>>>,
        ack_estimated: Duration,
        ack_max: Duration,
        required_auth: Option<Arc<String>>,
    ) -> Self {
        let state = WebhookReceiverState {
            events: events.clone(),
            ack_estimated,
            ack_max,
            required_auth,
        };

        let app = Router::new()
            .route("/webhook", post(webhook_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind webhook receiver");
        let port = listener.local_addr().unwrap().port();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("webhook receiver failed");
        });

        Self {
            url: format!("http://127.0.0.1:{port}/webhook"),
            port,
            events,
            shutdown_tx: Some(shutdown_tx),
        }
    }

    /// Stop the webhook receiver.
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for WebhookReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn webhook_handler(
    State(state): State<WebhookReceiverState>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> (StatusCode, axum::Json<Value>) {
    if let Some(expected) = &state.required_auth {
        let presented = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if presented != expected.as_str() {
            // Nothing recorded: an unauthenticated caller never got in, so
            // `events` stays empty for the test to assert on.
            return (StatusCode::UNAUTHORIZED, axum::Json(Value::Null));
        }
    }

    let str_field = |key: &str| {
        body.get(key)
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string)
    };
    let u64_field = |key: &str| body.get(key).and_then(serde_json::Value::as_u64);
    let event = ReceivedEvent {
        event_id: str_field("event_id").unwrap_or_default(),
        event_type: str_field("event").unwrap_or_default(),
        timestamp: str_field("timestamp").unwrap_or_default(),
        event_seq: u64_field("event_seq"),
        operation_id: str_field("operation_id"),
        started_at: str_field("started_at"),
        ended_at: str_field("ended_at"),
        elapsed_ms: u64_field("elapsed_ms"),
        predicted_duration_ms: u64_field("predicted_duration_ms"),
        max_duration_ms: u64_field("max_duration_ms"),
        payload: body.get("payload").cloned().unwrap_or(Value::Null),
        received_at: std::time::Instant::now(),
    };

    state.events.write().await.push(event);

    let ack = serde_json::json!({
        "estimated_duration": humantime::format_duration(state.ack_estimated).to_string(),
        "max_duration": humantime::format_duration(state.ack_max).to_string()
    });

    (StatusCode::OK, axum::Json(ack))
}
