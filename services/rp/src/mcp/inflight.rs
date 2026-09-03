//! The in-flight tool-call registry (rp.md § Safety → In-Flight Tool
//! Calls).
//!
//! Every `tools/call` is registered for its lifetime by the
//! `ServerHandler::call_tool` wrapper in [`super`] — keyed by an
//! rp-internal serial, because JSON-RPC request ids are only unique per
//! client session — together with the tool's [`ToolClass`] and a
//! [`Cancel`] handle derived from the request's own rmcp token. Tool
//! bodies pull the handle back out of the request context
//! ([`Cancel::from_context`]) and race their poll loops against it.
//!
//! Two things cancel a call:
//!
//! - the safety enforcer's unsafe transition, which cancels every
//!   **gated** entry and every in-flight `capture`
//!   ([`super::gate::cancelled_on_unsafe`]) through
//!   [`InFlight::cancel_for_safety`] and waits up to
//!   [`CANCEL_ACK_TIMEOUT`] for those bodies to unregister, so a
//!   cancelled slew's `AbortSlew` cannot land on the park that follows;
//! - the request's parent token, which rmcp cancels when the client's
//!   session closes or it sends `notifications/cancelled` — whatever
//!   the class, since the caller is gone either way.
//!
//! A cancelled body answers with the tool error `cancelled: <reason>`
//! ([`Cancel::error`]); the reason distinguishes the two.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rmcp::model::{Extensions, RequestId};
use rmcp::service::RequestContext;
use rmcp::RoleServer;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::gate::{self, ToolClass};

/// How long [`InFlight::cancel_for_safety`] waits for the cancelled
/// bodies to unregister before the caller proceeds regardless.
///
/// A body observes its token within one 100 ms poll tick and then
/// issues at most one stop command, so a wait past this bound means a
/// body is wedged on a device call — and the transition's own hardware
/// steps must not wait on that.
pub const CANCEL_ACK_TIMEOUT: Duration = Duration::from_secs(3);

/// Why an in-flight call was cancelled; rendered into the tool error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    /// The safety enforcer's unsafe transition.
    Safety,
    /// The caller's session closed, or it sent `notifications/cancelled`.
    ClientDisconnected,
}

impl fmt::Display for CancelReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Safety => "safety",
            Self::ClientDisconnected => "client disconnected",
        })
    }
}

/// A tool body's view of its own cancellation: a token plus the reason
/// it was cancelled for. Cheap to clone; every clone observes the same
/// cancellation.
#[derive(Clone)]
pub struct Cancel {
    inner: Arc<CancelInner>,
}

struct CancelInner {
    token: CancellationToken,
    /// Set by [`Cancel::cancel`] before the token fires. Unset when the
    /// cancellation came from the parent token instead — the client
    /// went away — which is what [`Cancel::reason`] reports then.
    reason: OnceLock<CancelReason>,
}

impl Cancel {
    /// A handle that is never cancelled — for unit tests and for tool
    /// bodies reached without a registered call.
    #[must_use]
    pub fn never() -> Self {
        Self::child_of(&CancellationToken::new())
    }

    /// A handle cancelled by [`Cancel::cancel`] or by `parent`.
    #[must_use]
    pub fn child_of(parent: &CancellationToken) -> Self {
        Self {
            inner: Arc::new(CancelInner {
                token: parent.child_token(),
                reason: OnceLock::new(),
            }),
        }
    }

    /// The handle the `call_tool` wrapper stored on the request, or a
    /// never-cancelled one when the body was reached some other way
    /// (unit tests drive `McpHandler` without an MCP transport).
    #[must_use]
    pub fn from_context(ctx: &RequestContext<RoleServer>) -> Self {
        Self::from_extensions(&ctx.extensions)
    }

    /// [`Cancel::from_context`] on the bare extension map.
    #[must_use]
    pub fn from_extensions(extensions: &Extensions) -> Self {
        extensions
            .get::<Self>()
            .cloned()
            .unwrap_or_else(Self::never)
    }

    /// Cancel with an explicit reason. The first reason wins; a later
    /// call is a no-op on an already-cancelled handle.
    pub fn cancel(&self, reason: CancelReason) {
        let _ = self.inner.reason.set(reason);
        self.inner.token.cancel();
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.token.is_cancelled()
    }

    /// Resolves once the handle is cancelled; never, otherwise.
    pub async fn cancelled(&self) {
        self.inner.token.cancelled().await;
    }

