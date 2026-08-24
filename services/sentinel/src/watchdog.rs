//! Operation watchdog — the push-based [`EventMonitor`] that subscribes to
//! rp's Server-Sent-Events stream, tracks per-operation deadlines, and
//! escalates a miss (or a dead rp) through the notifier chain.
//!
//! This is the Sentinel half of the two-loop supervision design: rp emits a
//! `*_started` event carrying a `max_duration_ms` and later a `*_complete` /
//! `*_failed`; [`OperationDeadlineMonitor`] tracks those deadlines
//! independently and reacts when one is missed or when the stream (and thus
//! rp) goes away. See `docs/services/sentinel.md` §Operation Watchdog and
//! `docs/services/rp.md` §Real-Time Stream for the wire contract.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::config::{OnExpiry, OperationWatchdogConfig};
use crate::corrective::{Corrective, CorrectiveTarget};
use crate::notifier::{Notification, NotificationRecord, Notifier};
use crate::state::StateHandle;

/// A self-driving monitor task that owns its own lifecycle rather than
/// being polled by the engine on an interval.
///
/// It owns either a long-lived connection it reacts to (the operation
/// watchdog) or a poll loop it paces itself (the service health
/// supervisor). The engine spawns one task per `EventMonitor` and runs
/// it until the cancellation token fires.
#[async_trait]
pub trait EventMonitor: Send + Sync + std::fmt::Debug {
    /// Display name (used in logs and notification history records).
    fn name(&self) -> &str;

    /// Run until `cancel` fires. Owns its own connection lifecycle,
    /// including reconnect.
    async fn run(&self, cancel: CancellationToken);
}

/// One parsed SSE frame from rp's event stream.
#[derive(Debug, Clone)]
pub struct SseFrame {
    /// SSE `id:` — the envelope's `event_seq` (the reconnect cursor).
    pub id: Option<u64>,
    /// SSE `event:` — the event type (e.g. `"slew_started"`).
    pub event: Option<String>,
    /// SSE `data:` — the full event-envelope JSON.
    pub data: String,
}

impl SseFrame {
    fn json(&self) -> Value {
        serde_json::from_str(&self.data).unwrap_or(Value::Null)
    }
}

/// What a frame means to the watchdog.
#[derive(Debug, PartialEq, Eq)]
enum FrameAction {
    /// An operation began; start tracking it.
    Started {
        family: String,
        operation_id: String,
        max_duration_ms: Option<u64>,
    },
    /// An operation finished (`*_complete` / `*_failed`); stop tracking it.
    Ended { operation_id: String },
    /// rp signalled lost history — every open operation is now unconfirmed.
    Gap,
    /// Not a lifecycle event the watchdog reacts to (keep-alive, point
    /// event, per-iteration progress, or a `*_started` with no
    /// `operation_id` to key on).
    Ignore,
}

/// Classify a frame by its event type. The operation *family* is the event
/// name with its lifecycle suffix stripped (`slew_started` → `slew`).
///
/// The `data` payload is parsed exactly once here (rather than per field) — the
/// event stream can be high-volume and re-parsing on the hot path is wasteful.
fn classify(frame: &SseFrame) -> FrameAction {
    let json = frame.json();
    let operation_id = || {
        json.get("operation_id")
            .and_then(|v| v.as_str())
            .map(String::from)
    };

    let event = frame
        .event
        .clone()
        .or_else(|| json.get("event").and_then(|v| v.as_str()).map(String::from));
    let Some(event) = event else {
        return FrameAction::Ignore;
    };
    if event == "stream_gap" {
        return FrameAction::Gap;
    }
    if let Some(family) = event.strip_suffix("_started") {
        return operation_id().map_or(FrameAction::Ignore, |operation_id| FrameAction::Started {
            family: family.to_string(),
            operation_id,
            max_duration_ms: json.get("max_duration_ms").and_then(Value::as_u64),
        });
    }
    if event.ends_with("_complete") || event.ends_with("_failed") {
        return operation_id().map_or(FrameAction::Ignore, |operation_id| FrameAction::Ended {
            operation_id,
        });
    }
    FrameAction::Ignore
}

/// Source of watchdog events. Abstracted so the deadline-tracking logic can
/// be unit-tested against scripted frames without a real HTTP server.
#[async_trait]
pub trait WatchdogEventSource: Send + Sync + std::fmt::Debug {
    /// Open the stream, resuming after `last_event_id` if given. On success
    /// returns a receiver of frames; when the stream ends (rp closed the
    /// body, or a transport error) the sender is dropped and the receiver
    /// yields `None`.
    async fn connect(&self, last_event_id: Option<u64>) -> crate::Result<mpsc::Receiver<SseFrame>>;
}

