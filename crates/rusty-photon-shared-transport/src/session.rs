//! Per-client session handle, while-open context, and lifecycle hooks.
//!
//! * [`Session`] is the handle every client holds. Acquired from
//!   [`crate::SharedTransport::acquire`]. Closed via
//!   [`Session::close`] (primary) or dropped (fallback).
//! * [`WhileOpen`] is the context handed to the [`Hooks::while_open`]
//!   task. Same `request` API as `Session` but does **not** participate
//!   in the external refcount — it's infrastructure, not a client.
//! * [`Hooks`] is the per-service plug for connect-side / disconnect-side
//!   / while-open work.

use std::future::Future;
use std::io;
use std::sync::Arc;

use derive_more::Debug;
use tokio::sync::RwLock;
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use crate::codec::Codec;
use crate::connection::{Connection, WireTiming};
use crate::error::{SessionError, TransportError};
use crate::shared::SharedTransport;
use crate::BoxFuture;

/// Shared cell wrapping the *current* [`Connection<C>`] for one open
/// transport.
///
/// [`SharedTransport`] holds an `Arc` clone of this cell in its
/// `slot`; every [`Session`] handed out by `acquire()` also holds an
/// `Arc` clone. The supervisor's reconnect path takes the cell's
/// `write` lock to swap the inner `Arc<Connection<C>>` atomically;
/// live `Session`s observe the new connection on their next
/// `request()` call (which takes a cheap `read` lock to clone the
/// current `Arc<Connection<C>>`). This indirection is what keeps a
/// live session working across a reconnect
/// ([§ Lifecycles](../../../docs/crates/rusty-photon-shared-transport.md#lifecycles)).
pub(crate) type ConnectionCell<C> = Arc<RwLock<Arc<Connection<C>>>>;

/// A live, refcounted handle to the shared transport.
///
/// In `LazyAcquire` the transport stays open, and any
/// [`Hooks::while_open`] task keeps running, while at least one
/// `Session` is alive; in `ServiceLifetime` it stays open from
/// [`start`](crate::SharedTransport::start) to
/// [`shutdown`](crate::SharedTransport::shutdown), whatever the sessions
/// do. Acquired exclusively through [`crate::SharedTransport::acquire`].
#[derive(Debug)]
#[debug("Session {{ closed: {}, .. }}", transport.is_none())]
pub struct Session<C: Codec> {
    transport: Option<Arc<SharedTransport<C>>>,
    #[debug(skip)]
    cell: Option<ConnectionCell<C>>,
}

impl<C: Codec> Session<C> {
    pub(crate) const fn new(transport: Arc<SharedTransport<C>>, cell: ConnectionCell<C>) -> Self {
        Self {
            transport: Some(transport),
            cell: Some(cell),
        }
    }

    /// Send `cmd` and return the matching typed response.
    ///
    /// Forwards to [`Connection::request_timed`] and drops the timing —
    /// the request arbitration
    /// lock makes this call safe to run concurrently from multiple
    /// `Session`s (and the while-open task) sharing the same transport.
    /// Reads the current connection via the cell so a supervisor-driven
    /// reconnect that swapped the inner `Arc<Connection<C>>` between
    /// requests is invisible to the caller — the next request transparently
    /// uses the fresh transport.
    ///
    /// # Errors
    ///
    /// Returns the [`Connection::request`] failures unchanged: a
    /// wire-level transport error, a codec decode failure, or an
    /// exhausted skip budget. Before any I/O it refuses with
    /// [`TransportError::Reconnecting`] while the transport is
    /// reconnecting or a stop is owed to the device, and with an I/O
    /// error once the transport is shut down or closed
    /// ([§ The request gate](../../../docs/crates/rusty-photon-shared-transport.md#the-request-gate)).
    pub async fn request(&self, cmd: C::Command) -> Result<C::Response, SessionError<C::Error>> {
        self.request_timed(cmd).await.map(|(resp, _)| resp)
    }

