//! Event surface for `rp`.
//!
//! `rp` emits events when operations happen. Two delivery paths share one
//! [`EventBus`]:
//!
//! 1. **Webhooks** — the historical path. Plugins register a callback URL
//!    and a set of subscribed event types; the bus fire-and-forget POSTs
//!    matching events to each (see [`docs/services/rp.md` §Event System]).
//! 2. **In-process broadcast** — a [`tokio::sync::broadcast`] channel that
//!    carries every emitted [`EventEnvelope`]. Phase 3 of the
//!    predictive-deadlines plan wires this to a `/api/events/subscribe`
//!    SSE endpoint; unit tests use it as the assertion seam.
//!
//! Every event is wrapped in a uniform [`EventEnvelope`]. The envelope is
//! **additive** over the historical webhook body: the `event_id`, `event`,
//! `timestamp`, and `payload` keys keep their exact meaning, so existing
//! plugins are unaffected. New fields (`event_seq`, `operation_id`, the
//! `started_at` / `ended_at` / `elapsed_ms` timing, and the
//! `predicted_duration_ms` / `max_duration_ms` deadline fields — populated
//! on `slew_started` since Phase 2.1, omitted for operations not yet
//! converted) are carried alongside.
//!
//! Blocking operations emit a *triple*: a `*_started` envelope at the
//! entry point and a `*_complete` or `*_failed` envelope at the end, all
//! sharing one `operation_id` so a consumer can correlate them.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rp_auth::config::ClientAuthConfig;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::debug;
use uuid::Uuid;

use crate::config::EventSubscription;

/// Capacity of the in-process broadcast channel. A consumer that falls
/// further behind than this many events sees a
/// [`broadcast::error::RecvError::Lagged`] and resumes from the channel's
/// current tail; the channel itself keeps running.
const BROADCAST_CAPACITY: usize = 256;

/// Capacity of the in-memory event-history ring that backs `Last-Event-ID`
/// replay on the SSE stream ([`EventBus::subscribe_with_history`], Phase 3).
/// Larger than [`BROADCAST_CAPACITY`] so a consumer that lagged out of the
/// live channel (256) can still recover its missed window on reconnect.
/// Events older than the most recent `HISTORY_CAPACITY` are evicted; a
/// reconnecting client whose cursor predates the ring learns it lost events
/// via [`Subscription::oldest_retained_seq`].
const HISTORY_CAPACITY: usize = 512;

/// Total budget for one webhook delivery, connect included. Delivery is
/// fire-and-forget with no retry, so the bound exists to stop a stalled
/// subscriber from pinning a spawned task (and the envelope clone it
/// carries) for the rest of the night — not to give the plugin room to
/// work. The acknowledgement is prompt by contract: a plugin answers with
/// its duration estimates and processes in parallel (rp.md § Delivery:
/// Webhooks).
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// rp's client for webhook delivery: `ca_cert_path` is the observatory CA
/// (`Config::ca_cert_path`, rp.md § Configuration), without which an
/// `https://` `webhook_url` signed by that CA fails certificate
/// verification. Built once per bus rather than per delivery — the old
/// per-event build repeated TLS-backend setup for every event of the
/// night, and would have re-read the CA file just as often.
fn build_webhook_client(
    ca_cert_path: Option<&Path>,
) -> Result<reqwest::Client, Box<dyn std::error::Error + Send + Sync>> {
    Ok(rusty_photon_tls::client::client_builder(ca_cert_path)?
        .user_agent("rusty-photon-rp")
        .timeout(WEBHOOK_TIMEOUT)
        .build()?)
}

/// A webhook subscriber: a plugin that receives matching events via HTTP POST.
pub struct EventPlugin {
    pub name: String,
    pub webhook_url: String,
    pub subscribes_to: Vec<String>,
    /// The registration's `auth` credential, presented as HTTP Basic on
    /// every delivery. `None` for a plugin that does not challenge —
    /// every event to one that does would 401.
    ///
    /// Behind an `Arc` because [`EventBus::deliver_to_plugins`] hands one
    /// to a spawned task per event per subscriber: sharing it keeps the
    /// plaintext password from being copied on that path, which is the
    /// busiest one rp has.
    pub auth: Option<Arc<ClientAuthConfig>>,
}

impl From<EventSubscription> for EventPlugin {
    /// The parsed registration, ready to deliver to. The only thing the
    /// bus adds is the `Arc` around the credential — see [`EventPlugin::auth`].
    fn from(subscription: EventSubscription) -> Self {
        Self {
            name: subscription.name,
            webhook_url: subscription.webhook_url,
            subscribes_to: subscription.subscribes_to,
            auth: subscription.auth.map(Arc::new),
        }
    }
}