/// Production event source: a long-lived `GET /api/events/subscribe` read
/// chunk by chunk with [`reqwest::Response::chunk`] (no `stream` cargo
/// feature needed) and parsed into [`SseFrame`]s.
#[derive(Debug)]
pub struct HttpWatchdogEventSource {
    url: String,
    client: reqwest::Client,
}

impl HttpWatchdogEventSource {
    #[must_use]
    pub fn new(rp_url: &str) -> Self {
        let url = format!("{}/api/events/subscribe", rp_url.trim_end_matches('/'));
        Self {
            url,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl WatchdogEventSource for HttpWatchdogEventSource {
    async fn connect(&self, last_event_id: Option<u64>) -> crate::Result<mpsc::Receiver<SseFrame>> {
        let mut req = self
            .client
            .get(&self.url)
            .header("accept", "text/event-stream");
        if let Some(id) = last_event_id {
            req = req.header("last-event-id", id.to_string());
        }
        let resp = req.send().await.map_err(|e| {
            crate::SentinelError::Http(format!("subscribe {} failed: {e}", self.url))
        })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(crate::SentinelError::Http(format!(
                "subscribe {} -> {}",
                self.url,
                status.as_u16()
            )));
        }

        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            let mut resp = resp;
            let mut buffer = String::new();
            loop {
                tokio::select! {
                    // Receiver dropped (monitor cancelled, or reconnecting):
                    // stop immediately and drop `resp` so the HTTP connection
                    // closes — otherwise this task could block on `chunk()`
                    // forever on a quiet stream, leaking the task + connection.
                    () = tx.closed() => return,
                    chunk = resp.chunk() => match chunk {
                        Ok(Some(chunk)) => {
                            buffer.push_str(&String::from_utf8_lossy(&chunk));
                            for frame in drain_frames(&mut buffer) {
                                if tx.send(frame).await.is_err() {
                                    return; // consumer dropped
                                }
                            }
                        }
                        // Stream ended (None) or transport error: dropping `tx`
                        // makes the receiver yield `None`.
                        _ => return,
                    },
                }
            }
        });
        Ok(rx)
    }
}

/// An operation the watchdog is currently tracking.
#[derive(Debug)]
struct Tracked {
    family: String,
    started: Instant,
    /// Absolute expiry instant, or `None` when the `*_started` carried no
    /// `max_duration_ms` (tracked open, no timer).
    deadline: Option<Instant>,
}

/// Outcome of one connected session's frame-consumption loop.
enum ConsumeOutcome {
    /// The cancellation token fired — shut down.
    Cancelled,
    /// The stream ended — reconnect.
    Disconnected,
}

/// Tracks rp operation deadlines from its event stream and escalates misses.
#[derive(Debug)]
pub struct OperationDeadlineMonitor {
    name: String,
    source: Arc<dyn WatchdogEventSource>,
    notifiers: Vec<Arc<dyn Notifier>>,
    state: StateHandle,
    config: OperationWatchdogConfig,
    /// The discovered-services registry, which `operations.<family>.service`
    /// references by name.
    services: crate::discovery::ServiceRegistry,
    /// Corrective-action ladder, run on expiry for `abort_then_restart`
    /// families. Never invoked for `notify_only` or liveness triggers.
    corrective: Arc<dyn Corrective>,
}

impl OperationDeadlineMonitor {
    pub fn new(
        name: impl Into<String>,
        source: Arc<dyn WatchdogEventSource>,
        notifiers: Vec<Arc<dyn Notifier>>,
        state: StateHandle,
        config: OperationWatchdogConfig,
        services: crate::discovery::ServiceRegistry,
        corrective: Arc<dyn Corrective>,
    ) -> Self {
        Self {
            name: name.into(),
            source,
            notifiers,
            state,
            config,
            services,
            corrective,
        }
    }

    /// Resolve the corrective target for a family: its configured `service`
    /// must be a discovered service. `None` for unconfigured / undiscovered
    /// families (the caller degrades to notify-only).
    async fn resolve_target(&self, family: &str) -> Option<CorrectiveTarget> {
        let service_name = self.config.operations.get(family)?.service.as_deref()?;
        self.services
            .read()
            .await
            .get(service_name)
            .map(|service| CorrectiveTarget::new(family, service))
    }