    /// [`Self::request`], plus when the exchange crossed the wire — see
    /// [`WireTiming`] and [`Connection::request_timed`].
    ///
    /// # Errors
    ///
    /// The [`Self::request`] failures, unchanged.
    pub async fn request_timed(
        &self,
        cmd: C::Command,
    ) -> Result<(C::Response, WireTiming), SessionError<C::Error>> {
        // `cell` only becomes `None` inside `close` (which consumes
        // `self`) or `drop` (which destructs `self`). Neither path can
        // race a live `&self` call to `request_timed`, so this branch is
        // unreachable in well-typed code — handled as an I/O error
        // instead of a panic to satisfy the workspace's no-panic policy.
        let Some(cell) = self.cell.as_ref() else {
            return Err(SessionError::Transport(TransportError::Io(
                io::Error::other("session.request after close/drop"),
            )));
        };
        if let Some(transport) = self.transport.as_ref() {
            // Check `is_reconnecting` first so the transient case
            // surfaces as `Reconnecting` (clients can retry). `shutdown()`
            // clears `reconnecting=false` and `available=false`, so any
            // false `available` reaching the next branch is terminal.
            if transport.is_reconnecting() {
                return Err(SessionError::Transport(TransportError::Reconnecting));
            }
            // And the fact the flag above only *publishes*. A 1→0
            // cleanup whose stop did not land records the debt and
            // then takes the transport out of service, while a
            // reconnect publishing a success writes the flags the other
            // way with no lock between them — `shutdown` waits on the
            // supervisor holding `acquire_lock`, so the publish cannot
            // take it. The two converge, but a request arriving
            // mid-race would read flags that say healthy while a stop
            // is still owed, and command a device that may never have
            // halted.
            //
            // Reading the debt here needs no such ordering to be
            // correct: it is the condition itself, not a publication
            // of it. `Reconnecting` is the honest answer — the replay
            // that discharges it is exactly what the caller is waiting
            // for.
            if transport.safety_debt_outstanding() {
                return Err(SessionError::Transport(TransportError::Reconnecting));
            }
            // Existing `Session`s keep `Arc` clones of the connection cell
            // even after `SharedTransport::shutdown` drops the slot's clone,
            // so without this short-circuit a session can still call
            // `connection.request_timed` against the un-dropped `Connection<C>` and
            // talk to hardware while the service is tearing down. Match the
            // error `acquire()` returns in the same situation.
            //
            // `shutdown()` is `ServiceLifetime`-only, so an unavailable
            // `LazyAcquire` transport is not a service going down: it is
            // a conduit a failed `reconnect_now()` closed. Saying "shut
            // down" there would send an operator looking for a teardown
            // that never happened, when what reopens it is the next 0→1
            // acquire — which needs *every* live session released, not
            // just the one that saw this error, since the reopen rides
            // on the refcount reaching zero.
            if !transport.is_available() {
                let reason = if transport.is_service_lifetime() {
                    "transport has been shut down"
                } else {
                    "transport is closed; it reopens once every session is released"
                };
                return Err(SessionError::Transport(TransportError::Io(
                    io::Error::other(reason),
                )));
            }
        }
        let connection = cell.read().await.clone();

        // Re-read after taking the conduit. The checks above ran
        // before this `await`, so a reconnect could have published in
        // between, and the clone would then be the *replacement*: a
        // request let through before the transition reaching the
        // conduit the replay asserts the stop on. A transition that
        // begins after this line can still let the request out on the
        // conduit it is closing — a command on the way *into* the
        // teardown, followed by the replay on the replacement. Closing
        // that too would need a gate held across every request, the
        // lock `attempt_reconnect` cannot take
        // (docs/crates/rusty-photon-shared-transport.md § The request gate).
        if let Some(transport) = self.transport.as_ref() {
            if transport.is_reconnecting() {
                return Err(SessionError::Transport(TransportError::Reconnecting));
            }
            // Re-read for the same reason the flag is re-read: a
            // cleanup can have recorded a stop as owed while this was
            // awaiting the cell above, and the conduit now in hand is
            // one nothing has halted.
            if transport.safety_debt_outstanding() {
                return Err(SessionError::Transport(TransportError::Reconnecting));
            }
        }

        connection.request_timed(cmd).await
    }

