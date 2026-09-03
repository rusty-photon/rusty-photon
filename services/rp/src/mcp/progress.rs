//! MCP `notifications/progress` emission from long-running blocking
//! helpers.
//!
//! ## Why this exists
//!
//! rp's blocking helpers in [`super::internals`] run for a long time by
//! design — a slew or park up to its predicted deadline, an exposure
//! plus readout, a focuser settle — and a client watching a compound
//! tool (`center_on_target`, `auto_focus`) sees nothing at all between
//! the request and the final result otherwise. Emitting
//! `notifications/progress` every [`PROGRESS_INTERVAL`] from each poll
//! loop gives a `progressToken`-bearing client a live phase label
//! (`slewing`, `parking`, `exposing`, `reading_out`, `focuser_moving`,
//! `settling`) and an elapsed / budget pair it can render.
//!
//! Progress is feedback, not liveness: whether a call is still running
//! is the in-flight registry's business ([`super::inflight`]), and
//! cancelling one goes through the registry's `Cancel` handle, which
//! the same poll loops race against. The two are independent — a
//! client with no `progressToken` is cancelled exactly like one with.
//!
//! ## Token plumbing
//!
//! Progress notifications are only meaningful to clients that send a
//! `progressToken` under `_meta` on the request.
//! [`ProgressSink::from_request_context`] returns `None` when no
//! token is present (or in unit tests that construct an `McpHandler`
//! without an MCP transport at all); helpers treat a `None` sink as a
//! no-op, so the emission path is purely additive.
//!
//! Tests construct a [`CountingProgressEmitter`] to inspect the
//! number and shape of emissions without instantiating a real rmcp
//! peer.

use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::{ProgressNotificationParam, ProgressToken, RequestMetaObject};
use rmcp::service::{Peer, RequestContext};
use rmcp::RoleServer;
use tracing::debug;

/// Cadence at which long-running helpers fire `notifications/progress`:
/// frequent enough for a UI to feel live, sparse enough that a
/// five-minute park costs sixty notifications rather than three
/// thousand.
pub(crate) const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

/// Abstraction over progress emission. Implemented by the real
/// [`ProgressSink`] (which actually sends notifications via
/// `Peer<RoleServer>::notify_progress`) and by test doubles that
/// record calls without a live MCP transport.
///
/// Helpers in [`super::internals`] accept `Option<&dyn ProgressEmitter>`;
/// `None` means "no client wants progress" and the helper skips the
/// emit step entirely.
#[async_trait]
pub(crate) trait ProgressEmitter: Send + Sync {
    async fn emit(&self, progress: f64, total: Option<f64>, message: Option<String>);
}

/// Live progress sink: bundles the per-request `Peer<RoleServer>` and
/// the client-supplied `ProgressToken` so a helper can emit
/// `notifications/progress` without re-fetching either every tick.
/// `Clone` so a multi-step compound (`refocus_train`) can hand each
/// step its own copy against the same token.
#[derive(Clone)]
pub(crate) struct ProgressSink {
    peer: Peer<RoleServer>,
    token: ProgressToken,
}

impl ProgressSink {
    /// Construct a sink from the request's `Peer` + `_meta`. Returns
    /// `None` when the client did not supply a `progressToken` —
    /// helpers treat the missing sink as "skip emission" rather than
    /// failing the tool (most BDD clients and many real consumers do
    /// not send a token).
    pub(crate) fn from_peer_and_meta(
        peer: Peer<RoleServer>,
        meta: &RequestMetaObject,
    ) -> Option<Self> {
        meta.get_progress_token().map(|token| Self { peer, token })
    }

    /// Convenience: pull both inputs off a `RequestContext`. Equivalent
    /// to calling [`Self::from_peer_and_meta`] with the context's
    /// `peer` and `meta` fields.
    pub(crate) fn from_request_context(ctx: &RequestContext<RoleServer>) -> Option<Self> {
        Self::from_peer_and_meta(ctx.peer.clone(), &ctx.meta)
    }

    /// View this sink as the trait object the helpers take. Callers
    /// hold an `Option<ProgressSink>` and the helpers want an
    /// `Option<&dyn ProgressEmitter>`; unsizing does not reach inside
    /// `Option`, and a closure with an inferred return type is not a
    /// coercion site, so `.as_ref().map(ProgressSink::as_emitter)` is
    /// the spelling that lets the coercion happen here instead.
    pub(crate) fn as_emitter(&self) -> &dyn ProgressEmitter {
        self
    }
}

#[async_trait]
impl ProgressEmitter for ProgressSink {
    async fn emit(&self, progress: f64, total: Option<f64>, message: Option<String>) {
        // `ProgressNotificationParam` is `#[non_exhaustive]`, so it must
        // be built via its constructor.
        let mut param = ProgressNotificationParam::new(self.token.clone(), progress);
        param.total = total;
        param.message = message;
        // A closed transport (client went away mid-tool) is a normal
        // case — surfacing it would abort the tool body for no
        // operational reason. Drop the error after a debug log.
        if let Err(e) = self.peer.notify_progress(param).await {
            debug!(error = %e, "notify_progress failed; client likely disconnected");
        }
    }
}

/// Test doubles and helpers for unit tests in this crate. Gated to
/// `#[cfg(test)]` so the production binary doesn't carry them; the
/// `#[allow]` attributes match the convention used for sibling
/// `#[cfg(test)]` blocks under `super` (e.g. `super::tests`).
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    clippy::type_complexity
)]
pub(crate) mod test_support {
    use super::ProgressEmitter;
    use async_trait::async_trait;

    /// Test double: counts emissions and stores their arguments so
    /// unit tests can assert "at least N progress notifications were
    /// sent during this run".
    pub struct CountingProgressEmitter {
        count: std::sync::atomic::AtomicUsize,
        records: std::sync::Mutex<Vec<(f64, Option<f64>, Option<String>)>>,
    }

    impl Default for CountingProgressEmitter {
        fn default() -> Self {
            Self {
                count: std::sync::atomic::AtomicUsize::new(0),
                records: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl CountingProgressEmitter {
        pub(crate) fn count(&self) -> usize {
            self.count.load(std::sync::atomic::Ordering::SeqCst)
        }

        /// Snapshot of every `(progress, total, message)` tuple emitted
        /// so far, for tests that assert on the *content* of a
        /// notification (e.g. the phase label) rather than just the count.
        pub(crate) fn records(&self) -> Vec<(f64, Option<f64>, Option<String>)> {
            self.records
                .lock()
                .expect("CountingProgressEmitter records lock poisoned")
                .clone()
        }
    }

    #[async_trait]
    impl ProgressEmitter for CountingProgressEmitter {
        async fn emit(&self, progress: f64, total: Option<f64>, message: Option<String>) {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.records
                .lock()
                .expect("CountingProgressEmitter records lock poisoned")
                .push((progress, total, message));
        }
    }
}
