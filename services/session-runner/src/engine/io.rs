//! The engine's outward-facing seams: the tool-call client, the clock, and
//! the event intake.
//!
//! All three are injectable so the interpreter is testable without `rp`:
//! unit tests drive the tree with a scripted [`ToolClient`], a mock
//! [`Clock`], and a hand-fed [`EventIntake`] (design:
//! `docs/services/session-runner.md` § Testing Strategy). The real
//! implementations are the `rmcp`-based MCP client, [`SystemClock`], and
//! the SSE client in `crate::events`.

use std::future::Future;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

/// A failed tool call, as the engine distinguishes them.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToolCallError {
    /// The tool ran and failed, or the call itself failed. Retryable via
    /// the instruction's `retry` policy; catchable by `try`.
    #[error("{0}")]
    Failed(String),
    /// `rp` terminated the MCP session (safety, per the design's § Safety
    /// Behavior). Never retried and never caught: the engine runs
    /// enclosing `finally` blocks best-effort and exits the run without
    /// posting a completion.
    #[error("MCP session terminated: {0}")]
    SessionTerminated(String),
}

/// The engine's view of `rp`'s MCP server.
pub trait ToolClient {
    /// Call `tool` with the given JSON-object arguments and return its
    /// structured result.
    fn call(
        &self,
        tool: &str,
        args: Map<String, Value>,
    ) -> impl Future<Output = Result<Value, ToolCallError>> + Send;
}

/// The engine clock — one seam for the engine's time needs, so engine
/// tests are deterministic and instant.
///
/// It covers "what time is it" (the expression context's `now`, read by
/// `seconds_until()`), "pause" (`wait` durations, poll intervals, retry
/// backoff), and "how long did that take" (the monotonic reading that
/// measures `wait` timeout budgets).
pub trait Clock {
    fn now(&self) -> DateTime<Utc>;
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send;
    /// A monotonic reading for measuring elapsed waits. Only differences
    /// are meaningful; the reference point is arbitrary but fixed for the
    /// clock's lifetime. Monotonic by contract: a wall-clock adjustment
    /// (NTP step) must not move it (design § `wait`).
    fn monotonic(&self) -> Duration;
}

/// The production clock: `chrono::Utc::now` + `tokio::time::sleep` +
/// `std::time::Instant` against a process-lifetime epoch.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    fn monotonic(&self) -> Duration {
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        EPOCH.get_or_init(Instant::now).elapsed()
    }
}

/// One event as the engine consumes it: the envelope's event-type name
/// plus its `payload` (which becomes the `event.*` namespace in trigger
/// scopes).
///
/// Produced by the SSE client (`crate::events`) and, for the synthetic
/// `correction_requested` source, by the engine itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineEvent {
    pub event: String,
    pub payload: Value,
}

/// The engine's intake of `rp` events, running from before the first
/// instruction so nothing emitted mid-session is missed.
///
/// Events received while an instruction runs stay buffered until the
/// engine next looks.
///
/// A closed or absent stream is not an error — [`EventIntake::next`]
/// simply never resolves, so an `until_event` wait runs to its timeout
/// and event triggers never fire (poll triggers keep running), matching
/// the design's stance that events can always be missed across an outage.
#[derive(Debug)]
pub struct EventIntake {
    /// `None` once the sending side is gone (or for
    /// [`EventIntake::disconnected`]) — nothing will ever arrive.
    rx: Option<mpsc::Receiver<EngineEvent>>,
}

impl EventIntake {
    #[must_use]
    pub const fn new(rx: mpsc::Receiver<EngineEvent>) -> Self {
        Self { rx: Some(rx) }
    }

    /// An intake with no stream behind it: nothing ever arrives.
    #[must_use]
    pub const fn disconnected() -> Self {
        Self { rx: None }
    }

    /// The next buffered event, without waiting; `None` when the buffer
    /// is empty (or the stream is gone).
    pub(crate) fn try_next(&mut self) -> Option<EngineEvent> {
        use tokio::sync::mpsc::error::TryRecvError;
        match self.rx.as_mut()?.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                None
            }
        }
    }

    /// Wait for the next event. Resolves **only** when an event arrives:
    /// on a closed stream it pends forever, so callers select this
    /// against a sleep without needing a closed-stream branch.
    pub(crate) async fn next(&mut self) -> EngineEvent {
        if let Some(rx) = self.rx.as_mut() {
            if let Some(event) = rx.recv().await {
                return event;
            }
            self.rx = None;
        }
        std::future::pending().await
    }
}