    /// Run the corrective ladder for an expired family and return the message
    /// suffix describing what ran. `notify_only` — and an `abort_then_restart`
    /// family with no resolvable `service` — produce an empty suffix.
    async fn corrective_action(&self, family: &str) -> String {
        if self.on_expiry_for(family) != OnExpiry::AbortThenRestart {
            return String::new();
        }
        if let Some(target) = self.resolve_target(family).await {
            debug!(
                "watchdog '{}' running corrective ladder for {} via service '{}'",
                self.name, family, target.service_name
            );
            self.corrective.run(&target).await.action_suffix()
        } else {
            warn!(
                "watchdog '{}' family '{}' is abort_then_restart but has no resolvable \
                 service; notifying only",
                self.name, family
            );
            String::new()
        }
    }

    /// Buffer added to a family's `max_duration_ms`, falling back to the
    /// watchdog default.
    fn buffer_for(&self, family: &str) -> Duration {
        self.config
            .operations
            .get(family)
            .and_then(|p| p.buffer)
            .unwrap_or(self.config.default_buffer)
    }

    /// Corrective-action policy for a family. Phase 4 always notifies; this
    /// is read so Phase 5 can branch on it without a signature change.
    fn on_expiry_for(&self, family: &str) -> OnExpiry {
        self.config
            .operations
            .get(family)
            .map(|p| p.on_expiry)
            .unwrap_or_default()
    }

    /// Consume frames from one connected session, managing per-operation
    /// deadline timers, until the stream ends or `cancel` fires.
    async fn consume(
        &self,
        rx: &mut mpsc::Receiver<SseFrame>,
        tracked: &mut HashMap<String, Tracked>,
        last_seq: &mut Option<u64>,
        cancel: &CancellationToken,
    ) -> ConsumeOutcome {
        loop {
            let next_deadline = tracked.values().filter_map(|t| t.deadline).min();
            let timer = async {
                match next_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            };

            tokio::select! {
                biased;
                () = cancel.cancelled() => return ConsumeOutcome::Cancelled,
                maybe = rx.recv() => match maybe {
                    Some(frame) => self.handle_frame(frame, tracked, last_seq).await,
                    None => return ConsumeOutcome::Disconnected,
                },
                () = timer => self.fire_expired(tracked).await,
            }
        }
    }

    /// Apply one frame to the tracking map.
    async fn handle_frame(
        &self,
        frame: SseFrame,
        tracked: &mut HashMap<String, Tracked>,
        last_seq: &mut Option<u64>,
    ) {
        if let Some(id) = frame.id {
            *last_seq = Some(last_seq.map_or(id, |cur| cur.max(id)));
        }
        match classify(&frame) {
            FrameAction::Started {
                family,
                operation_id,
                max_duration_ms,
            } => {
                // One `now` for both the deadline and `started`, so the
                // tracked start and its expiry are anchored to the same instant.
                let started = Instant::now();
                // An expiry too far out to represent is an operation the
                // deadline can never fire for, so overflow (`None`)
                // degrades to tracking it as untimed — loudly, because a
                // supplied max_duration_ms silently losing its expiry
                // would be hard to diagnose (buggy emitter, wrong units).
                let deadline = max_duration_ms.and_then(|ms| {
                    let at = started.checked_add(
                        Duration::from_millis(ms).saturating_add(self.buffer_for(&family)),
                    );
                    if at.is_none() {
                        warn!(
                            "watchdog '{}' {} op {}: max_duration_ms={} overflows the \
                             deadline clock; tracking as untimed",
                            self.name, family, operation_id, ms
                        );
                    }
                    at
                });
                debug!(
                    "watchdog '{}' tracking {} op {} (timed={})",
                    self.name,
                    family,
                    operation_id,
                    deadline.is_some()
                );
                tracked.insert(
                    operation_id,
                    Tracked {
                        family,
                        started,
                        deadline,
                    },
                );
            }
            FrameAction::Ended { operation_id } => {
                if tracked.remove(&operation_id).is_some() {
                    debug!(
                        "watchdog '{}' op {} completed in time",
                        self.name, operation_id
                    );
                }
            }
            FrameAction::Gap => {
                // Lost history: every open operation is now unconfirmed.
                let open: Vec<(String, Tracked)> = tracked.drain().collect();
                for (operation_id, t) in open {
                    let elapsed = t.started.elapsed();
                    self.escalate(
                        &t.family,
                        &operation_id,
                        elapsed,
                        "is unconfirmed after an event-stream gap",
                        "",
                    )
                    .await;
                }
            }
            FrameAction::Ignore => {}
        }
    }