/// The uniform wire shape for every emitted event.
///
/// Serializes to the webhook body and (Phase 3) the SSE `data:` payload.
/// Optional fields are omitted from JSON when absent (`skip_serializing_if`),
/// so historical point events (e.g. `filter_switch`) keep their original
/// `{event_id, event, timestamp, payload}` shape plus the new `event_seq`.
#[derive(Debug, Clone, Serialize)]
pub struct EventEnvelope {
    /// Per-emission UUID. Unchanged from the historical webhook body; this
    /// is the routing key for the plugin completion contract
    /// (`POST /api/plugins/{event_id}/complete`). Assigned by the bus.
    pub event_id: String,
    /// Monotonically increasing per-emission counter. Total order across
    /// all events; used as the SSE `id` (and `Last-Event-ID` replay key)
    /// in Phase 3. Assigned by the bus.
    pub event_seq: u64,
    /// Correlation key shared by an operation's `*_started`, `*_complete`,
    /// and `*_failed` events. `None` for historical point events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// The event type string, e.g. `"slew_started"`.
    pub event: String,
    /// ISO-8601 emission timestamp. Unchanged historical format. Assigned
    /// by the bus.
    pub timestamp: String,
    /// When the operation began (RFC-3339, millisecond precision).
    /// Present on operation events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// When the operation ended. Present on `*_complete` / `*_failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// Wall-clock duration of the operation. Present on
    /// `*_complete` / `*_failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// Predicted operation duration. Populated on `slew_started`
    /// (Phase 2.1); omitted for operations not yet converted to
    /// predictive deadlines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predicted_duration_ms: Option<u64>,
    /// Hard ceiling beyond which the operation is considered overrun.
    /// Populated on `slew_started` (Phase 2.1); omitted for operations
    /// not yet converted to predictive deadlines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_duration_ms: Option<u64>,
    /// Operation-specific detail. For `*_started` this carries the inputs
    /// (e.g. target coordinates); for `*_complete` / `*_failed` it carries
    /// the outcome (result fields, or `{"error": "..."}`). Kept under the
    /// historical `payload` key so existing subscribers are unaffected.
    pub payload: Value,
}

impl EventEnvelope {
    /// Build a `*_started` envelope. `operation` is the event-name prefix
    /// (e.g. `"slew"` → `"slew_started"`). Identity fields (`event_id`,
    /// `event_seq`, `timestamp`) are filled in by the bus at emit time.
    #[must_use]
    pub fn started(
        operation: &str,
        operation_id: &str,
        started_at: DateTime<Utc>,
        payload: Value,
    ) -> Self {
        Self {
            event_id: String::new(),
            event_seq: 0,
            operation_id: Some(operation_id.to_string()),
            event: format!("{operation}_started"),
            timestamp: String::new(),
            started_at: Some(started_at.to_rfc3339_opts(SecondsFormat::Millis, true)),
            ended_at: None,
            elapsed_ms: None,
            predicted_duration_ms: None,
            max_duration_ms: None,
            payload,
        }
    }

    /// Attach the predicted duration and hard-ceiling deadline (both in
    /// milliseconds) to a `*_started` envelope (Phase 2). Chainable; every
    /// other field is untouched, so operations without a prediction keep
    /// both fields `None` (omitted from the JSON).
    #[must_use]
    pub const fn with_deadlines(mut self, predicted_ms: u64, max_ms: u64) -> Self {
        self.predicted_duration_ms = Some(predicted_ms);
        self.max_duration_ms = Some(max_ms);
        self
    }

    /// Build a `*_complete` envelope, computing `ended_at` and `elapsed_ms`
    /// from `started_at` and the current time. `payload` carries the
    /// operation outcome.
    #[must_use]
    pub fn complete(
        operation: &str,
        operation_id: &str,
        started_at: DateTime<Utc>,
        payload: Value,
    ) -> Self {
        Self::ended(operation, "complete", operation_id, started_at, payload)
    }

    /// Build a `*_settled` envelope — the success terminator for the
    /// settle-blocking guider operations (`guide_settled`,
    /// `dither_settled`; the event catalog uses "settled" rather than
    /// "complete" for operations that end by guiding stabilizing).
    /// Same timing semantics as [`EventEnvelope::complete`].
    #[must_use]
    pub fn settled(
        operation: &str,
        operation_id: &str,
        started_at: DateTime<Utc>,
        payload: Value,
    ) -> Self {
        Self::ended(operation, "settled", operation_id, started_at, payload)
    }

    /// Build a `*_failed` envelope. The error message is carried as
    /// `{"error": <message>}` in the payload.
    #[must_use]
    pub fn failed(
        operation: &str,
        operation_id: &str,
        started_at: DateTime<Utc>,
        error: &str,
    ) -> Self {
        Self::ended(
            operation,
            "failed",
            operation_id,
            started_at,
            serde_json::json!({ "error": error }),
        )
    }

    fn ended(
        operation: &str,
        suffix: &str,
        operation_id: &str,
        started_at: DateTime<Utc>,
        payload: Value,
    ) -> Self {
        let ended_at = Utc::now();
        let elapsed_ms = ended_at
            .signed_duration_since(started_at)
            .num_milliseconds()
            .max(0)
            .cast_unsigned();
        Self {
            event_id: String::new(),
            event_seq: 0,
            operation_id: Some(operation_id.to_string()),
            event: format!("{operation}_{suffix}"),
            timestamp: String::new(),
            started_at: Some(started_at.to_rfc3339_opts(SecondsFormat::Millis, true)),
            ended_at: Some(ended_at.to_rfc3339_opts(SecondsFormat::Millis, true)),
            elapsed_ms: Some(elapsed_ms),
            predicted_duration_ms: None,
            max_duration_ms: None,
            payload,
        }
    }
}

