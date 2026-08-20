//! Core lifecycle: refcounted multi-client sharing of one duplex transport.
//!
//! [`SharedTransport`] owns the open-state lock, the refcount, the slot
//! holding the current [`Connection`], and the optional while-open
//! task's join handle + cancellation token. All transitions go through
//! [`SharedTransport::acquire`] (0→1+ and reuse) and the internal
//! `release_inline` / `release_detached` paths called by
//! [`Session::close`] and `Session::drop`.
//!
//! Two lifecycle modes coexist (see the
//! [`eager-hardware-validation`][lifecycle-plan] plan for the rationale):
//!
//! - **`LazyAcquire`** (default, unchanged from pre-Phase-0a): the port
//!   opens on the 0→1 `acquire()` and closes on the 1→0 `Session::close()`.
//!   `Hooks::on_last_disconnect` runs on 1→0; `Hooks::shutdown` is never
//!   invoked. Services that don't call [`SharedTransport::start`] get
//!   this behaviour.
//! - **`ServiceLifetime`** (opt-in via [`SharedTransport::start`]): the
//!   port opens at `start()` and stays open until [`SharedTransport::shutdown`]
//!   is called. `acquire()` becomes a fast refcount-bump.
//!   `Hooks::on_last_disconnect` runs on every 1→0 and the port stays
//!   open. `Hooks::shutdown` runs once on `shutdown()` before the port
//!   actually closes.
//!
//! The mode is a single [`AtomicBool`] flipped exclusively by `start()`
//! and `shutdown()`; all branches that need to discriminate read it
//! once under the [`acquire_lock`](SharedTransport::acquire_lock) so the
//! observation is consistent for the duration of one lifecycle
//! transition.
//!
//! [lifecycle-plan]: ../../../../docs/plans/archive/eager-hardware-validation.md
//! [`Session::close`]: crate::Session::close

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::codec::Codec;
use crate::connection::Connection;
use crate::error::{SessionError, TransportError};
use crate::session::{ConnectionCell, Hooks, Session, WhileOpen};
use crate::transport::TransportFactory;

/// Bounded join timeout for the while-open task at teardown.
///
/// If the task ignores cancellation it gets `abort()`-ed and teardown
/// proceeds. The request-arbitration lock the task may have been holding
/// is released by the abort (its connection clone drops).
const WHILE_OPEN_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Default cadence for the reconnect supervisor's periodic retry while
/// the transport is in the `Reconnecting` state. Five seconds is fast
/// enough that a brief USB unplug/replug recovers within one or two
/// attempts and slow enough that a permanently-dead device doesn't
/// spam syslog. Configurable per service via
/// [`SharedTransport::set_reconnect_interval`].
pub const DEFAULT_RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

/// Refcounted multi-client lifecycle wrapper around a single duplex
/// transport.
///
/// Constructed once at service startup and shared across all device
/// types that need the same wire. Every device calls
/// [`SharedTransport::acquire`] to get a [`Session`]; the 0→1 transition
/// runs [`Hooks::handshake`] and spawns [`Hooks::while_open`]; the 1→0
/// transition cancels the while-open task and runs [`Hooks::teardown`].
pub struct SharedTransport<C: Codec> {
    factory: Arc<dyn TransportFactory>,
    codec: C,
    hooks: Hooks<C>,

