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
use crate::connection::Connection;
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
/// current `Arc<Connection<C>>`). This is the indirection that makes
/// the live-session-survival contract from the
/// [transport-lifecycle plan][lifecycle-plan] possible.
///
/// [lifecycle-plan]: ../../../../docs/plans/archive/eager-hardware-validation.md
pub(crate) type ConnectionCell<C> = Arc<RwLock<Arc<Connection<C>>>>;

/// A live, refcounted handle to the shared transport.
///
/// While at least one `Session` is alive the transport stays open and
/// any [`Hooks::while_open`] task keeps running. Acquired exclusively
/// through [`crate::SharedTransport::acquire`].
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
    /// Forwards to [`Connection::request`] — the request arbitration
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
    /// exhausted skip budget.
    pub async fn request(&self, cmd: C::Command) -> Result<C::Response, SessionError<C::Error>> {
        // `cell` only becomes `None` inside `close` (which consumes
        // `self`) or `drop` (which destructs `self`). Neither path can
        // race a live `&self` call to `request`, so this branch is
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
            // Existing `Session`s keep `Arc` clones of the connection cell
            // even after `SharedTransport::shutdown` drops the slot's clone,
            // so without this short-circuit a session can still call
            // `connection.request` against the un-dropped `Connection<C>` and
            // talk to hardware while the service is tearing down. Match the
            // error `acquire()` returns in the same situation.
            if !transport.is_available() {
                return Err(SessionError::Transport(TransportError::Io(
                    io::Error::other("transport has been shut down"),
                )));
            }
        }
        let connection = cell.read().await.clone();
        connection.request(cmd).await
    }

    /// Primary teardown path.
    ///
    /// On the last live session, awaits while-open cancellation, runs
    /// [`Hooks::teardown`], closes the transport — all inline. Callers
    /// see today's observable behaviour: rollback is complete before
    /// this future resolves.
    ///
    /// On a non-last session, decrements the refcount and returns
    /// `Ok(())` immediately.
    ///
    /// # Errors
    ///
    /// Returns a [`TransportError`] if closing the underlying transport
    /// fails on the last live session.
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
        // the only remaining cell-holder when run_cleanup runs (in
        // LazyAcquire mode).
        self.cell.take();
        transport.release_inline().await
    }
}

impl<C: Codec> Drop for Session<C> {
    /// Detached cleanup safety net.
    ///
    /// If [`Session::close`] hasn't already consumed `transport`, this
    /// decrements the refcount and — if we're the last out — spawns
    /// the cleanup body on the current tokio runtime. The spawn is
    /// fire-and-forget; if no runtime is available or the runtime is
    /// shutting down, teardown commands may not run. See the design
    /// plan for the explicit-close-is-primary rationale.
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
        self.connection.request(cmd).await
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

/// Closure type for the [`Hooks::on_last_disconnect`] hook.
pub type OnLastDisconnectFn<C> =
    Box<dyn for<'a> Fn(&'a Connection<C>) -> BoxFuture<'a, ()> + Send + Sync>;

/// Closure type for the [`Hooks::shutdown`] hook.
pub type ShutdownFn<C> = Box<dyn for<'a> Fn(&'a Connection<C>) -> BoxFuture<'a, ()> + Send + Sync>;

/// Closure type for the [`Hooks::while_open`] hook.
pub type WhileOpenFn<C> = Box<dyn Fn(WhileOpen<C>) -> BoxFuture<'static, ()> + Send + Sync>;

/// Service-specific plug for the four lifecycle phases.
///
/// * `handshake` runs on every transition into the `Open` state — the
///   `LazyAcquire`-mode 0→1 `acquire()` call, the `ServiceLifetime`-mode
///   [`crate::SharedTransport::start`] call, and (in Phase 0b) every
///   successful reconnect. Runs **before** any [`Session`] escapes. On
///   error: rollback (count→0, available→false, transport dropped),
///   error propagated to the caller.
/// * `on_last_disconnect` runs on every refcount 1→0 transition.
///   Per-service safety commands (stop tracking, park, turn off
///   heater, …). In `LazyAcquire` mode, fires once after `while_open`
///   is cancelled and before transport teardown. In `ServiceLifetime`
///   mode, fires on every 1→0 and the port stays open — may run many
///   times during a service's lifetime. The hook signature returns
///   `()` and the runtime ignores any errors observed on the inner
///   `request` calls, so the hook is **best-effort** in both modes;
///   any failures hit while running it must be handled / logged
///   inside the hook body itself (typically `tracing::warn!` on the
///   request `Result`). Failures never propagate to
///   [`Session::close`]'s caller and never abort the cleanup path.
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
///   [`crate::SharedTransport::shutdown`] in the service's SIGTERM handler.
///   Final cleanup before the port closes. Only meaningful in
///   `ServiceLifetime` mode; never fires in `LazyAcquire` mode.
/// * `while_open` (optional) spawns after `handshake` succeeds and runs
///   for as long as the transport is `Open`. Cancelled (with a bounded
///   5-second join, then abort) before `on_last_disconnect` runs in
///   `LazyAcquire` mode, and before `shutdown` runs in `ServiceLifetime`
///   mode. In Phase 0b, also cancelled and respawned across reconnects.
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
            on_last_disconnect: Box::new(|_| Box::pin(async {})),
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