/// The result of subscribing to the event stream with a replay cursor,
/// returned by [`EventBus::subscribe_with_history`].
pub struct Subscription {
    /// Buffered envelopes with `event_seq` greater than the requested cursor,
    /// oldest first — replayed to the client before the live tail. Empty when
    /// no cursor was supplied.
    pub replay: Vec<EventEnvelope>,
    /// The live tail. Delivers every envelope dispatched after this
    /// subscription was created, exactly once and with no overlap with
    /// `replay` (the handoff is performed under one lock — see
    /// [`EventBus::subscribe_with_history`]).
    pub receiver: broadcast::Receiver<EventEnvelope>,
    /// The lowest `event_seq` still retained in the history ring at subscribe
    /// time, or `None` if no events have been emitted. The SSE handler
    /// compares this to the requested cursor: if the cursor is below
    /// `oldest_retained_seq - 1`, history was evicted past it and the handler
    /// emits a `stream_gap` so the client knows it lost events.
    pub oldest_retained_seq: Option<u64>,
}

/// Fans an emitted event out to webhook subscribers and to in-process
/// broadcast consumers, and retains a bounded history for SSE replay.
pub struct EventBus {
    plugins: Vec<EventPlugin>,
    /// Shared by every webhook delivery. Carries rp's top-level `ca_cert`
    /// trust, so a `webhook_url` served with the observatory's
    /// self-signed certificate verifies — the same wiring the orchestrator
    /// `/invoke` client carries (issue #800). `None` when no subscriber is
    /// registered: with nothing to deliver to there is no client to build,
    /// so a rig with no event plugins is never failed by a `ca_cert` it
    /// would not have used — the same rule
    /// [`crate::session::SessionManager`] follows for an absent
    /// orchestrator.
    client: Option<reqwest::Client>,
    broadcast: broadcast::Sender<EventEnvelope>,
    next_seq: AtomicU64,
    /// Bounded ring of recently emitted envelopes (newest at the back),
    /// capped at [`HISTORY_CAPACITY`]. Guards the broadcast handoff: the seq
    /// assignment, history append, and broadcast send all happen under this
    /// lock so a concurrent [`EventBus::subscribe_with_history`] sees each
    /// event in `replay` XOR on the live `receiver`, never both, never
    /// neither.
    history: Mutex<VecDeque<EventEnvelope>>,
}

impl EventBus {
    /// Build the bus from rp's `plugins` registrations. Fails when an
    /// event registration is not deliverable — a missing or malformed
    /// `name`, `webhook_url`, `subscribes_to`, or `auth`
    /// ([`EventSubscription::parse`]) — or when the webhook client cannot
    /// be built (an unreadable `ca_cert`). All are permanent
    /// configuration faults, and startup is the only place an operator is
    /// watching. Delivery itself never raises: it is fire-and-forget by
    /// design, which is precisely why an undeliverable registration must
    /// not survive to run — nothing downstream of here would ever report it.
    pub fn from_config(
        plugin_configs: &[Value],
        ca_cert_path: Option<&Path>,
    ) -> Result<Self, String> {
        let plugins = plugin_configs
            .iter()
            .enumerate()
            .filter(|(_, p)| crate::config::is_event_plugin(p))
            .map(|(index, p)| {
                EventSubscription::parse(index, p)
                    .map(EventPlugin::from)
                    // The same rendering `load_config` gives a
                    // `FieldError`, so the message an operator sees does
                    // not depend on which of the two rejected the config.
                    .map_err(|e| format!("{} {}", e.path, e.msg))
            })
            .collect::<Result<Vec<_>, String>>()?;

        let client = if plugins.is_empty() {
            None
        } else {
            Some(
                build_webhook_client(ca_cert_path)
                    .map_err(|e| format!("failed to build the webhook HTTP client: {e}"))?,
            )
        };

        let (broadcast, _rx) = broadcast::channel(BROADCAST_CAPACITY);

        Ok(Self {
            plugins,
            client,
            broadcast,
            // Start at 1 so the first event has event_seq == 1.
            next_seq: AtomicU64::new(1),
            history: Mutex::new(VecDeque::with_capacity(HISTORY_CAPACITY)),
        })
    }