    /// Primary teardown path.
    ///
    /// On the last live session, runs the 1→0 cleanup inline, complete
    /// before this future resolves: in `ServiceLifetime`
    /// [`Hooks::on_last_disconnect`] alone, the port staying open; in
    /// `LazyAcquire` the while-open cancellation, the hook and the close
    /// of the transport.
    ///
    /// On a non-last session, decrements the refcount and returns
    /// `Ok(())` immediately.
    ///
    /// # Errors
    ///
    /// Effectively infallible: teardown absorbs and logs its own
    /// failures, and the transport close cannot fail — though on the
    /// last session out it waits for an in-flight request, bounded by
    /// the transport's own I/O timeout. The only `Err`
    /// arm is the defensive guard against a close after close/drop,
    /// unreachable in well-typed code.
    pub async fn close(mut self) -> Result<(), TransportError> {
        // `close` consumes `self`, so this branch only fires if the
        // field was never populated — which `Session::new` always does.
        // Unreachable by construction; surfaced as an I/O error rather
        // than a panic to satisfy the workspace's no-panic policy.
        let Some(transport) = self.transport.take() else {
            return Err(TransportError::Io(io::Error::other(
                "session.close after close/drop",
            )));
        };
        // Drop our cell clone before the cleanup runs so the slot is
        // the only remaining cell-holder when it does (in LazyAcquire
        // mode).
        self.cell.take();
        transport.release_inline().await
    }
}

impl<C: Codec> Drop for Session<C> {
    /// Detached cleanup safety net.
    ///
    /// If [`Session::close`] hasn't already consumed `transport`, this
    /// spawns the release on the current tokio runtime: the refcount
    /// decrement and, if we're the last out, the cleanup body. The spawn
    /// is fire-and-forget, so neither need have happened when the drop
    /// returns; with no runtime neither happens at all, with a warning,
    /// and a runtime shutting down may never run it. Hence a disconnect
    /// closes its session rather than dropping it
    /// ([§ The device side](../../../docs/crates/rusty-photon-shared-transport.md#the-device-side)).
    fn drop(&mut self) {
        if let Some(transport) = self.transport.take() {
            self.cell.take();
            transport.release_detached();
        }
    }
}

/// Non-refcounted context handed to the [`Hooks::while_open`] closure.
///
/// Shares the inner `Arc<Connection<C>>` with the primary `acquire()`
/// path, so [`WhileOpen::request`] goes through the same request
/// arbitration lock as the [`Session`]s' requests. Dropping a
/// `WhileOpen` does **not** touch the external refcount.
#[derive(Debug)]
#[debug("WhileOpen {{ cancelled: {}, .. }}", cancel.is_cancelled())]
pub struct WhileOpen<C: Codec> {
    connection: Arc<Connection<C>>,
    cancel: CancellationToken,
}

impl<C: Codec> WhileOpen<C> {
    pub(crate) const fn new(connection: Arc<Connection<C>>, cancel: CancellationToken) -> Self {
        Self { connection, cancel }
    }

    /// Send `cmd` and return the matching typed response.
    ///
    /// # Errors
    ///
    /// Returns the [`Connection::request`] failures unchanged: a
    /// wire-level transport error, a codec decode failure, or an
    /// exhausted skip budget.
    pub async fn request(&self, cmd: C::Command) -> Result<C::Response, SessionError<C::Error>> {
        self.request_timed(cmd).await.map(|(resp, _)| resp)
    }

    /// [`Self::request`], plus when the exchange crossed the wire — see
    /// [`WireTiming`] and [`Connection::request_timed`].
    ///
    /// # Errors
    ///
    /// The [`Self::request`] failures, unchanged.
    pub async fn request_timed(
        &self,
        cmd: C::Command,
    ) -> Result<(C::Response, WireTiming), SessionError<C::Error>> {
        self.connection.request_timed(cmd).await
    }