    /// External session refcount. Incremented on every [`acquire`], decremented
    /// on every [`Session::close`] / [`Session::drop`]. The while-open task
    /// does NOT participate.
    ///
    /// [`acquire`]: SharedTransport::acquire
    /// [`Session::close`]: crate::Session::close
    /// [`Session::drop`]: crate::Session
    count: AtomicU32,
    /// `true` between handshake-success and teardown-start. Distinct from
    /// `count > 0` because a connect-in-flight has `count == 1` but
    /// `available == false`, and a teardown-in-flight has `count == 0` but
    /// the underlying transport hasn't closed yet.
    available: AtomicBool,
    /// `false` until [`SharedTransport::start`] is called; `true` between
    /// start and [`SharedTransport::shutdown`]. Drives the
    /// `LazyAcquire`-vs-`ServiceLifetime` mode discrimination at the
    /// top of [`acquire`](Self::acquire) and inside
    /// `run_last_disconnect_locked`. See the module-level docstring for
    /// the two modes' behaviour.
    service_lifetime: AtomicBool,
    /// `true` while the reconnect supervisor is mid-recovery — between
    /// observing a transport error and the next successful handshake.
    /// Sessions that observe this short-circuit with
    /// [`TransportError::Reconnecting`] so callers don't drive requests
    /// against the dying transport.
    reconnecting: AtomicBool,
    /// `Some` between the 0→1 open and the 1→0 close. Cloned out for every
    /// new [`Session`]; cleared at teardown so the underlying transport
    /// can drop. The cell layer ([`ConnectionCell`]) lets the supervisor
    /// swap the inner `Arc<Connection<C>>` atomically so live `Session`s
    /// follow the swap on their next request — see [`session::ConnectionCell`].
    ///
    /// [`session::ConnectionCell`]: crate::session::ConnectionCell
    slot: Mutex<Option<ConnectionCell<C>>>,
    /// Serialises [`acquire`] against the inline / detached cleanup paths.
    /// Held across the entire 0→1 transition and the entire 1→0 transition;
    /// the fast path (acquire when `count > 0`) takes it just long enough to
    /// increment and read the slot.
    ///
    /// [`acquire`]: SharedTransport::acquire
    acquire_lock: Mutex<()>,
    while_open_state: Mutex<Option<(JoinHandle<()>, CancellationToken)>>,
    /// Reconnect-supervisor task handle + cancel token. `Some` between
    /// `start()` and `shutdown()` in `ServiceLifetime` mode; `None` in
    /// `LazyAcquire` mode (no supervisor exists).
    supervisor_state: Mutex<Option<(JoinHandle<()>, CancellationToken)>>,
    /// Fired by [`Connection::request`] on every `TransportError` and by
    /// [`SharedTransport::reconnect_now`]. The supervisor `tokio::select!`s
    /// between this and its periodic ticker.
    reconnect_signal: Arc<Notify>,
    /// Period between reconnect attempts while in the `Reconnecting`
    /// state. Configurable per service via
    /// [`SharedTransport::set_reconnect_interval`]. Default
    /// [`DEFAULT_RECONNECT_INTERVAL`] = 5s.
    reconnect_interval: Mutex<Duration>,
    /// Serialises [`attempt_reconnect`](Self::attempt_reconnect) so
    /// the supervisor's periodic / signal-driven tick can't race
    /// [`reconnect_now`](Self::reconnect_now) (or two concurrent
    /// `reconnect_now` callers). Without this both paths would call
    /// `attempt_reconnect` directly and could run overlapping
    /// `factory.open` → handshake → cell swap → `while_open` respawn
    /// sequences, producing extra `open()` calls and orphaned poll
    /// tasks. With the mutex, at most one attempt runs at a time;
    /// the second caller blocks, then runs its own attempt (rare
    /// redundant work, but never inconsistent state).
    attempt_reconnect_lock: Mutex<()>,
}

impl<C: Codec> SharedTransport<C> {
    /// Build the shared transport. Returns an [`Arc`] because every
    /// device handle stores one, plus the [`Session`]s the devices hold.
    pub fn new(factory: Arc<dyn TransportFactory>, codec: C, hooks: Hooks<C>) -> Arc<Self> {
        Arc::new(Self {
            factory,
            codec,
            hooks,
            count: AtomicU32::new(0),
            available: AtomicBool::new(false),
            service_lifetime: AtomicBool::new(false),
            reconnecting: AtomicBool::new(false),
            slot: Mutex::new(None),
            acquire_lock: Mutex::new(()),
            while_open_state: Mutex::new(None),
            supervisor_state: Mutex::new(None),
            reconnect_signal: Arc::new(Notify::new()),
            reconnect_interval: Mutex::new(DEFAULT_RECONNECT_INTERVAL),
            attempt_reconnect_lock: Mutex::new(()),
        })
    }

    /// Override the reconnect supervisor's periodic retry interval.
    /// Takes effect on the next supervisor wake-up — services that need
    /// a non-default cadence should call this before [`start`](Self::start).
    pub async fn set_reconnect_interval(&self, interval: Duration) {
        *self.reconnect_interval.lock().await = interval;
    }