    /// Subscribe to the in-process event stream. Each subscriber receives
    /// every envelope emitted after it subscribes. Used by the SSE endpoint
    /// (Phase 3) and by tests.
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.broadcast.subscribe()
    }

    /// Subscribe with `Last-Event-ID` replay. Returns the buffered envelopes
    /// after `last_seq` (oldest first) together with a live receiver, with a
    /// race-free handoff: the history snapshot and `broadcast.subscribe()` are
    /// taken under the same lock that [`EventBus::dispatch`] holds across its
    /// history-append + broadcast-send, so every event is delivered via
    /// `replay` or the live `receiver` exactly once — never duplicated at the
    /// boundary, never dropped. `last_seq == None` requests no replay (a fresh
    /// subscriber that only wants the live tail).
    pub fn subscribe_with_history(&self, last_seq: Option<u64>) -> Subscription {
        let history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let oldest_retained_seq = history.front().map(|e| e.event_seq);
        let replay = last_seq.map_or_else(Vec::new, |cursor| {
            history
                .iter()
                .filter(|e| e.event_seq > cursor)
                .cloned()
                .collect()
        });
        // subscribe() under the lock: it only delivers sends that happen after
        // this point, and dispatch's send is under the same lock, so nothing
        // emitted before now reaches the live receiver (it is in `replay`
        // instead) and nothing emitted after now is missed.
        let receiver = self.broadcast.subscribe();
        Subscription {
            replay,
            receiver,
            oldest_retained_seq,
        }
    }

    /// Emit a historical point event (no operation lifecycle). Keeps the
    /// original signature; the envelope carries `operation_id == None`.
    pub fn emit(&self, event_type: &str, payload: Value) {
        let envelope = EventEnvelope {
            event_id: String::new(),
            event_seq: 0,
            operation_id: None,
            event: event_type.to_string(),
            timestamp: String::new(),
            started_at: None,
            ended_at: None,
            elapsed_ms: None,
            predicted_duration_ms: None,
            max_duration_ms: None,
            payload,
        };
        self.dispatch(envelope);
    }

    /// Emit an operation lifecycle event built via [`EventEnvelope::started`],
    /// [`EventEnvelope::complete`], or [`EventEnvelope::failed`].
    pub fn emit_operation(&self, envelope: EventEnvelope) {
        self.dispatch(envelope);
    }

    /// Assign identity fields, retain the envelope in the history ring, then
    /// fan out to webhooks and the broadcast channel.
    fn dispatch(&self, mut envelope: EventEnvelope) {
        envelope.event_id = Uuid::new_v4().to_string();
        envelope.timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        // Seq assignment, the history append, and the broadcast send happen
        // under one lock so that (a) history stays in event_seq order even
        // under concurrent emitters, and (b) the replay→live handoff in
        // subscribe_with_history is race-free (each event is replayed XOR
        // delivered live). The webhook fan-out is an independent path and runs
        // after the lock is released.
        {
            let mut history = self
                .history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            envelope.event_seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
            if history.len() >= HISTORY_CAPACITY {
                history.pop_front();
            }
            history.push_back(envelope.clone());

            // Broadcast for in-process consumers. An error means there are no
            // active subscribers (or all have lagged out); that is expected
            // and harmless — the webhook path below is independent.
            let _ = self.broadcast.send(envelope.clone());
        }

        self.deliver_to_plugins(&envelope);
    }

    fn deliver_to_plugins(&self, envelope: &EventEnvelope) {
        // `None` only when no subscriber is registered, in which case the
        // loop below has nothing to iterate anyway.
        let Some(client) = self.client.as_ref() else {
            return;
        };
        let event_type = &envelope.event;
        for plugin in &self.plugins {
            if plugin.subscribes_to.iter().any(|s| s == event_type) {
                let url = plugin.webhook_url.clone();
                let name = plugin.name.clone();
                let event_type = event_type.clone();
                let body = match serde_json::to_value(envelope) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            plugin = %name,
                            event = %event_type,
                            error = %e,
                            "skipping event delivery: envelope serialization failed"
                        );
                        continue;
                    }
                };

                let client = client.clone();
                let auth = plugin.auth.clone();

                tokio::spawn(async move {
                    debug!(plugin = %name, event = %event_type, url = %url, "emitting event to plugin");
                    let mut request = client.post(&url).json(&body);
                    if let Some(auth) = &auth {
                        // Per-request rather than a default header on the
                        // client: reqwest marks the header sensitive here,
                        // and a default header would put the credential in
                        // the shared client's Debug output.
                        request = request.basic_auth(&auth.username, Some(&auth.password));
                    }
                    match request.send().await {
                        Ok(resp) if resp.status().is_success() => {
                            debug!(plugin = %name, event = %event_type, status = %resp.status(), "event delivered");
                        }
                        // A plugin that answers but does not accept is the
                        // failure this path is worst at showing: `send()`
                        // returns `Ok` for a 401 or a 500 alike, delivery
                        // is fire-and-forget, and the night continues.
                        // Logged at `warn!` because it is
                        // operator-actionable — a wrong `auth` block
                        // silently costs a plugin its whole night's work
                        // otherwise. The wording stays neutral across the
                        // whole non-2xx range, since a 500 is the plugin
                        // failing rather than refusing; `status` says
                        // which.
                        Ok(resp) => {
                            tracing::warn!(plugin = %name, event = %event_type, status = %resp.status(), "plugin returned a non-success status");
                        }
                        Err(e) => {
                            debug!(plugin = %name, event = %event_type, error = %e, "failed to deliver event");
                        }
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Test code opts out of the workspace restriction lints (see root
    // Cargo.toml `[workspace.lints.clippy]`); per AGENTS rule 7 tests
    // prefer `unwrap()` so a failure shows what the error was.

    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;

    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::json;
    use tokio::sync::RwLock;

    use super::*;

    fn bus() -> EventBus {
        EventBus::from_config(&[], None).unwrap()
    }

    #[tokio::test]
    async fn legacy_emit_has_no_operation_id_and_monotonic_seq() {
        let bus = bus();
        let mut rx = bus.subscribe();

        bus.emit("filter_switch", json!({ "filter_name": "Ha" }));
        bus.emit("safety_changed", json!({ "monitor": "roof" }));

        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();

        assert_eq!(first.event, "filter_switch");
        assert!(first.operation_id.is_none());
        assert_eq!(first.payload["filter_name"], "Ha");
        // event_seq is monotonically increasing across emissions.
        assert_eq!(second.event_seq, first.event_seq + 1);
        // Distinct per-emission event_id.
        assert_ne!(first.event_id, second.event_id);
    }

    #[tokio::test]
    async fn operation_triple_shares_operation_id() {
        let bus = bus();
        let mut rx = bus.subscribe();
        let operation_id = "op-123";
        let started_at = Utc::now();

        bus.emit_operation(EventEnvelope::started(
            "slew",
            operation_id,
            started_at,
            json!({ "ra": 12.0, "dec": -30.0 }),
        ));
        bus.emit_operation(EventEnvelope::complete(
            "slew",
            operation_id,
            started_at,
            json!({ "actual_ra": 12.0, "actual_dec": -30.0 }),
        ));

        let started = rx.recv().await.unwrap();
        let complete = rx.recv().await.unwrap();

        assert_eq!(started.event, "slew_started");
        assert_eq!(complete.event, "slew_complete");
        // Same operation_id correlates the two events.
        assert_eq!(started.operation_id.as_deref(), Some(operation_id));
        assert_eq!(complete.operation_id.as_deref(), Some(operation_id));
        // ...but each emission has its own event_id and event_seq.
        assert_ne!(started.event_id, complete.event_id);
        assert!(complete.event_seq > started.event_seq);
        // started carries started_at but no end timing.
        assert!(started.started_at.is_some());
        assert!(started.ended_at.is_none());
        // complete carries the full timing trio.
        assert!(complete.ended_at.is_some());
        assert!(complete.elapsed_ms.is_some());
        // Phase 1 reserves but never populates the deadline fields.
        assert!(started.predicted_duration_ms.is_none());
        assert!(complete.max_duration_ms.is_none());
    }

    #[tokio::test]
    async fn settled_is_a_complete_shaped_terminator_with_the_settled_suffix() {
        let bus = bus();
        let mut rx = bus.subscribe();
        let started_at = Utc::now();

        bus.emit_operation(EventEnvelope::settled(
            "guide",
            "op-7",
            started_at,
            json!({ "rms_ra_px": 0.3 }),
        ));

        let settled = rx.recv().await.unwrap();
        assert_eq!(settled.event, "guide_settled");
        assert_eq!(settled.operation_id.as_deref(), Some("op-7"));
        assert_eq!(settled.payload["rms_ra_px"], 0.3);
        assert!(settled.started_at.is_some());
        assert!(settled.ended_at.is_some());
        assert!(settled.elapsed_ms.is_some());
    }

    #[tokio::test]
    async fn failed_carries_error_payload() {
        let bus = bus();
        let mut rx = bus.subscribe();
        let started_at = Utc::now();

        bus.emit_operation(EventEnvelope::failed(
            "park",
            "op-9",
            started_at,
            "mount unreachable",
        ));

        let failed = rx.recv().await.unwrap();
        assert_eq!(failed.event, "park_failed");
        assert_eq!(failed.payload["error"], "mount unreachable");
        assert!(failed.elapsed_ms.is_some());
    }

    #[test]
    fn envelope_omits_absent_optional_fields_in_json() {
        // A legacy point event serializes to the historical shape plus
        // event_seq — no operation_id / timing / deadline keys.
        let envelope = EventEnvelope {
            event_id: "e1".to_string(),
            event_seq: 7,
            operation_id: None,
            event: "filter_switch".to_string(),
            timestamp: "2026-05-19T20:14:33Z".to_string(),
            started_at: None,
            ended_at: None,
            elapsed_ms: None,
            predicted_duration_ms: None,
            max_duration_ms: None,
            payload: json!({ "filter_name": "Ha" }),
        };
        let json = serde_json::to_value(&envelope).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.contains_key("event_id"));
        assert!(obj.contains_key("event_seq"));
        assert!(obj.contains_key("event"));
        assert!(obj.contains_key("timestamp"));
        assert!(obj.contains_key("payload"));
        assert!(!obj.contains_key("operation_id"));
        assert!(!obj.contains_key("started_at"));
        assert!(!obj.contains_key("predicted_duration_ms"));
    }

    #[test]
    fn subscribe_with_history_replays_events_after_cursor() {
        let bus = bus();
        bus.emit("tick", json!({ "n": 1 }));
        bus.emit("tick", json!({ "n": 2 }));
        bus.emit("tick", json!({ "n": 3 }));

        // A reconnecting client that last saw event_seq 1 is replayed 2 and 3.
        let sub = bus.subscribe_with_history(Some(1));
        let seqs: Vec<u64> = sub.replay.iter().map(|e| e.event_seq).collect();
        assert_eq!(seqs, vec![2, 3]);
        assert_eq!(sub.oldest_retained_seq, Some(1));
    }

    #[test]
    fn subscribe_with_history_none_replays_nothing() {
        let bus = bus();
        bus.emit("tick", json!({ "n": 1 }));
        bus.emit("tick", json!({ "n": 2 }));

        // No cursor → a fresh subscriber that only wants the live tail.
        let sub = bus.subscribe_with_history(None);
        assert!(sub.replay.is_empty());
        // History is retained regardless of whether replay was requested.
        assert_eq!(sub.oldest_retained_seq, Some(1));
    }

    #[test]
    fn history_evicts_oldest_beyond_capacity() {
        let bus = bus();
        let total = (HISTORY_CAPACITY + 5) as u64;
        for n in 1..=total {
            bus.emit("tick", json!({ "n": n }));
        }

        let sub = bus.subscribe_with_history(Some(0));
        // Only the most recent HISTORY_CAPACITY events are retained.
        assert_eq!(sub.replay.len(), HISTORY_CAPACITY);
        // The first five (seq 1..=5) were evicted; the oldest retained is 6.
        assert_eq!(sub.oldest_retained_seq, Some(6));
        assert_eq!(sub.replay.first().unwrap().event_seq, 6);
        assert_eq!(sub.replay.last().unwrap().event_seq, total);
    }

    #[tokio::test]
    async fn replay_then_live_handoff_delivers_each_event_once() {
        let bus = bus();
        // Two events before subscribing → land in replay.
        bus.emit("tick", json!({ "n": 1 }));
        bus.emit("tick", json!({ "n": 2 }));

        let mut sub = bus.subscribe_with_history(Some(0));
        let replay_seqs: Vec<u64> = sub.replay.iter().map(|e| e.event_seq).collect();
        assert_eq!(replay_seqs, vec![1, 2]);

        // Two events after subscribing → arrive live, exactly once, in order,
        // and are NOT also duplicated into the replay snapshot above.
        bus.emit("tick", json!({ "n": 3 }));
        bus.emit("tick", json!({ "n": 4 }));

        let live1 = sub.receiver.recv().await.unwrap();
        let live2 = sub.receiver.recv().await.unwrap();
        assert_eq!(live1.event_seq, 3);
        assert_eq!(live2.event_seq, 4);
    }

    #[test]
    fn oldest_retained_seq_exposes_unreplayable_gap() {
        let bus = bus();
        let total = (HISTORY_CAPACITY + 5) as u64;
        for n in 1..=total {
            bus.emit("tick", json!({ "n": n }));
        }

        // A client reconnecting from seq 2 cannot be fully replayed: seq 3..=5
        // were evicted. The subscription replays what remains, and
        // oldest_retained_seq (6) lets the handler detect the gap because the
        // requested cursor + 1 (3) is below it.
        let sub = bus.subscribe_with_history(Some(2));
        assert_eq!(sub.oldest_retained_seq, Some(6));
        assert_eq!(sub.replay.first().unwrap().event_seq, 6);
        assert!(sub.oldest_retained_seq.unwrap() > 2 + 1);
    }

    // Webhook delivery had neither CA trust nor a credential, which pinned
    // every event plugin to plain HTTP: the moment one served TLS or
    // 401-challenged, it silently stopped receiving events — a
    // fire-and-forget path logs the failure at `debug!` and moves on, so
    // nothing surfaces until someone notices the plugin did no work all
    // night. These tests pin both halves against a stub that actually
    // challenges and one that actually serves a CA-signed certificate,
    // because the config fields alone prove nothing about the client that
    // reads them.

    /// A webhook subscriber, and what it received. `_shutdown` stops the
    /// server when the stub drops.
    struct WebhookStub {
        url: String,
        bodies: Arc<RwLock<Vec<Value>>>,
        hits: Arc<AtomicU32>,
        _shutdown: tokio::sync::oneshot::Sender<()>,
    }

    /// Subscriber stub that 401s unless the request carries exactly
    /// `Basic <base64(user:pass)>`, mirroring `rp_auth`'s middleware.
    async fn spawn_challenging_webhook_stub(username: &str, password: &str) -> WebhookStub {
        use base64::engine::general_purpose::STANDARD as BASE64;
        use base64::Engine as _;

        #[derive(Clone)]
        struct StubState {
            bodies: Arc<RwLock<Vec<Value>>>,
            hits: Arc<AtomicU32>,
            expected: Arc<String>,
        }
        let state = StubState {
            bodies: Arc::new(RwLock::new(Vec::new())),
            hits: Arc::new(AtomicU32::new(0)),
            expected: Arc::new(format!(
                "Basic {}",
                BASE64.encode(format!("{username}:{password}"))
            )),
        };
        let app = Router::new()
            .route(
                "/webhook",
                post(
                    |State(state): State<StubState>,
                     headers: HeaderMap,
                     Json(body): Json<Value>| async move {
                        let presented = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default();
                        let accepted = presented == state.expected.as_str();
                        if accepted {
                            state.bodies.write().await.push(body);
                        }
                        // Counted last, so an observer that sees the hit
                        // also sees the recorded body — no test race.
                        state.hits.fetch_add(1, Ordering::SeqCst);
                        if accepted {
                            StatusCode::OK
                        } else {
                            StatusCode::UNAUTHORIZED
                        }
                    },
                ),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        WebhookStub {
            url: format!("http://127.0.0.1:{port}/webhook"),
            bodies: state.bodies,
            hits: state.hits,
            _shutdown: tx,
        }
    }

    /// Delivery is fire-and-forget on a spawned task, so every assertion
    /// about what a subscriber received has to wait for it. Bounded so a
    /// regression fails loudly instead of hanging the suite; `budget` is
    /// explicit because the negative case — proving a delivery does *not*
    /// arrive — pays the whole of it every run.
    async fn wait_for_count(hits: &AtomicU32, expected: u32, budget: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        while tokio::time::Instant::now() < deadline {
            if hits.load(Ordering::SeqCst) >= expected {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        hits.load(Ordering::SeqCst) >= expected
    }

    /// Comfortably past [`WEBHOOK_TIMEOUT`], the budget the production
    /// client gives one delivery, so only a delivery that never arrives
    /// fails these tests — a merely slow one on a loaded runner cannot.
    /// Derived from that constant rather than hand-picked, so raising the
    /// production timeout cannot leave the tests waiting less than the
    /// code is allowed to take. Multiplied as a `Duration`, not via
    /// `as_secs()`: a sub-second `WEBHOOK_TIMEOUT` would truncate to a
    /// zero budget and fail every one of these tests instantly. Costs
    /// nothing when the delivery lands: [`wait_for_count`] polls and
    /// returns early.
    const DELIVERY_BUDGET: Duration = WEBHOOK_TIMEOUT.saturating_mul(2);

    fn bus_with_subscriber(webhook_url: &str, auth: Option<Value>) -> EventBus {
        let mut entry = json!({
            "name": "image-analyzer",
            "type": "event",
            "webhook_url": webhook_url,
            "subscribes_to": ["filter_switch"],
        });
        if let Some(auth) = auth {
            entry
                .as_object_mut()
                .unwrap()
                .insert("auth".to_string(), auth);
        }
        EventBus::from_config(&[entry], None).unwrap()
    }

    #[tokio::test]
    async fn delivery_presents_the_registration_credential() {
        let stub = spawn_challenging_webhook_stub("observatory", "secret").await;
        let bus = bus_with_subscriber(
            &stub.url,
            Some(json!({"username": "observatory", "password": "secret"})),
        );

        bus.emit("filter_switch", json!({"filter_name": "Ha"}));

        assert!(
            wait_for_count(&stub.hits, 1, DELIVERY_BUDGET).await,
            "event never delivered"
        );
        assert_eq!(
            stub.bodies.read().await.len(),
            1,
            "the challenging subscriber never accepted the credential"
        );
    }

    /// The negative half of the test above: without the registration's
    /// `auth`, the same stub 401s — proving the credential, not the stub's
    /// leniency, is what makes delivery land.
    #[tokio::test]
    async fn an_uncredentialed_delivery_is_rejected_by_a_challenging_subscriber() {
        let stub = spawn_challenging_webhook_stub("observatory", "secret").await;
        let bus = bus_with_subscriber(&stub.url, None);

        bus.emit("filter_switch", json!({"filter_name": "Ha"}));

        assert!(
            wait_for_count(&stub.hits, 1, DELIVERY_BUDGET).await,
            "event never delivered"
        );
        assert!(
            stub.bodies.read().await.is_empty(),
            "the stub must not have accepted an uncredentialed delivery"
        );
    }

    /// A subscriber that does not challenge still receives the event, with
    /// no `Authorization` header invented for it.
    #[tokio::test]
    async fn an_uncredentialed_registration_sends_no_authorization_header() {
        let recorded: Arc<RwLock<Vec<Option<String>>>> = Arc::new(RwLock::new(Vec::new()));
        let hits = Arc::new(AtomicU32::new(0));
        let handler_recorded = recorded.clone();
        let handler_hits = hits.clone();
        let app = Router::new().route(
            "/webhook",
            post(move |headers: HeaderMap| {
                let recorded = handler_recorded.clone();
                let hits = handler_hits.clone();
                async move {
                    recorded.write().await.push(
                        headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(String::from),
                    );
                    hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let bus = bus_with_subscriber(&format!("http://127.0.0.1:{port}/webhook"), None);
        bus.emit("filter_switch", json!({"filter_name": "Ha"}));

        assert!(
            wait_for_count(&hits, 1, DELIVERY_BUDGET).await,
            "event never delivered"
        );
        assert_eq!(recorded.read().await.as_slice(), &[None]);
    }

    #[tokio::test]
    async fn a_malformed_subscriber_auth_block_fails_startup() {
        let plugins = vec![json!({
            "name": "image-analyzer",
            "type": "event",
            "webhook_url": "http://127.0.0.1:11140/webhook",
            "subscribes_to": ["exposure_complete"],
            "auth": {"username": "observatory"},
        })];

        let Err(error) = EventBus::from_config(&plugins, None) else {
            panic!("a half-written credential must fail startup");
        };
        assert!(
            error.contains("plugins.0.auth") && error.contains("password"),
            "a half-written credential must name the field it broke: {error}"
        );
    }

    /// An event registration that can receive nothing is a fault, not an
    /// inert entry: the bus refuses to build rather than start a night
    /// with a plugin that looks registered and is silently never
    /// delivered to. Each case names the field an operator must fix.
    #[test]
    fn an_undeliverable_registration_fails_startup() {
        for (case, entry, expected_field) in [
            (
                "no name",
                json!({
                    "type": "event",
                    "webhook_url": "http://127.0.0.1:11140/webhook",
                    "subscribes_to": ["exposure_complete"],
                }),
                "plugins.0.name",
            ),
            (
                "no webhook_url",
                json!({
                    "name": "image-analyzer",
                    "type": "event",
                    "subscribes_to": ["exposure_complete"],
                }),
                "plugins.0.webhook_url",
            ),
            (
                "no subscribes_to",
                json!({
                    "name": "image-analyzer",
                    "type": "event",
                    "webhook_url": "http://127.0.0.1:11140/webhook",
                }),
                "plugins.0.subscribes_to",
            ),
            (
                // The typo this whole rule exists for: a plugin that
                // looks subscribed and receives nothing until dawn.
                "subscribes_to misspelled",
                json!({
                    "name": "image-analyzer",
                    "type": "event",
                    "webhook_url": "http://127.0.0.1:11140/webhook",
                    "subscribe_to": ["exposure_complete"],
                }),
                "plugins.0.subscribes_to",
            ),
            (
                "subscribes_to empty",
                json!({
                    "name": "image-analyzer",
                    "type": "event",
                    "webhook_url": "http://127.0.0.1:11140/webhook",
                    "subscribes_to": [],
                }),
                "plugins.0.subscribes_to",
            ),
            (
                "subscribes_to holding a non-string",
                json!({
                    "name": "image-analyzer",
                    "type": "event",
                    "webhook_url": "http://127.0.0.1:11140/webhook",
                    "subscribes_to": ["exposure_complete", 42],
                }),
                "plugins.0.subscribes_to",
            ),
        ] {
            let Err(error) = EventBus::from_config(&[entry], None) else {
                panic!("{case}: an undeliverable registration must fail startup");
            };
            assert!(
                error.contains(expected_field),
                "{case}: must name {expected_field}, got {error}"
            );
        }
    }

    /// The bus reads only the registrations it delivers to, so a plugin
    /// of another kind keeps its own shape — a tool provider carrying
    /// neither `webhook_url` nor `subscribes_to` is not an undeliverable
    /// subscriber, it is simply not a subscriber.
    #[test]
    fn an_undialed_registration_is_not_held_to_the_subscriber_rules() {
        let plugins = vec![json!({
            "name": "ml-quality-classifier",
            "type": "tool_provider",
            "mcp_server_url": "http://127.0.0.1:11150/mcp",
        })];

        EventBus::from_config(&plugins, None).unwrap();
    }

    #[tokio::test]
    async fn an_unreadable_ca_cert_fails_startup() {
        let plugins = vec![json!({
            "name": "image-analyzer",
            "type": "event",
            "webhook_url": "https://127.0.0.1:11140/webhook",
            "subscribes_to": ["exposure_complete"],
        })];

        let Err(error) = EventBus::from_config(&plugins, Some(Path::new("/nonexistent/ca.pem")))
        else {
            panic!("an unreadable ca_cert must fail startup");
        };
        assert!(
            error.contains("webhook HTTP client"),
            "unexpected error: {error}"
        );
    }

    /// The other half of that rule: with no subscriber registered there is
    /// no client to build, so a `ca_cert` rp would never have used on this
    /// path does not fail startup — the same shape
    /// `SessionManager` follows for an absent orchestrator.
    #[tokio::test]
    async fn a_subscriberless_bus_does_not_touch_the_ca_cert() {
        EventBus::from_config(&[], Some(Path::new("/nonexistent/ca.pem"))).unwrap();
    }

    /// Proves the webhook client actually plumbs `ca_cert_path` into
    /// reqwest: a bus carrying the observatory CA delivers to a subscriber
    /// serving a CA-signed certificate, and the same bus without that
    /// trust never gets past the handshake.
    #[tokio::test]
    async fn a_ca_trusting_bus_delivers_to_a_tls_subscriber() {
        let pki_dir = tempfile::tempdir().unwrap();
        rusty_photon_tls::test_cert::generate_ca(pki_dir.path()).unwrap();
        let ca_cert_pem = std::fs::read_to_string(pki_dir.path().join("ca.pem")).unwrap();
        let ca_key_pem = std::fs::read_to_string(pki_dir.path().join("ca-key.pem")).unwrap();
        let certs_dir = pki_dir.path().join("certs");
        rusty_photon_tls::test_cert::generate_service_cert(
            &ca_cert_pem,
            &ca_key_pem,
            "image-analyzer",
            &certs_dir,
        )
        .unwrap();
        let tls_config = rusty_photon_tls::config::TlsConfig {
            cert: certs_dir
                .join("image-analyzer.pem")
                .to_string_lossy()
                .into_owned(),
            key: certs_dir
                .join("image-analyzer-key.pem")
                .to_string_lossy()
                .into_owned(),
        };

        let hits = Arc::new(AtomicU32::new(0));
        let handler_hits = hits.clone();
        let router = Router::new().route(
            "/webhook",
            post(move |Json(_body): Json<Value>| {
                let hits = handler_hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = rusty_photon_tls::server::bind_dual_stack_tokio(addr)
            .await
            .unwrap();
        let bound_addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            rusty_photon_tls::server::serve_tls(listener, router, &tls_config, async {
                shutdown_rx.await.ok();
            })
            .await
            .unwrap();
        });

        // The bound IPv4 loopback, not "localhost": the listener is
        // IPv4-only and the generated cert's SANs include 127.0.0.1.
        let webhook_url = format!("https://{bound_addr}/webhook");
        let entry = json!({
            "name": "image-analyzer",
            "type": "event",
            "webhook_url": webhook_url,
            "subscribes_to": ["filter_switch"],
        });
        let plugins = [entry];
        let ca_path = pki_dir.path().join("ca.pem");

        let trusting = EventBus::from_config(&plugins, Some(&ca_path)).unwrap();
        trusting.emit("filter_switch", json!({"filter_name": "Ha"}));
        assert!(
            wait_for_count(&hits, 1, DELIVERY_BUDGET).await,
            "a bus trusting the CA must reach the TLS subscriber"
        );

        let untrusting = EventBus::from_config(&plugins, None).unwrap();
        untrusting.emit("filter_switch", json!({"filter_name": "Ha"}));
        // Absence has nothing to poll for, so this one pays its whole
        // budget every run: a short grace period for the spawned task to
        // reach the handshake and fail it, deliberately not
        // DELIVERY_BUDGET, which is sized for a delivery that *arrives*.
        assert!(
            !wait_for_count(&hits, 2, Duration::from_millis(500)).await,
            "a bus without the CA must fail the handshake"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "no untrusted request may reach the subscriber's handler"
        );

        shutdown_tx.send(()).ok();
        server.await.ok();
    }
}