    /// Future that resolves when the surrounding [`SharedTransport`]
    /// begins teardown. Poll loops should `tokio::select!` between
    /// this and their interval tick so cancellation interrupts the
    /// next sleep promptly.
    pub fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.cancel.cancelled()
    }

    /// Returns `true` once teardown has fired the cancellation token.
    /// Non-blocking; useful for `if ctx.is_cancelled() { break; }`.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// Closure type for the [`Hooks::handshake`] hook.
pub type HandshakeFn<C> = Box<
    dyn for<'a> Fn(&'a Connection<C>) -> BoxFuture<'a, Result<(), <C as Codec>::Error>>
        + Send
        + Sync,
>;

/// What a stop-class hook says about the state it was asked to
/// assert — a judgement the hook makes, not an error it returns.
///
/// The distinction matters because the two are not the same question.
/// A hook's individual requests can all succeed while the state still
/// does not hold: the device answers, and the answer does not get the
/// state asserted. Only the hook is in a position to know that — it
/// issued the commands and read the replies, above the layer where
/// [`Connection::request`] decides whether anything reached the wire.
///
/// This is a verdict, not a diagnosis. [`Self::NotAsserted`] is
/// returned for an explicit refusal, for a reply that will not decode,
/// for a device that turns out not to be the expected one, and for a
/// command that never went out — the type carries no reason, and a
/// caller must not read one into it. That is deliberate: the callers
/// all do the same thing with it (decline to treat the conduit as
/// safe), and the hook is where the concrete error is known and where
/// it should be logged. A caller that words a log or an error as
/// though it knows *which* failure occurred is making a claim this
/// type does not support.
///
/// A hook that issues several commands folds its own partial results.
/// "Half of it landed" is not a third answer: the state either holds
/// on the device or it does not, and a caller deciding whether to
/// serve clients needs that one bit. [`StateAssertion::and`] is the
/// fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateAssertion {
    /// The state the hook asserts now holds on the device.
    Asserted,
    /// It does not, or the hook cannot vouch that it does. The caller
    /// must not treat this conduit as carrying a known-good safety
    /// state.
    NotAsserted,
}

impl StateAssertion {
    /// Fold two verdicts: asserted only when both are.
    #[must_use]
    pub const fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Asserted, Self::Asserted) => Self::Asserted,
            _ => Self::NotAsserted,
        }
    }

    /// Whether the state holds.
    #[must_use]
    pub const fn is_asserted(self) -> bool {
        matches!(self, Self::Asserted)
    }
}

/// Closure type for the [`Hooks::on_last_disconnect`] hook.
pub type OnLastDisconnectFn<C> =
    Box<dyn for<'a> Fn(&'a Connection<C>) -> BoxFuture<'a, StateAssertion> + Send + Sync>;

/// Closure type for the [`Hooks::shutdown`] hook.
pub type ShutdownFn<C> = Box<dyn for<'a> Fn(&'a Connection<C>) -> BoxFuture<'a, ()> + Send + Sync>;

/// Closure type for the [`Hooks::while_open`] hook.
pub type WhileOpenFn<C> = Box<dyn Fn(WhileOpen<C>) -> BoxFuture<'static, ()> + Send + Sync>;