    /// Escalate (and stop tracking) every operation whose deadline has passed.
    async fn fire_expired(&self, tracked: &mut HashMap<String, Tracked>) {
        let now = Instant::now();
        let expired: Vec<String> = tracked
            .iter()
            .filter(|(_, t)| t.deadline.is_some_and(|d| d <= now))
            .map(|(id, _)| id.clone())
            .collect();
        for operation_id in expired {
            if let Some(t) = tracked.remove(&operation_id) {
                let elapsed = t.started.elapsed();
                // Run the corrective ladder (abort_then_restart) before
                // notifying, so the alert reports what was attempted.
                let action = self.corrective_action(&t.family).await;
                self.escalate(
                    &t.family,
                    &operation_id,
                    elapsed,
                    "exceeded its deadline",
                    &action,
                )
                .await;
            }
        }
    }

    /// Dispatch one escalation through the configured notifier chain and
    /// record it in the dashboard notification history. `action` is the
    /// corrective-ladder summary (empty for notify-only / liveness triggers).
    async fn escalate(
        &self,
        operation: &str,
        operation_id: &str,
        elapsed: Duration,
        reason: &str,
        action: &str,
    ) {
        let message = self
            .config
            .message_template
            .replace("%operation%", operation)
            .replace("%operation_id%", operation_id)
            .replace(
                "%elapsed%",
                &humantime::format_duration(elapsed).to_string(),
            )
            .replace("%reason%", reason)
            .replace("%action%", action);

        warn!("watchdog '{}' escalation: {}", self.name, message);

        let notification = Notification {
            title: "Observatory Watchdog".to_string(),
            message: message.clone(),
            priority: 0,
            sound: None,
        };
        let now_ms = current_epoch_ms();

        for notifier in &self.notifiers {
            // Empty `notifiers` selection means "every configured notifier".
            if !self.config.notifiers.is_empty()
                && !self
                    .config
                    .notifiers
                    .iter()
                    .any(|n| n == notifier.type_name())
            {
                continue;
            }
            let result = notifier.notify(&notification).await;
            if let Err(e) = &result {
                warn!(
                    "watchdog notification via '{}' failed: {}",
                    notifier.type_name(),
                    e
                );
            }
            let record = NotificationRecord {
                monitor_name: self.name.clone(),
                notifier_type: notifier.type_name().to_string(),
                message: message.clone(),
                success: result.is_ok(),
                error: result.as_ref().err().map(std::string::ToString::to_string),
                timestamp_epoch_ms: now_ms,
            };
            self.state.write().await.add_notification(record);
        }
    }
}

#[async_trait]
impl EventMonitor for OperationDeadlineMonitor {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, cancel: CancellationToken) {
        let mut tracked: HashMap<String, Tracked> = HashMap::new();
        // Carries across reconnects: the deadlines are absolute, so an
        // operation still open after a reconnect keeps its original timer.
        let mut last_seq: Option<u64> = None;
        let mut attempts: u32 = 0;

        debug!("watchdog '{}' starting", self.name);
        loop {
            if cancel.is_cancelled() {
                return;
            }
            match self.source.connect(last_seq).await {
                Ok(mut rx) => {
                    attempts = 0;
                    debug!(
                        "watchdog '{}' connected (resume after {:?})",
                        self.name, last_seq
                    );
                    match self
                        .consume(&mut rx, &mut tracked, &mut last_seq, &cancel)
                        .await
                    {
                        ConsumeOutcome::Cancelled => return,
                        ConsumeOutcome::Disconnected => {
                            debug!("watchdog '{}' stream disconnected", self.name);
                        }
                    }
                }
                Err(e) => {
                    attempts = attempts.saturating_add(1);
                    warn!(
                        "watchdog '{}' connect attempt {} failed: {}",
                        self.name, attempts, e
                    );
                    if self.config.reconnect_max_attempts != 0
                        && attempts >= self.config.reconnect_max_attempts
                    {
                        self.escalate(
                            "rp",
                            "-",
                            Duration::ZERO,
                            "is unresponsive (event stream unreachable)",
                            "",
                        )
                        .await;
                        // Reset so a recovered rp resumes tracking and a
                        // still-dead rp re-alerts after another N attempts.
                        attempts = 0;
                    }
                }
            }

            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(self.config.reconnect_backoff) => {}
            }
        }
    }
}

/// Extract every complete SSE frame (`\n\n`-delimited) from `buffer`, leaving
/// any trailing partial frame for the next chunk.
fn drain_frames(buffer: &mut String) -> Vec<SseFrame> {
    let mut out = Vec::new();
    while let Some(idx) = buffer.find("\n\n") {
        let block: String = buffer.drain(..idx.saturating_add(2)).collect();
        if let Some(frame) = parse_frame(&block) {
            out.push(frame);
        }
    }
    out
}