    /// Returns `true` between successful handshake and the start of teardown.
    ///
    /// Cheap, non-blocking. A `true` from this method is a moment-in-time
    /// snapshot — by the time the caller acts on it the transport may
    /// have started teardown. Use [`acquire`](Self::acquire) to obtain
    /// a guarantee.
    pub fn is_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }

    /// Returns `true` while the reconnect supervisor is mid-recovery.
    /// Used by [`Session::request`] to short-circuit requests against
    /// a dying transport with [`TransportError::Reconnecting`].
    pub fn is_reconnecting(&self) -> bool {
        self.reconnecting.load(Ordering::SeqCst)
    }

    /// Opt in to `ServiceLifetime` mode: open the port, run the
    /// handshake (which is the identity-probe checkpoint), and spawn
    /// the while-open task. The port stays open until
    /// [`shutdown`](Self::shutdown) is called, regardless of how many
    /// clients are connected.
    ///
    /// Intended call site: the service binary's `main`, after config
    /// load and before binding the Alpaca HTTP server. On error,
    /// `main` should map the returned [`SessionError`] to a non-zero
    /// `ExitCode` so systemd / orchestration treats startup as a
    /// failure instead of advertising a broken device.
    ///
    /// Idempotent: a second call observes `service_lifetime == true`
    /// and returns `Ok(())` immediately.
    pub async fn start(self: &Arc<Self>) -> Result<(), SessionError<C::Error>> {
        let _guard = self.acquire_lock.lock().await;

        if self.service_lifetime.load(Ordering::SeqCst) && self.available.load(Ordering::SeqCst) {
            // Already in ServiceLifetime mode with an open transport
            // — idempotent.
            return Ok(());
        }

        if self.available.load(Ordering::SeqCst) {
            // A client already opened the transport lazily via
            // `acquire()` before `start()` was called. Promote the
            // existing transport to ServiceLifetime mode in place —
            // the slot is populated, while_open is running, the
            // handshake already ran. No I/O needed; just flip the flag
            // so the next 1→0 transition keeps the port open instead
            // of tearing it down. The lazy `acquire()` cold-start
            // path already attaches the reconnect signal to the
            // Connection it published, so spawning the supervisor
            // here wires up listener+notifier correctly.
            self.service_lifetime.store(true, Ordering::SeqCst);
            self.spawn_supervisor().await;
            return Ok(());
        }

        // Cold start covers both first-ever start and start-after-shutdown.
        // Falls through to the open / handshake / publish sequence below.

        // Cold start: open the transport, run the handshake, spawn
        // while_open + supervisor. Structurally identical to the 0→1
        // path inside `acquire()` but the refcount stays at 0 — the
        // service holds the transport open via the `service_lifetime`
        // flag, not via a refcount slot.
        let raw_transport = self.factory.open().await.map_err(SessionError::Transport)?;
        let connection = Arc::new(
            Connection::new(raw_transport, self.codec.clone())
                .with_reconnect_signal(self.reconnect_signal.clone()),
        );

        (self.hooks.handshake)(&connection)
            .await
            .map_err(SessionError::Codec)?;

        // Build the while_open future BEFORE publishing so a panic in
        // the closure body doesn't leave the slot populated. Mirrors
        // the same precaution in `acquire()`.
        let while_open_pending = self.hooks.while_open.as_ref().map(|while_open_fn| {
            let cancel = CancellationToken::new();
            let ctx = WhileOpen::new(connection.clone(), cancel.clone());
            let fut = while_open_fn(ctx);
            (fut, cancel)
        });

        let cell: ConnectionCell<C> = Arc::new(RwLock::new(connection));
        *self.slot.lock().await = Some(cell);
        self.available.store(true, Ordering::SeqCst);
        self.reconnecting.store(false, Ordering::SeqCst);
        self.service_lifetime.store(true, Ordering::SeqCst);

        if let Some((fut, cancel)) = while_open_pending {
            let handle = tokio::spawn(fut);
            *self.while_open_state.lock().await = Some((handle, cancel));
        }

        // Spawn the reconnect supervisor. Owns transient transport-loss
        // recovery for the lifetime of the ServiceLifetime cycle;
        // cancelled by `shutdown()`.
        self.spawn_supervisor().await;

        Ok(())
    }

    /// Spawn the reconnect supervisor. Idempotent: if a supervisor task
    /// is already registered (e.g. `start()` was called twice without an
    /// intervening `shutdown()`), the existing one is cancelled and
    /// replaced.
    async fn spawn_supervisor(self: &Arc<Self>) {
        let mut sup = self.supervisor_state.lock().await;
        if let Some((mut old_handle, old_cancel)) = sup.take() {
            old_cancel.cancel();
            if tokio::time::timeout(WHILE_OPEN_TEARDOWN_TIMEOUT, &mut old_handle)
                .await
                .is_err()
            {
                // A supervisor loop that ignores its cancellation token
                // would leak indefinitely (and potentially fight a
                // replacement spawned right after). Abort and warn so
                // operators see the misbehaving hook in logs.
                old_handle.abort();
                warn!(
                    timeout = ?WHILE_OPEN_TEARDOWN_TIMEOUT,
                    "previous supervisor task did not respond to cancellation; aborted"
                );
            }
        }
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let st_for_task = Arc::clone(self);
        let handle = tokio::spawn(async move {
            st_for_task.supervisor_loop(cancel_for_task).await;
        });
        *sup = Some((handle, cancel));
    }

    /// Supervisor body. Waits on the reconnect signal or the periodic
    /// ticker; on wake, attempts a reconnect if the transport is in the
    /// `Reconnecting` state. Loops until cancelled by `shutdown()`.
    async fn supervisor_loop(self: Arc<Self>, cancel: CancellationToken) {
        loop {
            let interval = *self.reconnect_interval.lock().await;

            let mut signaled = false;
            tokio::select! {
                () = cancel.cancelled() => break,
                () = self.reconnect_signal.notified() => {
                    signaled = true;
                }
                () = tokio::time::sleep(interval) => {}
            }

            if signaled {
                // Connection observed a transport error. Flip into
                // Reconnecting (clients short-circuit immediately) and
                // attempt recovery.
                self.reconnecting.store(true, Ordering::SeqCst);
                self.available.store(false, Ordering::SeqCst);
            }

            if !self.reconnecting.load(Ordering::SeqCst) {
                continue;
            }

            match self.attempt_reconnect().await {
                Ok(()) => {
                    self.reconnecting.store(false, Ordering::SeqCst);
                    self.available.store(true, Ordering::SeqCst);
                    debug!("transport reconnected successfully");
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        retry_in = ?interval,
                        "transport reconnect attempt failed; will retry"
                    );
                }
            }
        }
    }

    /// Run one reconnect attempt: open a fresh transport, run the
    /// handshake against it, swap it into the slot's
    /// [`ConnectionCell`], and respawn `while_open` against the new
    /// connection. Live sessions resume on the new transport on their
    /// next `request()` call.
    ///
    /// Serialised via [`attempt_reconnect_lock`](Self::attempt_reconnect_lock):
    /// the supervisor loop and `reconnect_now()` both go through this
    /// method, and the lock prevents them from running
    /// `factory.open` / handshake / cell swap concurrently. A second
    /// caller blocks until the first completes (typically tens of
    /// milliseconds; longer if `factory.open` is slow on the
    /// platform), then runs its own attempt against the now-fresh
    /// state — wasteful in the rare collision case, but never
    /// inconsistent.
    async fn attempt_reconnect(self: &Arc<Self>) -> Result<(), SessionError<C::Error>> {
        let _attempt_guard = self.attempt_reconnect_lock.lock().await;
        let raw_transport = self.factory.open().await.map_err(SessionError::Transport)?;
        let new_conn = Arc::new(
            Connection::new(raw_transport, self.codec.clone())
                .with_reconnect_signal(self.reconnect_signal.clone()),
        );

        // Run the handshake against the fresh connection in isolation —
        // it owns its own command lock; no contention with live sessions
        // (which are still pointing at the old, dead cell value).
        (self.hooks.handshake)(&new_conn)
            .await
            .map_err(SessionError::Codec)?;

        // Cancel the old while_open task before installing the new
        // connection so its dying-transport poll iterations don't race
        // the swap.
        {
            let mut wo_state = self.while_open_state.lock().await;
            if let Some((mut old_handle, old_cancel)) = wo_state.take() {
                old_cancel.cancel();
                if tokio::time::timeout(WHILE_OPEN_TEARDOWN_TIMEOUT, &mut old_handle)
                    .await
                    .is_err()
                {
                    // A stubborn while_open task that ignores the
                    // cancellation token would keep firing requests
                    // against the dead transport alongside the freshly
                    // installed connection. Abort it so a single
                    // misbehaving hook doesn't outlive its replacement.
                    old_handle.abort();
                    warn!(
                        timeout = ?WHILE_OPEN_TEARDOWN_TIMEOUT,
                        "while_open task did not respond to cancellation during reconnect; aborted"
                    );
                }
            }
        }

        // Atomic cell swap: live `Session<C>` references see the new
        // connection on their next `request()` call. Clone the cell `Arc`
        // out under the slot mutex and drop the guard before awaiting
        // the cell's `RwLock`, so the slot lock isn't held across the
        // cell await (avoids needless contention and removes a fragile
        // lock-ordering between `slot` and the cell).
        let cell = self
            .slot
            .lock()
            .await
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| {
                SessionError::Transport(TransportError::Io(io::Error::other(
                    "slot empty during reconnect attempt",
                )))
            })?;
        *cell.write().await = new_conn.clone();

        // Respawn `while_open` against the fresh connection.
        if let Some(while_open_fn) = self.hooks.while_open.as_ref() {
            let cancel = CancellationToken::new();
            let ctx = WhileOpen::new(new_conn.clone(), cancel.clone());
            let fut = while_open_fn(ctx);
            let handle = tokio::spawn(fut);
            *self.while_open_state.lock().await = Some((handle, cancel));
        }

        Ok(())
    }

    /// Trigger an immediate reconnect attempt outside the supervisor's
    /// usual cadence. Returns once the attempt completes (success or
    /// failure). Useful for the on-acquire eager path (Phase 0b
    /// follow-up) and for tests / a future operator CLI.
    pub async fn reconnect_now(self: &Arc<Self>) -> Result<(), SessionError<C::Error>> {
        self.reconnecting.store(true, Ordering::SeqCst);
        self.available.store(false, Ordering::SeqCst);
        let result = self.attempt_reconnect().await;
        if result.is_ok() {
            self.reconnecting.store(false, Ordering::SeqCst);
            self.available.store(true, Ordering::SeqCst);
        }
        result
    }

    /// Exit `ServiceLifetime` mode: cancel the while-open task, run
    /// [`Hooks::shutdown`], drop the connection (closing the port).
    /// Called from the service's SIGTERM handler.
    ///
    /// Live sessions are not force-closed; their requests will fail
    /// once the underlying transport's last `Arc<Connection<C>>` drops.
    /// The service is responsible for ordering — stop accepting new
    /// HTTP requests and wait for in-flight clients to disconnect
    /// before calling `shutdown()`.
    ///
    /// No-op in `LazyAcquire` mode (returns `Ok(())` immediately).
    /// After a successful `shutdown()` the transport is back in
    /// `Closed` state; a fresh [`start`](Self::start) re-opens it.
    pub async fn shutdown(&self) -> Result<(), TransportError> {
        let _guard = self.acquire_lock.lock().await;

        if !self.service_lifetime.load(Ordering::SeqCst) {
            return Ok(());
        }

        self.available.store(false, Ordering::SeqCst);

        // Cancel the supervisor first so it doesn't fight with
        // shutdown's own teardown by trying to reconnect mid-shutdown.
        let supervisor = self.supervisor_state.lock().await.take();
        if let Some((mut handle, cancel)) = supervisor {
            cancel.cancel();
            if tokio::time::timeout(WHILE_OPEN_TEARDOWN_TIMEOUT, &mut handle)
                .await
                .is_err()
            {
                handle.abort();
                warn!(
                    timeout = ?WHILE_OPEN_TEARDOWN_TIMEOUT,
                    "supervisor task did not respond to cancellation; aborted"
                );
            }
        }

        // Cancel while_open BEFORE running the shutdown hook so the
        // poll loop doesn't race the final cleanup commands on the
        // wire (the shutdown hook holds the command lock via its
        // `request` calls; while_open holding the same lock would
        // serialise but could time out depending on the poll cadence).
        let while_open = self.while_open_state.lock().await.take();
        if let Some((mut handle, cancel)) = while_open {
            cancel.cancel();
            match tokio::time::timeout(WHILE_OPEN_TEARDOWN_TIMEOUT, &mut handle).await {
                Ok(Ok(())) => {}
                Ok(Err(join_err)) => {
                    warn!(
                        error = %join_err,
                        "while_open task panicked or was cancelled before shutdown"
                    );
                }
                Err(_) => {
                    handle.abort();
                    warn!(
                        timeout = ?WHILE_OPEN_TEARDOWN_TIMEOUT,
                        "while_open task did not respond to cancellation; aborted"
                    );
                }
            }
        }

        let cell = self.slot.lock().await.take();
        if let Some(cell) = cell {
            let conn = cell.read().await.clone();
            (self.hooks.shutdown)(&conn).await;
            // Drop the local clone first so refcount drops; then the
            // cell (which holds the last remaining Arc<Connection>) drops
            // and the FrameTransport finally closes.
            drop(conn);
            drop(cell);
        }

        self.reconnecting.store(false, Ordering::SeqCst);

        // Leave `service_lifetime = true` so subsequent `acquire()` calls
        // observe `service_lifetime && !available` and refuse. The next
        // `start()` will see `!available` and do a fresh cold-start
        // (intended for tests / restart scenarios; production normally
        // exits the process after `shutdown()`).
        Ok(())
    }

    /// Hand out a [`Session`]. In `LazyAcquire` mode (default), the 0→1
    /// transition opens the transport, runs the handshake, and starts
    /// the while-open task. In `ServiceLifetime` mode (after
    /// [`start`](Self::start)), every `acquire` is a fast refcount-bump
    /// and slot-clone — the port is already open.
    ///
    /// In `ServiceLifetime` mode after [`shutdown`](Self::shutdown) has
    /// been called, returns `TransportError::Io("transport has been shut
    /// down")`.
    pub async fn acquire(self: &Arc<Self>) -> Result<Session<C>, SessionError<C::Error>> {
        let _guard = self.acquire_lock.lock().await;
        let prev = self.count.fetch_add(1, Ordering::SeqCst);

        let service_lifetime = self.service_lifetime.load(Ordering::SeqCst);

        if prev == 0
            && service_lifetime
            && !self.available.load(Ordering::SeqCst)
            && !self.reconnecting.load(Ordering::SeqCst)
        {
            // ServiceLifetime mode but `shutdown()` already ran. Roll
            // back the speculative increment and refuse — the service
            // is going down and acquiring a new session would defeat
            // the orderly teardown.
            //
            // The `!reconnecting` guard distinguishes terminal shutdown
            // (return `Io("transport has been shut down")`) from the
            // transient reconnect window (where `available=false` too)
            // so a first-client acquire during a reconnect falls through
            // into the ServiceLifetime slot-clone path below. The
            // resulting session's first `request()` then short-circuits
            // on `is_reconnecting()` and returns
            // `TransportError::Reconnecting`, which is the correct UX
            // (\"try again\") rather than a misleading shutdown error.
            self.count.fetch_sub(1, Ordering::SeqCst);
            return Err(SessionError::Transport(TransportError::Io(
                io::Error::other("transport has been shut down"),
            )));
        }

        if prev == 0 && !service_lifetime {
            // LazyAcquire 0→1 path: drop-guarded so any error or panic
            // through the handshake rolls the count back.
            let mut rollback = RollbackGuard {
                count: &self.count,
                armed: true,
            };

            let raw_transport = self.factory.open().await.map_err(SessionError::Transport)?;
            let connection = Arc::new(
                Connection::new(raw_transport, self.codec.clone())
                    .with_reconnect_signal(self.reconnect_signal.clone()),
            );

            (self.hooks.handshake)(&connection)
                .await
                .map_err(SessionError::Codec)?;

            // Build the while-open future BEFORE publishing slot /
            // available — a panic in the user-supplied closure body
            // must roll back without leaving the slot populated. The
            // future itself is `Send` and we keep it local until the
            // publish phase below.
            let while_open_pending = self.hooks.while_open.as_ref().map(|while_open_fn| {
                let cancel = CancellationToken::new();
                let ctx = WhileOpen::new(connection.clone(), cancel.clone());
                let fut = while_open_fn(ctx);
                (fut, cancel)
            });

            // Publish phase: from here on every step is infallible
            // (atomic store, async Mutex::lock without poisoning,
            // tokio::spawn inside an established runtime). The
            // rollback can safely be disarmed before these run.
            rollback.armed = false;
            let cell: ConnectionCell<C> = Arc::new(RwLock::new(connection));
            *self.slot.lock().await = Some(cell.clone());
            self.available.store(true, Ordering::SeqCst);

            if let Some((fut, cancel)) = while_open_pending {
                let handle = tokio::spawn(fut);
                *self.while_open_state.lock().await = Some((handle, cancel));
            }

            return Ok(Session::new(Arc::clone(self), cell));
        }

        // Reuse / ServiceLifetime fast path: clone the slot's cell Arc.
        // Both flavours land here — LazyAcquire when count was already
        // > 0 (another caller already opened the transport), and
        // ServiceLifetime where `start()` populated the slot with
        // count == 0.
        let slot = self.slot.lock().await;
        let Some(cell) = slot.as_ref().cloned() else {
            // In LazyAcquire mode this is impossible by construction —
            // the 0→1 path populates `slot` before releasing
            // `acquire_lock`. In ServiceLifetime mode this can only
            // fire if a buggy caller invoked `acquire()` between
            // `start()` failing and the failure propagating; the
            // rollback path leaves both `available` and
            // `service_lifetime` false, but a successful `start()`
            // would have populated the slot before flipping the flag.
            // Either way, roll back the speculative increment and
            // surface an I/O error rather than panicking.
            drop(slot);
            self.count.fetch_sub(1, Ordering::SeqCst);
            return Err(SessionError::Transport(TransportError::Io(
                io::Error::other("transport refcount > 0 but slot empty"),
            )));
        };
        Ok(Session::new(Arc::clone(self), cell))
    }

    /// Inline release path. Called by [`Session::close`]. If we're the
    /// last live session, runs the cleanup body and returns its result.
    /// Otherwise just decrements and returns.
    ///
    /// [`Session::close`]: crate::Session::close
    pub(crate) async fn release_inline(&self) -> Result<(), TransportError> {
        let _guard = self.acquire_lock.lock().await;
        let prev = self.count.fetch_sub(1, Ordering::SeqCst);
        if prev > 1 {
            return Ok(());
        }
        // prev == 1 → we were the last; or prev == 0 → bug (caller
        // released twice). The latter is asserted out so it fails fast
        // in development; in release builds a stray fetch_sub on 0
        // wraps to u32::MAX and we'd still pass the prev > 1 check above.
        debug_assert_eq!(prev, 1, "release_inline called with refcount=0");
        self.run_cleanup_locked().await
    }

    /// Detached release path. Called by [`Session::drop`]. Spawns the
    /// inline release as a fire-and-forget task on the current tokio
    /// runtime. If no runtime is available the refcount is **not**
    /// decremented and teardown is **not** run; this matches the
    /// documented Drop-is-fallback contract.
    ///
    /// [`Session::drop`]: crate::Session
    pub(crate) fn release_detached(self: Arc<Self>) {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Err(e) = self.release_inline().await {
                        warn!(
                            error = %e,
                            "detached transport cleanup failed (Session::drop fallback path)"
                        );
                    }
                });
            }
            Err(_) => {
                warn!(
                    "no tokio runtime available for Session::drop cleanup; \
                     refcount stuck and teardown skipped — call Session::close().await \
                     from inside a tokio runtime instead"
                );
            }
        }
    }

    /// 1→0 cleanup body. The caller must hold [`acquire_lock`](Self::acquire_lock).
    ///
    /// In `LazyAcquire` mode: cancel/join `while_open` first (so the
    /// poll loop doesn't race the safety commands on the wire),
    /// then run [`Hooks::on_last_disconnect`], then drop the slot's
    /// connection so the port closes. This ordering matches the
    /// contract documented on [`Hooks::on_last_disconnect`] in
    /// `session.rs`.
    ///
    /// In `ServiceLifetime` mode: only run [`Hooks::on_last_disconnect`];
    /// `while_open`, the supervisor, and the port all stay live for
    /// the next client. Transport teardown belongs to the explicit
    /// [`shutdown`](Self::shutdown) call from the service binary.
    async fn run_cleanup_locked(&self) -> Result<(), TransportError> {
        // Re-check count: a new acquire could not have raced in while we
        // held the lock, but the symmetric check makes the invariant
        // explicit and is a cheap sanity net.
        if self.count.load(Ordering::SeqCst) > 0 {
            return Ok(());
        }

        let service_lifetime = self.service_lifetime.load(Ordering::SeqCst);

        if !service_lifetime {
            // LazyAcquire mode: cancel while_open BEFORE running
            // on_last_disconnect so the poll loop's in-flight requests
            // don't interleave with the safety commands. The command
            // lock would arbitrate the bytes-on-the-wire, but the
            // resulting interleaving (one teardown command, one
            // routine poll, one teardown command, …) is not what the
            // hook author wrote against — clean wire access matters.
            self.available.store(false, Ordering::SeqCst);

            let while_open = self.while_open_state.lock().await.take();
            if let Some((mut handle, cancel)) = while_open {
                cancel.cancel();
                match tokio::time::timeout(WHILE_OPEN_TEARDOWN_TIMEOUT, &mut handle).await {
                    Ok(Ok(())) => {}
                    Ok(Err(join_err)) => {
                        warn!(
                            error = %join_err,
                            "while_open task panicked or was cancelled before teardown"
                        );
                    }
                    Err(_) => {
                        handle.abort();
                        warn!(
                            timeout = ?WHILE_OPEN_TEARDOWN_TIMEOUT,
                            "while_open task did not respond to cancellation; aborted"
                        );
                    }
                }
            }
        }
        // In ServiceLifetime mode, while_open keeps running across this
        // transition by design (the port stays open for the next client).

        // Safety teardown: run `on_last_disconnect` against the live
        // connection. In LazyAcquire mode while_open is now quiet so
        // the hook has exclusive command-lock access. In ServiceLifetime
        // mode while_open still runs but the hook's writes still
        // arbitrate through the command lock — same per-command
        // serialisation Sessions use.
        //
        // Clone the cell `Arc` out under the slot mutex and drop the
        // guard before awaiting either the cell's `RwLock` or the hook
        // itself. The hook can issue wire I/O via the connection's
        // command lock, so holding the slot mutex through the hook
        // would block any concurrent `slot.lock()` caller for the full
        // hook duration — and would establish a `slot → command_lock`
        // ordering that nothing else respects.
        let cell_opt = {
            let slot_guard = self.slot.lock().await;
            slot_guard.as_ref().map(Arc::clone)
        };
        if let Some(cell) = cell_opt {
            let conn = cell.read().await.clone();
            (self.hooks.on_last_disconnect)(&conn).await;
        }

        if service_lifetime {
            // Port stays open; next acquire reuses the slot connection.
            return Ok(());
        }

        // LazyAcquire mode: drop the slot's cell so the inner
        // Arc<Connection<C>> drops, which drops the FrameTransport,
        // which closes the OS-level conduit.
        let cell = self.slot.lock().await.take();
        if let Some(cell) = cell {
            drop(cell);
        }
        Ok(())
    }
}

/// On drop, decrements `count` unless explicitly disarmed. Used by
/// [`SharedTransport::acquire`] so an error or panic between
/// `count.fetch_add(1)` and a successful `Session` return rolls the
/// count back to its prior value.
struct RollbackGuard<'a> {
    count: &'a AtomicU32,
    armed: bool,
}

impl Drop for RollbackGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.count.fetch_sub(1, Ordering::SeqCst);
        }
    }
}