/// Service-specific plug for the four lifecycle phases, whose design is
/// in [§ Hooks](../../../docs/crates/rusty-photon-shared-transport.md#hooks).
///
/// * `handshake` runs on every open, **before** any [`Session`] can
///   reach the conduit: the `LazyAcquire`-mode 0→1 `acquire()` call, a
///   cold [`crate::SharedTransport::start`], and every reconnect
///   attempt — not on a `ServiceLifetime`-mode `acquire()`, which is a
///   refcount bump. On error the open fails with nothing published:
///   `acquire()` and `start()` return the error, and a reconnect
///   attempt fails.
/// * `on_last_disconnect` asserts the no-client state. It runs on every
///   refcount 1→0 transition; on every cold
///   [`crate::SharedTransport::start`] before the conduit it opened is
///   published, since a start is a no-client state too, and the only
///   one in which the device may be acting on instructions from a
///   lifecycle this process has no memory of; on a fresh conduit while
///   an earlier invocation's stop is owed, **even with a client
///   attached**, since the debt outlives the refcount that incurred it;
///   and, in `ServiceLifetime` mode, on a reconnect's replacement when
///   the attempt began with no client attached
///   ([§ The no-client state](../../../docs/crates/rusty-photon-shared-transport.md#the-no-client-state)).
///   On a 1→0 in `LazyAcquire` mode it fires after `while_open` is
///   cancelled and before transport teardown; in `ServiceLifetime`
///   mode with `while_open` running and the port staying open.
///
///   Per-service **stop-class** commands only: halt an axis, stop
///   tracking, abort an exposure. Not a park slew, not a cover or
///   lamp, not a power or dew toggle — tenet 3 names those as
///   actuation, and this hook reaches the wire on start and reconnect
///   paths, where only stopping is permitted.
///
///   **The hook returns a verdict.** [`StateAssertion`] answers one
///   question — does the state this hook asserts now hold on the
///   device? The runtime cannot work that out for itself: it watches
///   [`Connection::request`] for commands that never reached the wire,
///   which catches a dead link but not a device that answered and
///   *refused*. A service whose codec decodes above the connection —
///   as the mount's does, returning raw frames that a typed decode
///   rejects later — turns a refusal into a value only the hook ever
///   sees. Returning `NotAsserted` is how it says so; returning
///   `Asserted` on a hook that issues nothing costs nothing.
///
///   The hook is still **best-effort about its own errors**: a failed
///   request must be handled / logged inside the hook body (typically
///   `tracing::warn!` on the request `Result`), never propagated. It
///   does not fail [`Session::close`], and it does not stop the rest
///   of the cleanup. What changes with the verdict is only what the
///   *runtime* concludes afterwards: a stop that did not land is owed
///   to the next conduit
///   ([§ The safety debt](../../../docs/crates/rusty-photon-shared-transport.md#the-safety-debt)).
///
///   A **panic** is not covered by that, and the difference matters to
///   whoever writes the hook. The cleanup awaits it inline, so an
///   unwind takes the steps after it too: in `LazyAcquire` the close
///   and the slot clear never run, and in either mode neither does the
///   bookkeeping that decides whether the safety state landed. A drop
///   guard answers for that much — it records the state as not landed,
///   so the next open replays it — but it cannot close a conduit, so
///   the port stays held until that open releases it. Where the panic
///   itself surfaces depends on how the session went away: awaited by
///   [`Session::close`] it unwinds into that caller, while a plain
///   drop runs the cleanup detached and the panic kills that task.
///
///   **State-lifetime contract:** the next `acquire()` after
///   `on_last_disconnect` does **not** re-run `handshake` in
///   `ServiceLifetime` mode (it is a fast refcount-bump). Hooks must
///   therefore only clear state that the next acquire will explicitly
///   re-establish on its own — clearing handshake-populated caches
///   (CPR, identity fields, capability bits) here will orphan them
///   until the next reconnect runs a fresh handshake. Move that kind
///   of cleanup into `shutdown` (and rely on the reconnect supervisor
///   to refresh the cache via its own handshake call on the new
///   connection).
/// * `shutdown` runs exactly once per `start()`/`shutdown()` cycle, from
///   [`crate::SharedTransport::shutdown`], with the supervisor joined
///   and `while_open` cancelled. Final cleanup before the port closes.
///   Only meaningful in `ServiceLifetime` mode; never fires in
///   `LazyAcquire` mode.
/// * `while_open` (optional) spawns once the conduit it polls is
///   published and runs for as long as the transport is open.
///   Cancelled (with a bounded 5-second join, then abort — and aborted
///   outright if the teardown doing that is itself abandoned) before
///   that conduit closes: before `on_last_disconnect` runs in
///   `LazyAcquire` mode, before `shutdown` runs in `ServiceLifetime`
///   mode, and at each reconnect attempt, which spawns it again on the
///   replacement.
///
///   **Liveness is the hook's own, not the transport's.** The task is
///   spawned and its handle parked in lifecycle state until a teardown
///   joins it; nothing looks at it in between. So a body that returns
///   early, or panics, simply stops — the transport goes on reporting
///   itself available, and a hook that was maintaining a cache goes on
///   serving whatever was in it. A hook that needs to outlive its own
///   failures has to say so from the inside: handle them in the task
///   body, and expose its own staleness to whatever asks (a
///   last-updated timestamp on the cache, a health field a supervisor
///   polls).
///
///   Not observed on purpose. The only lever this crate has is the
///   transport, and treating a dead poll task as a transport failure
///   would close and re-open the port — which does not fix a hook that
///   panics, since the respawned task panics the same way, and turns a
///   deterministic hook bug into an endless cycle of port teardowns.
///   On Windows that cycle meets the `Access is denied` of a port
///   re-opened too soon. A stale cache is the lesser failure, and it
///   is one the service can see and the transport cannot.
pub struct Hooks<C: Codec> {
    pub handshake: HandshakeFn<C>,
    pub on_last_disconnect: OnLastDisconnectFn<C>,
    pub shutdown: ShutdownFn<C>,
    pub while_open: Option<WhileOpenFn<C>>,
}