    /// The reason this handle was cancelled for. A cancellation that
    /// arrived through the parent token carries no explicit reason and
    /// reads as [`CancelReason::ClientDisconnected`]. Only meaningful
    /// after [`Cancel::is_cancelled`] is `true`.
    #[must_use]
    pub fn reason(&self) -> CancelReason {
        self.inner
            .reason
            .get()
            .copied()
            .unwrap_or(CancelReason::ClientDisconnected)
    }

    /// The tool error text a cancelled body answers with:
    /// `cancelled: <reason>`.
    #[must_use]
    pub fn error(&self) -> String {
        format!("cancelled: {}", self.reason())
    }
}

struct Entry {
    request_id: RequestId,
    tool: String,
    class: ToolClass,
    cancel: Cancel,
}

/// The registry. One per `McpHandler`, shared with the safety enforcer.
#[derive(Default)]
pub struct InFlight {
    entries: Mutex<HashMap<u64, Entry>>,
    next_key: AtomicU64,
    /// Signalled on every unregistration so
    /// [`InFlight::cancel_for_safety`] can wait for its acknowledgements
    /// without polling.
    removed: Notify,
}

impl InFlight {
    /// Enter a call for its lifetime. Returns the guard that
    /// unregisters it on drop and the [`Cancel`] handle for the body;
    /// the handle is a child of `parent` (the request's rmcp token).
    pub fn register(
        self: &Arc<Self>,
        request_id: &RequestId,
        tool: &str,
        class: ToolClass,
        parent: &CancellationToken,
    ) -> (Guard, Cancel) {
        let cancel = Cancel::child_of(parent);
        let key = self.next_key.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(
            key,
            Entry {
                request_id: request_id.clone(),
                tool: tool.to_string(),
                class,
                cancel: cancel.clone(),
            },
        );
        debug!(%request_id, tool, ?class, "tool call registered");
        (
            Guard {
                registry: Arc::clone(self),
                key,
            },
            cancel,
        )
    }

    /// Number of calls currently in flight.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The unsafe transition: cancel every entry
    /// [`gate::cancelled_on_unsafe`] selects — the gated calls and any
    /// in-flight `capture` — with [`CancelReason::Safety`], then wait,
    /// bounded by [`CANCEL_ACK_TIMEOUT`], for those bodies to
    /// unregister. Returns how many were cancelled. Every other entry
    /// is untouched.
    pub async fn cancel_for_safety(&self) -> usize {
        let cancelled: Vec<u64> = {
            let entries = self.lock();
            entries
                .iter()
                .filter(|(_, entry)| gate::cancelled_on_unsafe(&entry.tool, entry.class))
                .map(|(key, entry)| {
                    warn!(
                        request_id = %entry.request_id,
                        tool = %entry.tool,
                        "cancelling in-flight tool call on unsafe transition"
                    );
                    entry.cancel.cancel(CancelReason::Safety);
                    *key
                })
                .collect()
        };
        if cancelled.is_empty() {
            return 0;
        }
        let acknowledged = async {
            loop {
                // Arm the notification before checking, so an
                // unregistration between the check and the await is
                // not lost (`Notify::notify_waiters` wakes only
                // already-waiting or enabled futures).
                let notified = self.removed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !self.any_present(&cancelled) {
                    break;
                }
                notified.await;
            }
        };
        if tokio::time::timeout(CANCEL_ACK_TIMEOUT, acknowledged)
            .await
            .is_ok()
        {
            debug!(count = cancelled.len(), "cancelled tool calls acknowledged");
        } else {
            warn!(
                timeout = ?CANCEL_ACK_TIMEOUT,
                "cancelled tool calls did not all unregister in time; proceeding"
            );
        }
        cancelled.len()
    }

    fn any_present(&self, keys: &[u64]) -> bool {
        let entries = self.lock();
        keys.iter().any(|key| entries.contains_key(key))
    }