/// Parse one `\n\n`-delimited SSE block. Comment lines (`:`-prefixed
/// keep-alives) are skipped; a block with no `event` and no `data` yields
/// `None`.
fn parse_frame(block: &str) -> Option<SseFrame> {
    let mut id = None;
    let mut event = None;
    let mut data_lines: Vec<String> = Vec::new();
    for line in block.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "id" => id = value.trim().parse::<u64>().ok(),
            "event" => event = Some(value.to_string()),
            "data" => data_lines.push(value.to_string()),
            _ => {}
        }
    }
    if event.is_none() && data_lines.is_empty() {
        return None;
    }
    Some(SseFrame {
        id,
        event,
        data: data_lines.join("\n"),
    })
}

fn current_epoch_ms() -> u64 {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    // Overflows u64 in the year 584556019. Saturate rather than wrap.
    u64::try_from(ms).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use crate::state::new_state_handle;

    // ---- SSE parser ----------------------------------------------------

    #[test]
    fn parses_a_complete_frame() {
        let mut buf =
            "event: slew_started\nid: 12\ndata: {\"event_seq\":12,\"operation_id\":\"op-1\"}\n\n"
                .to_string();
        let frames = drain_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id, Some(12));
        assert_eq!(
            classify(&frames[0]),
            FrameAction::Started {
                family: "slew".to_string(),
                operation_id: "op-1".to_string(),
                max_duration_ms: None,
            }
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn keeps_partial_frame_for_next_chunk() {
        let mut buf = "event: slew_started\nid: 7\n".to_string();
        assert!(drain_frames(&mut buf).is_empty());
        buf.push_str("data: {\"event_seq\":7}\n\n");
        assert_eq!(drain_frames(&mut buf).len(), 1);
    }

    #[test]
    fn skips_keep_alive_comment() {
        let mut buf =
            ":keep-alive\n\nevent: park_started\nid: 3\ndata: {\"event_seq\":3}\n\n".to_string();
        let frames = drain_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("park_started"));
    }

    // ---- classification ------------------------------------------------

    fn frame(seq: u64, event: &str, op: Option<&str>, max_ms: Option<u64>) -> SseFrame {
        let mut data = serde_json::json!({ "event_seq": seq, "event": event });
        if let Some(op) = op {
            data["operation_id"] = serde_json::json!(op);
        }
        if let Some(ms) = max_ms {
            data["max_duration_ms"] = serde_json::json!(ms);
        }
        SseFrame {
            id: Some(seq),
            event: Some(event.to_string()),
            data: data.to_string(),
        }
    }

    #[test]
    fn classify_started_strips_suffix_to_family() {
        let action = classify(&frame(1, "move_focuser_started", Some("op-1"), Some(5000)));
        assert_eq!(
            action,
            FrameAction::Started {
                family: "move_focuser".to_string(),
                operation_id: "op-1".to_string(),
                max_duration_ms: Some(5000),
            }
        );
    }

    #[test]
    fn classify_complete_and_failed_are_ended() {
        assert_eq!(
            classify(&frame(2, "slew_complete", Some("op-1"), None)),
            FrameAction::Ended {
                operation_id: "op-1".to_string()
            }
        );
        assert_eq!(
            classify(&frame(3, "slew_failed", Some("op-1"), None)),
            FrameAction::Ended {
                operation_id: "op-1".to_string()
            }
        );
    }

    #[test]
    fn classify_ignores_started_without_operation_id_and_progress_events() {
        assert_eq!(
            classify(&frame(4, "slew_started", None, Some(5000))),
            FrameAction::Ignore
        );
        assert_eq!(
            classify(&frame(5, "centering_iteration", Some("op-1"), None)),
            FrameAction::Ignore
        );
    }

    #[test]
    fn classify_detects_stream_gap() {
        let gap = SseFrame {
            id: None,
            event: Some("stream_gap".to_string()),
            data: r#"{"event":"stream_gap","lagged":44}"#.to_string(),
        };
        assert_eq!(classify(&gap), FrameAction::Gap);
    }

    // ---- monitor behavior ----------------------------------------------

    /// One scripted connection: a batch of frames, optionally followed by a
    /// disconnect, or a connect failure.
    #[derive(Debug)]
    enum Script {
        /// Deliver these frames, then keep the stream open (no disconnect).
        FramesOpen(Vec<SseFrame>),
        /// Deliver these frames, then disconnect (receiver yields `None`).
        FramesThenClose(Vec<SseFrame>),
        /// Fail to connect.
        Fail,
    }

    #[derive(Debug, Default)]
    struct MockSource {
        scripts: Mutex<VecDeque<Script>>,
        connects: Mutex<Vec<Option<u64>>>,
        /// Senders kept alive so "open" streams never disconnect.
        keepalive: Mutex<Vec<mpsc::Sender<SseFrame>>>,
    }

    impl MockSource {
        fn new(scripts: Vec<Script>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Mutex::new(scripts.into()),
                connects: Mutex::new(Vec::new()),
                keepalive: Mutex::new(Vec::new()),
            })
        }

        fn connect_log(&self) -> Vec<Option<u64>> {
            self.connects.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl WatchdogEventSource for MockSource {
        async fn connect(
            &self,
            last_event_id: Option<u64>,
        ) -> crate::Result<mpsc::Receiver<SseFrame>> {
            self.connects.lock().unwrap().push(last_event_id);
            let script = self.scripts.lock().unwrap().pop_front();
            match script {
                Some(Script::Fail) => Err(crate::SentinelError::Http(
                    "mock connect failed".to_string(),
                )),
                Some(Script::FramesThenClose(frames)) => {
                    let (tx, rx) = mpsc::channel(256);
                    for f in frames {
                        let _ = tx.try_send(f);
                    }
                    drop(tx); // disconnect
                    Ok(rx)
                }
                // FramesOpen, or scripts exhausted: keep the sender alive so
                // the stream stays open until the monitor is cancelled.
                other => {
                    let (tx, rx) = mpsc::channel(256);
                    if let Some(Script::FramesOpen(frames)) = other {
                        for f in frames {
                            let _ = tx.try_send(f);
                        }
                    }
                    self.keepalive.lock().unwrap().push(tx);
                    Ok(rx)
                }
            }
        }
    }

    /// Records every corrective target it is asked to act on, and returns a
    /// fixed action suffix so the escalation message can be asserted.
    #[derive(Debug, Default)]
    struct RecordingCorrective {
        targets: Arc<Mutex<Vec<CorrectiveTarget>>>,
    }

    #[async_trait]
    impl Corrective for RecordingCorrective {
        async fn run(&self, target: &CorrectiveTarget) -> crate::corrective::LadderOutcome {
            self.targets.lock().unwrap().push(target.clone());
            let mut outcome = crate::corrective::LadderOutcome::default();
            outcome.rungs.push("health=responsive".to_string());
            outcome.rungs.push("abort=ok".to_string());
            outcome
        }
    }

    #[derive(Debug, Default)]
    struct RecordingNotifier {
        messages: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Notifier for RecordingNotifier {
        fn type_name(&self) -> &'static str {
            "recording"
        }
        async fn notify(&self, notification: &Notification) -> crate::Result<()> {
            self.messages
                .lock()
                .unwrap()
                .push(notification.message.clone());
            Ok(())
        }
    }

    fn test_config(default_buffer_secs: u64, max_attempts: u32) -> OperationWatchdogConfig {
        let json = format!(
            r#"{{
                "rp_url": "http://unused",
                "reconnect_max_attempts": {max_attempts},
                "reconnect_backoff": "1s",
                "default_buffer": "{default_buffer_secs}s",
                "message_template": "%operation%/%operation_id% %reason% after %elapsed%%action%",
                "operations": {{ "slew": {{ "buffer": "0s" }} }}
            }}"#
        );
        serde_json::from_str(&json).unwrap()
    }

    /// Build a monitor over the mock source; returns it, the shared message
    /// log, and the recording corrective ladder (to assert what it was asked
    /// to act on).
    fn build_monitor(
        source: Arc<MockSource>,
        config: OperationWatchdogConfig,
    ) -> (
        OperationDeadlineMonitor,
        Arc<Mutex<Vec<String>>>,
        Arc<RecordingCorrective>,
    ) {
        build_monitor_with_services(source, config, std::collections::HashMap::new())
    }

    /// [`build_monitor`] plus a discovered-services registry (ladder tests).
    fn build_monitor_with_services(
        source: Arc<MockSource>,
        config: OperationWatchdogConfig,
        services: std::collections::HashMap<String, crate::discovery::DiscoveredService>,
    ) -> (
        OperationDeadlineMonitor,
        Arc<Mutex<Vec<String>>>,
        Arc<RecordingCorrective>,
    ) {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let notifier = Arc::new(RecordingNotifier {
            messages: Arc::clone(&messages),
        });
        let corrective = Arc::new(RecordingCorrective::default());
        let state = new_state_handle(vec![], 100);
        let monitor = OperationDeadlineMonitor::new(
            "Test Watchdog",
            source,
            vec![notifier],
            state,
            config,
            Arc::new(tokio::sync::RwLock::new(services)),
            Arc::<RecordingCorrective>::clone(&corrective),
        );
        (monitor, messages, corrective)
    }

    /// Yield repeatedly so spawned tasks make progress under paused time.
    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn operation_completing_in_time_does_not_escalate() {
        let source = MockSource::new(vec![Script::FramesOpen(vec![
            frame(1, "slew_started", Some("op-1"), Some(5000)),
            frame(2, "slew_complete", Some("op-1"), None),
        ])]);
        let (monitor, messages, _corrective) = build_monitor(source, test_config(10, 5));
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move { monitor.run(cancel2).await });

        settle().await;
        tokio::time::advance(Duration::from_mins(2)).await;
        settle().await;

        assert!(
            messages.lock().unwrap().is_empty(),
            "a completed operation must not escalate: {:?}",
            messages.lock().unwrap()
        );
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn operation_overrunning_deadline_escalates_once() {
        let source = MockSource::new(vec![Script::FramesOpen(vec![frame(
            1,
            "slew_started",
            Some("op-1"),
            Some(5000), // 5 s + slew buffer 0 s => 5 s deadline
        )])]);
        let (monitor, messages, _corrective) = build_monitor(source, test_config(10, 5));
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move { monitor.run(cancel2).await });

        settle().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        settle().await;

        let msgs = messages.lock().unwrap().clone();
        assert_eq!(msgs.len(), 1, "exactly one escalation expected: {msgs:?}");
        assert!(msgs[0].contains("slew/op-1"), "{}", msgs[0]);
        assert!(msgs[0].contains("exceeded its deadline"), "{}", msgs[0]);
        assert!(
            msgs[0].ends_with("after 6s"),
            "elapsed must render as humantime: {}",
            msgs[0]
        );
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn escalation_renders_elapsed_as_multi_unit_humantime() {
        let source = MockSource::new(vec![Script::FramesOpen(vec![frame(
            1,
            "slew_started",
            Some("op-1"),
            Some(65_000), // 65 s + slew buffer 0 s => 65 s deadline
        )])]);
        let (monitor, messages, _corrective) = build_monitor(source, test_config(10, 5));
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move { monitor.run(cancel2).await });

        settle().await;
        tokio::time::advance(Duration::from_secs(66)).await;
        settle().await;

        let msgs = messages.lock().unwrap().clone();
        assert_eq!(msgs.len(), 1, "exactly one escalation expected: {msgs:?}");
        assert_eq!(msgs[0], "slew/op-1 exceeded its deadline after 1m 6s");
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn started_without_max_duration_is_tracked_but_never_times_out() {
        let source = MockSource::new(vec![Script::FramesOpen(vec![frame(
            1,
            "plate_solve_started",
            Some("op-1"),
            None, // no max_duration_ms -> no timer
        )])]);
        let (monitor, messages, _corrective) = build_monitor(source, test_config(10, 5));
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move { monitor.run(cancel2).await });

        settle().await;
        tokio::time::advance(Duration::from_hours(1)).await;
        settle().await;

        assert!(
            messages.lock().unwrap().is_empty(),
            "an untimed operation must not escalate on its own"
        );
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn stream_gap_escalates_every_open_operation() {
        let gap = SseFrame {
            id: None,
            event: Some("stream_gap".to_string()),
            data: r#"{"event":"stream_gap"}"#.to_string(),
        };
        let source = MockSource::new(vec![Script::FramesOpen(vec![
            frame(1, "slew_started", Some("op-1"), Some(600_000)),
            frame(2, "park_started", Some("op-2"), Some(600_000)),
            gap,
        ])]);
        let (monitor, messages, _corrective) = build_monitor(source, test_config(10, 5));
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move { monitor.run(cancel2).await });

        settle().await;

        let msgs = messages.lock().unwrap().clone();
        assert_eq!(msgs.len(), 2, "both open ops escalate on a gap: {msgs:?}");
        assert!(
            msgs.iter().all(|m| m.contains("event-stream gap")),
            "{msgs:?}"
        );
        assert!(msgs.iter().any(|m| m.contains("op-1")));
        assert!(msgs.iter().any(|m| m.contains("op-2")));
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_resumes_tracking_and_replays_completion() {
        let source = MockSource::new(vec![
            Script::FramesThenClose(vec![frame(1, "slew_started", Some("op-1"), Some(5000))]),
            // Reconnect replays the completion that arrived during the gap.
            Script::FramesOpen(vec![frame(2, "slew_complete", Some("op-1"), None)]),
        ]);
        let (monitor, messages, _corrective) =
            build_monitor(Arc::clone(&source), test_config(10, 5));
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move { monitor.run(cancel2).await });

        settle().await;
        tokio::time::advance(Duration::from_secs(2)).await; // past reconnect backoff
        settle().await;
        tokio::time::advance(Duration::from_mins(2)).await; // past the original deadline
        settle().await;

        assert!(
            messages.lock().unwrap().is_empty(),
            "completion replayed on reconnect must clear the op: {:?}",
            messages.lock().unwrap()
        );
        let connects = source.connect_log();
        assert_eq!(connects.len(), 2, "expected one reconnect: {connects:?}");
        assert_eq!(
            connects[1],
            Some(1),
            "reconnect must resume after the last seen event_seq"
        );
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn exhausting_reconnects_escalates_rp_unresponsive() {
        let source = MockSource::new(vec![Script::Fail, Script::Fail]);
        let (monitor, messages, _corrective) = build_monitor(source, test_config(10, 2));
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move { monitor.run(cancel2).await });

        // Two failed connects (backoff 1 s between) then escalate.
        for _ in 0..5 {
            settle().await;
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        settle().await;

        let msgs = messages.lock().unwrap().clone();
        assert!(
            msgs.iter().any(|m| m.contains("unresponsive")),
            "rp unresponsive escalation expected: {msgs:?}"
        );
        cancel.cancel();
        handle.await.unwrap();
    }

    // ---- corrective-ladder branching -----------------------------------

    /// A config whose `slew` family runs the corrective ladder against a
    /// `mount` service on expiry.
    fn ladder_config() -> OperationWatchdogConfig {
        let json = r#"{
            "rp_url": "http://unused",
            "reconnect_max_attempts": 5,
            "reconnect_backoff": "1s",
            "default_buffer": "0s",
            "message_template": "%operation%/%operation_id% %reason% after %elapsed%%action%",
            "operations": {
                "slew": { "buffer": "0s", "on_expiry": "abort_then_restart", "service": "mount" }
            }
        }"#;
        serde_json::from_str(json).unwrap()
    }

    /// The discovered-services registry [`ladder_config`]'s `slew` references.
    fn ladder_services() -> std::collections::HashMap<String, crate::discovery::DiscoveredService> {
        std::collections::HashMap::from([(
            "mount".to_string(),
            crate::discovery::DiscoveredService {
                name: "mount".to_string(),
                unit: "rusty-photon-mount".to_string(),
                state: crate::discovery::RunState::Running,
                probe: Some(crate::discovery::ProbeSpec {
                    health_url: "http://mount/management/v1/configureddevices".to_string(),
                    alpaca_base: "http://mount/api/v1".to_string(),
                    port: 80,
                }),
            },
        )])
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_with_abort_then_restart_runs_corrective_ladder() {
        let source = MockSource::new(vec![Script::FramesOpen(vec![frame(
            1,
            "slew_started",
            Some("op-1"),
            Some(5000),
        )])]);
        let (monitor, messages, corrective) =
            build_monitor_with_services(source, ladder_config(), ladder_services());
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move { monitor.run(cancel2).await });

        settle().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        settle().await;

        let targets = corrective.targets.lock().unwrap().clone();
        assert_eq!(targets.len(), 1, "the ladder must run once on expiry");
        assert_eq!(targets[0].service_name, "mount");
        assert_eq!(
            targets[0].binding.unwrap().device_type,
            "telescope",
            "slew resolves to the telescope device"
        );

        let msgs = messages.lock().unwrap().clone();
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(
            msgs[0].contains("corrective action: health=responsive, abort=ok"),
            "escalation must report the ladder outcome: {}",
            msgs[0]
        );
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn abort_then_restart_without_resolvable_service_degrades_to_notify() {
        // `slew` is abort_then_restart but names no `service` -> notify only.
        let mut config = ladder_config();
        config.operations.get_mut("slew").unwrap().service = None;
        let source = MockSource::new(vec![Script::FramesOpen(vec![frame(
            1,
            "slew_started",
            Some("op-1"),
            Some(5000),
        )])]);
        let (monitor, messages, corrective) =
            build_monitor_with_services(source, config, ladder_services());
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move { monitor.run(cancel2).await });

        settle().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        settle().await;

        assert!(
            corrective.targets.lock().unwrap().is_empty(),
            "an unresolvable service must not invoke the ladder"
        );
        let msgs = messages.lock().unwrap().clone();
        assert_eq!(msgs.len(), 1, "still notifies: {msgs:?}");
        assert!(
            !msgs[0].contains("corrective action"),
            "no corrective action ran: {}",
            msgs[0]
        );
        cancel.cancel();
        handle.await.unwrap();
    }
}