impl<C: Codec> Hooks<C> {
    /// Hooks that do nothing on any transition and have no
    /// background poll task. Useful as a base for `.handshake = ...`
    /// chains in tests, and as a sane default for services that don't
    /// need any of the four.
    #[must_use]
    pub fn noop() -> Self {
        Self {
            handshake: Box::new(|_| Box::pin(async { Ok(()) })),
            on_last_disconnect: Box::new(|_| Box::pin(async { StateAssertion::Asserted })),
            shutdown: Box::new(|_| Box::pin(async {})),
            while_open: None,
        }
    }
}

impl<C: Codec> Default for Hooks<C> {
    fn default() -> Self {
        Self::noop()
    }
}

// Type-check that the closures' futures are Send so they can cross
// tokio's task boundary. If a closure captures a non-Send value (e.g.
// Rc, RefCell) this will fail at the call site, which is the desired
// developer experience.
const _: fn() = || {
    const fn assert_send<T: Send>() {}
    assert_send::<BoxFuture<'static, ()>>();
};

// Silence unused: the Future bound via WaitForCancellationFuture isn't
// referenced directly by name elsewhere — explicit import keeps the
// public API surface clear in docs.
const _: fn() = || {
    const fn _assert_future<F: Future<Output = ()>>() {}
};

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::StateAssertion;

    /// The fold is what a hook issuing several commands uses to answer
    /// one question, so both directions of it are contract.
    #[test]
    fn a_fold_is_asserted_only_when_every_part_is() {
        assert_eq!(
            StateAssertion::Asserted.and(StateAssertion::Asserted),
            StateAssertion::Asserted
        );
        // Every mixed pairing is not-asserted: half a halt is not a
        // third answer, it is a mount that may still be turning.
        assert_eq!(
            StateAssertion::Asserted.and(StateAssertion::NotAsserted),
            StateAssertion::NotAsserted
        );
        assert_eq!(
            StateAssertion::NotAsserted.and(StateAssertion::Asserted),
            StateAssertion::NotAsserted
        );
        assert_eq!(
            StateAssertion::NotAsserted.and(StateAssertion::NotAsserted),
            StateAssertion::NotAsserted
        );
    }

    #[test]
    fn only_asserted_reads_as_asserted() {
        assert!(StateAssertion::Asserted.is_asserted());
        assert!(!StateAssertion::NotAsserted.is_asserted());
    }
}