    fn remove(&self, key: u64) {
        let removed = self.lock().remove(&key);
        if let Some(entry) = removed {
            debug!(request_id = %entry.request_id, tool = %entry.tool, "tool call unregistered");
        }
        self.removed.notify_waiters();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Entry>> {
        // A poisoned lock only means another call panicked while
        // holding it; the map itself is still consistent.
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Unregisters its call on drop.
#[must_use = "dropping the guard unregisters the call immediately"]
pub struct Guard {
    registry: Arc<InFlight>,
    key: u64,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.registry.remove(self.key);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn registry() -> Arc<InFlight> {
        Arc::new(InFlight::default())
    }

    fn id(n: i64) -> RequestId {
        RequestId::Number(n)
    }

    #[test]
    fn register_enters_and_dropping_the_guard_leaves() {
        let registry = registry();
        let parent = CancellationToken::new();
        let (guard, cancel) = registry.register(&id(1), "slew", ToolClass::Gated, &parent);
        assert_eq!(registry.len(), 1);
        assert!(!cancel.is_cancelled());
        drop(guard);
        assert!(registry.is_empty());
    }

    #[test]
    fn two_sessions_using_the_same_request_id_do_not_collide() {
        let registry = registry();
        let parent = CancellationToken::new();
        let (_a, _) = registry.register(&id(1), "slew", ToolClass::Gated, &parent);
        let (_b, _) = registry.register(&id(1), "park", ToolClass::Ungated, &parent);
        assert_eq!(registry.len(), 2);
    }

    #[tokio::test]
    async fn cancel_for_safety_cancels_gated_entries_with_the_safety_reason() {
        let registry = registry();
        let parent = CancellationToken::new();
        let (slew_guard, slew) = registry.register(&id(1), "slew", ToolClass::Gated, &parent);
        let (_park_guard, park) = registry.register(&id(2), "park", ToolClass::Ungated, &parent);

        // Acknowledge from a task, as a real body would by returning.
        let ack = tokio::spawn(async move {
            slew.cancelled().await;
            let reason = slew.reason();
            drop(slew_guard);
            reason
        });
        let count = registry.cancel_for_safety().await;
        assert_eq!(count, 1);
        assert_eq!(ack.await.unwrap(), CancelReason::Safety);
        assert!(!park.is_cancelled(), "the ungated park must keep running");
        assert_eq!(registry.len(), 1, "only the cancelled slew unregistered");
    }

    /// `capture` is ungated but its in-flight body is cancelled too —
    /// the abort-exposure step delivered through the body.
    #[tokio::test]
    async fn cancel_for_safety_cancels_an_in_flight_capture() {
        let registry = registry();
        let parent = CancellationToken::new();
        let (capture_guard, capture) =
            registry.register(&id(1), "capture", ToolClass::Ungated, &parent);
        let (_filter_guard, filter) =
            registry.register(&id(2), "set_filter", ToolClass::Ungated, &parent);
        let ack = tokio::spawn(async move {
            capture.cancelled().await;
            let error = capture.error();
            drop(capture_guard);
            error
        });
        assert_eq!(registry.cancel_for_safety().await, 1);
        assert_eq!(ack.await.unwrap(), "cancelled: safety");
        assert!(!filter.is_cancelled(), "the filter move must keep running");
    }

    #[tokio::test]
    async fn cancel_for_safety_with_nothing_to_cancel_returns_immediately() {
        let registry = registry();
        let parent = CancellationToken::new();
        let (_guard, _) = registry.register(&id(1), "park", ToolClass::Ungated, &parent);
        assert_eq!(registry.cancel_for_safety().await, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_for_safety_gives_up_waiting_after_the_ack_timeout() {
        let registry = registry();
        let parent = CancellationToken::new();
        // The guard is held for the whole test: a wedged body.
        let (_guard, cancel) = registry.register(&id(1), "slew", ToolClass::Gated, &parent);
        let started = tokio::time::Instant::now();
        let count = registry.cancel_for_safety().await;
        assert_eq!(count, 1);
        assert!(cancel.is_cancelled());
        assert_eq!(started.elapsed(), CANCEL_ACK_TIMEOUT);
    }

    #[tokio::test]
    async fn parent_cancellation_reads_as_client_disconnected() {
        let registry = registry();
        let parent = CancellationToken::new();
        let (_guard, cancel) = registry.register(&id(1), "capture", ToolClass::Ungated, &parent);
        parent.cancel();
        cancel.cancelled().await;
        assert_eq!(cancel.reason(), CancelReason::ClientDisconnected);
        assert_eq!(cancel.error(), "cancelled: client disconnected");
    }

    #[test]
    fn explicit_reason_wins_over_a_later_one() {
        let cancel = Cancel::never();
        cancel.cancel(CancelReason::Safety);
        cancel.cancel(CancelReason::ClientDisconnected);
        assert_eq!(cancel.reason(), CancelReason::Safety);
        assert_eq!(cancel.error(), "cancelled: safety");
    }

    #[test]
    fn never_is_not_cancelled() {
        assert!(!Cancel::never().is_cancelled());
    }

    #[test]
    fn from_extensions_finds_the_stored_handle_or_falls_back_to_never() {
        let mut extensions = Extensions::new();
        assert!(!Cancel::from_extensions(&extensions).is_cancelled());
        let stored = Cancel::never();
        stored.cancel(CancelReason::Safety);
        extensions.insert(stored);
        let found = Cancel::from_extensions(&extensions);
        assert!(found.is_cancelled());
        assert_eq!(found.reason(), CancelReason::Safety);
    }
}
