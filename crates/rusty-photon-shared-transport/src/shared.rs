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
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify, RwLock};
// `tokio`'s Instant, not `std`'s, so the floor below honours paused time
// in tests the same way the sleeps around it do.
use tokio::task::JoinHandle;
use tokio::time::Instant;
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
/// the transport is in the `Reconnecting` state.
///
/// Five seconds is fast enough that a brief USB unplug/replug recovers
/// within one or two attempts and slow enough that a permanently-dead
/// device doesn't spam syslog. Configurable per service via
/// [`SharedTransport::set_reconnect_interval`].
pub const DEFAULT_RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

/// Clears the retry promise if a cold start does not finish.
///
/// A cold open cancels the supervisor before asking for the conduit, so
/// between that and spawning the next one there is no retry owner. If
/// the open or the handshake fails in between, `reconnecting` would be
/// left saying a retry is coming with nothing alive to make one, and
/// the transport would answer every later acquire with the defensive
/// empty-slot error.
struct ColdStartGuard<'a> {
    reconnecting: &'a AtomicBool,
    armed: bool,
}

impl Drop for ColdStartGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        warn!("start did not complete; clearing the retry nobody would make");
        self.reconnecting.store(false, Ordering::SeqCst);
    }
}

/// Records the pessimistic answer if a safety hook does not return.
///
/// The bookkeeping after `on_last_disconnect` decides, from what the
/// connection saw, whether the state landed. A hook that panics never
/// reaches it, and the honest answer for a hook that did not finish is
/// the same as for one whose commands failed: the state did not land.
/// Everything needed to say so is an atomic, so it can be said from
/// `Drop` — which the close and the slot cannot be, and which is why
/// those are left to the next open to sort out.
struct UnlandedStateGuard<'a> {
    owed: &'a AtomicBool,
    reconnecting: &'a AtomicBool,
    available: &'a AtomicBool,
    service_lifetime: bool,
    armed: bool,
}

impl Drop for UnlandedStateGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        warn!("last-disconnect hook did not return; assuming its state did not land");
        self.owed.store(true, Ordering::SeqCst);
        if self.service_lifetime {
            self.reconnecting.store(true, Ordering::SeqCst);
            self.available.store(false, Ordering::SeqCst);
        }
    }
}

/// Restores the "nothing will retry this" state if a manual reconnect
/// does not return.
///
/// `reconnect_now` sets `reconnecting` before the attempt and answers
/// for it afterwards. A panic in a service's hook skips that answer,
/// and in `LazyAcquire` — where no supervisor will ever clear the flag
/// and a live session keeps the refcount off zero — every later request
/// short-circuits on a retry nobody is going to make.
struct ManualReconnectGuard<'a> {
    reconnecting: &'a AtomicBool,
    service_lifetime: &'a AtomicBool,
    armed: bool,
}

impl Drop for ManualReconnectGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if !self.service_lifetime.load(Ordering::SeqCst) {
            warn!("manual reconnect did not return; clearing the retry nobody would make");
            self.reconnecting.store(false, Ordering::SeqCst);
        }
    }
}

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
    /// When the last reconnect attempt started, from either entry
    /// point. The supervisor's cadence floor reads it; `reconnect_now`
    /// stamps it without waiting on it, because an explicit operator
    /// action should be prompt. Keeping it here rather than local to
    /// the supervisor is what makes the first retry after a manual
    /// attempt observe the interval like every other one.
    last_attempt: Mutex<Option<Instant>>,
    /// Set when an `on_last_disconnect` could not land — its commands
    /// went to a conduit that was already dead or closed, which is what
    /// a 1→0 during a reconnect runs against. The refcount alone cannot
    /// carry that: a new client can acquire before the next attempt
    /// reads it, and the obligation would be dropped on the floor with
    /// the mount still moving.
    ///
    /// Recorded in both modes; what discharges it differs. A reconnect
    /// pays it where a supervisor exists, the next 0→1 open where one
    /// does not, and a cold `start` either way — the handshake those
    /// run is not a substitute, because it is not the safety hook.
    /// Removing any of those replays re-opens the hole: a session
    /// handed out while this is set is a session on a conduit whose
    /// safety state never reached the device.
    safety_state_owed: AtomicBool,
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
            last_attempt: Mutex::new(None),
            safety_state_owed: AtomicBool::new(false),
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

    /// Whether the transport is in `ServiceLifetime` mode. Only there
    /// can [`shutdown`](Self::shutdown) have run, which is what lets
    /// [`Session::request`] tell a service going down from a
    /// `LazyAcquire` conduit that merely failed to reopen.
    pub(crate) fn is_service_lifetime(&self) -> bool {
        self.service_lifetime.load(Ordering::SeqCst)
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
    ///
    /// A cold start publishes a *new* connection cell, so [`Session`]s
    /// handed out before it keep pointing at the old one and do not
    /// follow the conduit this call opens — their requests fail with
    /// the closed-conduit error until they are released and re-acquired.
    /// That is the same contract as after [`shutdown`](Self::shutdown),
    /// which likewise does not force-close live sessions. Only a
    /// reconnect swaps a conduit *within* the existing cell, which is
    /// what lets sessions survive one.
    ///
    /// # Errors
    ///
    /// Returns a [`SessionError`] if opening the transport or running
    /// the handshake hook fails.
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
        //
        // Drop any reconnect notification still pending first. There is
        // no live conduit at this point, so nothing pending can be
        // about one: it was raised by a previous lifecycle's teardown,
        // or by an earlier start whose handshake or safety replay
        // failed on the wire and left before any supervisor existed to
        // hear it. Inherited, it makes the supervisor spawned below
        // tear down the conduit this call is about to open. Not done on
        // the promote branch above, where the conduit *is* live and a
        // pending wake is about it.
        // Release first, then drain: closing the inherited conduit can
        // itself raise a notification, and that one is as stale as the
        // rest.
        self.release_any_held_conduit().await;

        // From here the supervisor is gone and the next one is not
        // spawned until the publish, so nothing would retry a failure
        // in between.
        let mut cold_start = ColdStartGuard {
            reconnecting: &self.reconnecting,
            armed: true,
        };

        self.drop_pending_reconnect_signal("left by an earlier lifecycle");
        let raw_transport = self.factory.open().await.map_err(SessionError::Transport)?;
        let connection = Arc::new(
            Connection::new(raw_transport, self.codec.clone())
                .with_reconnect_signal(self.reconnect_signal.clone()),
        );

        (self.hooks.handshake)(&connection)
            .await
            .map_err(SessionError::Codec)?;

        self.drop_pending_reconnect_signal("tolerated by the handshake");

        // A debt incurred before this call — a lazy 1→0 whose stop did
        // not land, or a `ServiceLifetime` one that took the transport
        // out of service — outlives the mode it was incurred in. This
        // open is a chance to pay it, and refusing to start beats
        // serving clients a conduit whose safety state is unknown.
        self.discharge_owed_state(&connection).await?;

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
        cold_start.armed = false;

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

    /// Drop a pending reconnect notification, if there is one.
    ///
    /// Two callers, and both are about a wake that is not evidence the
    /// conduit in hand is bad. Before a cold open there is no live
    /// conduit at all, so anything pending was raised by a previous
    /// lifecycle. After a handshake, a hook is allowed to treat a probe
    /// as optional and ignore its error — but the request still fired
    /// the signal, and left armed that permit is spent the moment a
    /// supervisor exists, tearing down the conduit the handshake just
    /// accepted. Every open path needs it for that reason, not only the
    /// reconnect.
    ///
    /// Scoped tightly on purpose: `enable()` registers the future as a
    /// waiter when nothing is pending, and one left alive would catch a
    /// later notification meant for the supervisor.
    fn drop_pending_reconnect_signal(&self, reason: &'static str) {
        let pending = self.reconnect_signal.notified();
        tokio::pin!(pending);
        if pending.as_mut().enable() {
            debug!(reason, "dropped a reconnect notification");
        }
    }

    /// Cancel the while-open task and wait for it, bounded. `context`
    /// names the caller in the warning a task that ignores its
    /// cancellation earns, since the three callers — replacing a
    /// conduit, abandoning a replacement, and opening over one left
    /// behind — are told apart only by that.
    async fn cancel_while_open(&self, context: &'static str) {
        let while_open = self.while_open_state.lock().await.take();
        let Some((mut handle, cancel)) = while_open else {
            return;
        };
        cancel.cancel();
        if tokio::time::timeout(WHILE_OPEN_TEARDOWN_TIMEOUT, &mut handle)
            .await
            .is_err()
        {
            // A stubborn task that ignores the cancellation token
            // would keep firing requests against a conduit its owner
            // has moved on from. Abort it rather than let one
            // misbehaving hook outlive what it was watching.
            handle.abort();
            // Same reason: the task may be mid-request, holding the
            // command lock the close is about to want.
            let _ = handle.await;
            warn!(
                timeout = ?WHILE_OPEN_TEARDOWN_TIMEOUT,
                context,
                "while_open task did not respond to cancellation; aborted"
            );
        }
    }

    /// Quiesce whatever the previous lifecycle left running, before
    /// opening another conduit.
    ///
    /// A cold open assumes the slot is empty, and usually it is —
    /// teardown empties it. It is not after a cleanup that ended
    /// early: a `ServiceLifetime` 1→0 whose safety stop did not land
    /// leaves the conduit open by design and only marks the transport
    /// unavailable, and a hook that panics leaves the slot populated
    /// from either mode. Opening on top of that asks the factory for a
    /// port this process still holds, which on Windows is the
    /// `Access is denied` this crate exists to avoid, and orphans the
    /// poll task watching the old conduit.
    ///
    /// The supervisor is part of that. `start()`'s cold path also runs
    /// with `service_lifetime` still true and `available` false, which
    /// is the state a failed last-disconnect stop leaves — and there
    /// the supervisor is alive and may be mid-attempt. Two opens would
    /// then race for the same port, one of them refused, and the older
    /// attempt could publish over the lifecycle this one is building.
    ///
    /// So the invariant is the open's, not the teardown's: never ask
    /// for a conduit while still holding one, or while anything else
    /// is still trying to replace it.
    async fn release_any_held_conduit(&self) {
        let supervisor = self.supervisor_state.lock().await.take();
        if let Some((mut handle, cancel)) = supervisor {
            cancel.cancel();
            if tokio::time::timeout(WHILE_OPEN_TEARDOWN_TIMEOUT, &mut handle)
                .await
                .is_err()
            {
                handle.abort();
                // `abort()` only asks. Waiting is the point here: a
                // supervisor still inside an attempt is holding, or
                // about to open, the very port this open is for.
                let _ = handle.await;
                warn!(
                    timeout = ?WHILE_OPEN_TEARDOWN_TIMEOUT,
                    "supervisor did not respond to cancellation before a fresh open; aborted"
                );
            }
        }

        self.cancel_while_open("opening over a conduit left behind")
            .await;

        let cell = self.slot.lock().await.take();
        if let Some(cell) = cell {
            debug!("releasing a conduit left behind before opening another");
            cell.read().await.close().await;
        }
    }

    /// Pay a safety stop an earlier cleanup could not land, on a
    /// conduit that has just handshaken and is not yet published.
    ///
    /// Used by the two paths that open a conduit outside the reconnect
    /// supervisor — the lazy 0→1 and a cold `start` — because each is a
    /// chance to discharge the debt and there are no others while no
    /// reconnect is running. The reconnect keeps its own replay: it
    /// also fires on a zero refcount, and it verifies across the whole
    /// attempt rather than around this one call. A client cannot have
    /// commanded anything on a conduit that has not been published, so
    /// asserting the stop here is safe whatever the refcount says.
    ///
    /// # Errors
    ///
    /// Returns a [`SessionError`] when the replay did not reach the
    /// device. The conduit is closed and the debt left standing — the
    /// caller must abandon this conduit rather than expose one whose
    /// safety state is still unknown.
    async fn discharge_owed_state(
        &self,
        connection: &Connection<C>,
    ) -> Result<(), SessionError<C::Error>> {
        if !self.safety_state_owed.load(Ordering::SeqCst) {
            return Ok(());
        }

        debug!("discharging an owed last-disconnect state on the fresh conduit");
        let before = connection.wire_failures();
        (self.hooks.on_last_disconnect)(connection).await;
        if connection.wire_failures() != before {
            connection.close().await;
            return Err(SessionError::Transport(TransportError::Io(
                io::Error::other(
                    "the owed last-disconnect state did not land on the fresh conduit",
                ),
            )));
        }

        self.safety_state_owed.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Hold off until at least `interval` has passed since the last
    /// attempt from *either* entry point, re-reading the timestamp each
    /// time round rather than sleeping once against a snapshot: a
    /// `reconnect_now()` can stamp it while this sleep is in progress,
    /// and the guarantee is one attempt per interval, not one sleep.
    ///
    /// Returns `false` if cancelled while waiting.
    async fn wait_out_cadence(&self, interval: Duration, cancel: &CancellationToken) -> bool {
        loop {
            let Some(previous) = *self.last_attempt.lock().await else {
                return true;
            };
            let waited = previous.elapsed();
            if waited >= interval {
                return true;
            }
            tokio::select! {
                () = cancel.cancelled() => return false,
                () = tokio::time::sleep(interval.saturating_sub(waited)) => {}
            }
        }
    }

    /// Supervisor body. Waits on the reconnect signal or the periodic
    /// ticker; on wake, attempts a reconnect if the transport is in the
    /// `Reconnecting` state. Loops until cancelled by `shutdown()`.
    ///
    /// At most one attempt per `reconnect_interval`, however the wake
    /// arrived — see the floor below for why the signal alone is not a
    /// safe trigger.
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

            // Floor between attempts. An attempt raises the signal from
            // inside itself whenever one of its own requests fails on
            // the wire — the handshake's, or the no-client safety
            // replay's. `Notify::notify_one` keeps that permit, so the
            // next `notified()` returns at once and the loop starts
            // another attempt with no delay at all: a stop command that
            // keeps failing on a freshly handshaken link would cycle
            // the port as fast as it can be opened, which on Windows is
            // the pathology this crate's retry ladder exists to ride
            // out. The `retry_in` the failure arm logs is only true
            // with this floor in place.
            if !self.wait_out_cadence(interval, &cancel).await {
                break;
            }

            // Re-read after the wait: the state that justified this
            // attempt is from before it, and a `reconnect_now()` can
            // have recovered the transport in the meantime. Attempting
            // anyway would close the connection that call just
            // published and open another for nothing.
            if !self.reconnecting.load(Ordering::SeqCst) {
                continue;
            }

            // Run the attempt as a task rather than inline. It awaits
            // the service's own handshake and safety hooks, and a
            // panic inside either would unwind this loop — the one
            // task that retries — leaving `reconnecting` set with
            // nothing left to clear it. As a task the panic comes back
            // as a join error, the same way a panicking `while_open`
            // task already does, and the attempt is simply a failed
            // one. `reconnect_now` keeps propagating its own: there
            // the caller asked, and sees it.
            let attempting = Arc::clone(&self);
            let mut attempt = tokio::spawn(async move { attempting.attempt_reconnect().await });
            let joined = tokio::select! {
                joined = &mut attempt => joined,
                () = cancel.cancelled() => {
                    // Dropping a `JoinHandle` does not stop the task.
                    // `shutdown()` is waiting on this loop's own join
                    // and gives it a bounded time, so an attempt left
                    // running would go on holding the port it just
                    // opened — and could publish into a lifecycle that
                    // is already gone. Abort, then wait for it to
                    // actually be gone before leaving.
                    attempt.abort();
                    let _ = attempt.await;
                    break;
                }
            };
            let outcome = match joined {
                Ok(result) => result,
                Err(join_err) => {
                    warn!(
                        error = %join_err,
                        "reconnect attempt panicked; treating it as a failed attempt"
                    );
                    Err(SessionError::Transport(TransportError::Io(
                        io::Error::other("reconnect attempt panicked"),
                    )))
                }
            };

            match outcome {
                Ok(()) => {
                    // Order matters: `Session::request` reads
                    // `reconnecting` and then `available`, so clearing
                    // the first while the second is still false gives a
                    // racing request the terminal shutdown error — for
                    // a reconnect that just succeeded. Published this
                    // way round, the worst it sees is `Reconnecting`,
                    // which is transient and retryable.
                    self.available.store(true, Ordering::SeqCst);
                    self.reconnecting.store(false, Ordering::SeqCst);
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

    /// Run one reconnect attempt: quiesce and close the connection
    /// being replaced, open a fresh transport, run the handshake
    /// against it, swap it into the slot's [`ConnectionCell`], and
    /// respawn `while_open` against the new connection. Live sessions
    /// resume on the new transport on their next `request()` call.
    ///
    /// The old connection is closed **before** the new one is opened.
    /// That order matters for an exclusive conduit: a Windows COM port
    /// refuses a second `CreateFile` while the process still holds the
    /// first handle, so opening first means the open that would have
    /// released the old handle is the one that fails — and the
    /// transport never recovers, however many times the supervisor
    /// retries. Nothing is lost by closing early: `attempt_reconnect`
    /// only runs on a transport already marked unavailable, and live
    /// sessions short-circuit on [`TransportError::Reconnecting`]
    /// before they reach the connection.
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

        *self.last_attempt.lock().await = Some(Instant::now());

        // Stamped here rather than in the supervisor so a manual
        // `reconnect_now()` feeds the cadence floor too: otherwise the
        // supervisor's first retry after one — which a failed replay
        // signals for immediately — would start with no delay.

        // Cancel the old while_open task first: it holds its own
        // `Arc<Connection<C>>` and may be mid-request on the dying
        // transport, so it has to be gone before the close below can
        // take the command lock, and before its poll iterations could
        // race the cell swap.
        self.cancel_while_open("replacing a conduit").await;

        // Clone the cell `Arc` out under the slot mutex and drop the
        // guard before awaiting the cell's `RwLock`, so the slot lock
        // isn't held across the cell await (avoids needless contention
        // and removes a fragile lock-ordering between `slot` and the
        // cell). Read here rather than after the open so the conduit
        // can be released first.
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

        // Release the conduit the replacement is about to ask for. An
        // aborted while_open task or a live `Session` can still hold an
        // `Arc` to this connection, so dropping the cell's reference
        // would not be enough — see `Connection::close`.
        cell.read().await.close().await;

        let raw_transport = self.factory.open().await.map_err(SessionError::Transport)?;
        let new_conn = Arc::new(
            Connection::new(raw_transport, self.codec.clone())
                .with_reconnect_signal(self.reconnect_signal.clone()),
        );

        // Run the handshake against the fresh connection in isolation —
        // it owns its own command lock; no contention with live sessions
        // (which are still pointing at the old, closed cell value).
        (self.hooks.handshake)(&new_conn)
            .await
            .map_err(SessionError::Codec)?;

        // Everything this conduit carries from here on has to land for
        // the attempt to count as a recovery — see the check at the end
        // of this method. Snapshotted after the handshake rather than
        // from zero so a handshake that tolerates a failed probe of its
        // own is not held against it.
        let failures_at_handshake = new_conn.wire_failures();

        // Drained here rather than later so a failure in the replay or
        // the poll task still gets its wake.
        self.drop_pending_reconnect_signal("tolerated by the handshake");

        // Build the while-open future BEFORE publishing, for the same
        // reason the lazy 0→1 path does: the closure is user-supplied
        // and can panic, and a panic after the publish would leave the
        // replacement installed in the slot with no poll task watching
        // it.
        //
        // Caught rather than propagated, so this one failure mode
        // closes the replacement conduit explicitly instead of leaving
        // it to a drop — which on Windows is not a release.
        //
        // That is all it covers, and the boundary is narrower than it
        // looks. The supervisor running the attempt as a task catches a
        // panic in a hook the attempt *awaits* — the handshake's, the
        // safety replay's. The poll future below is spawned and not
        // awaited, so a panic in its body is not seen here at all: it
        // surfaces as a join error at the next teardown, and until then
        // the transport is available with nothing polling it.
        let mut while_open_pending = None;
        if let Some(while_open_fn) = self.hooks.while_open.as_ref() {
            let cancel = CancellationToken::new();
            let ctx = WhileOpen::new(new_conn.clone(), cancel.clone());
            if let Ok(fut) = catch_unwind(AssertUnwindSafe(|| while_open_fn(ctx))) {
                while_open_pending = Some((fut, cancel));
            } else {
                // The panic hook has already printed it; this is about
                // what the transport does next.
                warn!("while_open constructor panicked; treating the attempt as failed");
                new_conn.close().await;
                return Err(SessionError::Transport(TransportError::Io(
                    io::Error::other("while_open constructor panicked during reconnect"),
                )));
            }
        }

        // Atomic cell swap: live `Session<C>` references see the new
        // connection on their next `request()` call.
        //
        // Publish under the slot guard, and only into the cell still in
        // the slot. `shutdown()` and the `LazyAcquire` 1→0 cleanup both
        // take the slot, and neither is excluded from this path —
        // taking `acquire_lock` here instead would deadlock against
        // `shutdown()`, which holds it while joining the supervisor
        // that is running this very attempt. So an attempt in flight
        // can find the transport torn down underneath it, and
        // publishing anyway would hide the replacement in a cell
        // nothing reads again, holding a port nothing will close.
        //
        // The slot → cell lock order this introduces cannot invert:
        // nothing holds a cell lock while taking the slot.
        let slot = self.slot.lock().await;
        let still_in_slot = slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &cell));
        if still_in_slot {
            *cell.write().await = new_conn.clone();
        }
        // Explicit, and last: the guard has to outlive the write above,
        // which is the point of taking it here at all.
        drop(slot);

        if !still_in_slot {
            new_conn.close().await;
            return Err(SessionError::Transport(TransportError::Io(
                io::Error::other("transport was torn down during the reconnect attempt"),
            )));
        }

        // Respawn `while_open` against the fresh connection. Only the
        // spawn is left here; everything that could panic ran above.
        if let Some((fut, cancel)) = while_open_pending {
            let handle = tokio::spawn(fut);
            *self.while_open_state.lock().await = Some((handle, cancel));
        }

        // Re-assert the no-client state on the replacement.
        //
        // `on_last_disconnect` is where a service puts what must hold
        // while nothing is attached — for the mount, halting both axes
        // and stopping tracking. Its commands go out on whatever
        // connection was live when the 1→0 landed, and during a
        // reconnect that connection is dead or, since this method
        // closes it first, closed: the hook runs, every command fails,
        // and nothing replays it. A mount that was moving when its
        // link dropped then stays moving with no client attached and
        // no further attempt to stop it.
        //
        // So run it again here when the refcount is still zero. Tenet
        // 3 permits it on a reconnect path: the hook is the
        // last-disconnect one, which is stop-class by construction —
        // and it is a no-op for every service whose hook is empty.
        //
        // The count is read without `acquire_lock`, so a client can
        // acquire between the check and the hook's last command. That
        // is safe, and not because the hook is best-effort: `acquire()`
        // lets a first client in during a reconnect precisely so its
        // first `request()` can answer `Reconnecting` rather than a
        // misleading shutdown error, and `reconnecting` stays set until
        // this method returns. So a client arriving inside this window
        // holds a session that cannot put a command on the wire until
        // the halt below has already gone out. It cannot be mid-slew
        // here, because it has not been able to command one.
        // Replay when nothing is attached, and also when a 1→0 already
        // tried and failed — the refcount is a snapshot, so a client
        // that acquires between that failure and this line would
        // otherwise bury the obligation. Such a client cannot be
        // mid-slew: `reconnecting` stays set until this method returns,
        // so every request it makes is refused until the stop lands.
        let owed = self.safety_state_owed.load(Ordering::SeqCst);
        let replayed = self.service_lifetime.load(Ordering::SeqCst)
            && (owed || self.count.load(Ordering::SeqCst) == 0);
        if replayed {
            debug!(
                owed,
                "re-asserting the last-disconnect state on the fresh conduit"
            );
            // Owed until proven landed: if the check below fails, the
            // attempt returns `Err` with this still set, so the next
            // attempt replays regardless of who is attached by then.
            self.safety_state_owed.store(true, Ordering::SeqCst);
            (self.hooks.on_last_disconnect)(&new_conn).await;
        }

        // A conduit that failed to carry something is not a recovery.
        //
        // The safety hook returns `()` — best-effort, by the contract
        // its other callers rely on — so its own result cannot say
        // whether the stop landed. The connection can: a request that
        // did not complete on the wire bumped this counter. Reporting
        // such an attempt as a success would clear `reconnecting` and
        // set `available`, and a client could then acquire and drive a
        // mount that is still moving, because the halt meant to stop it
        // never reached the device.
        //
        // Checked over the whole attempt rather than just around the
        // replay above, because the refcount read there is a snapshot:
        // a client still attached at that line can release during the
        // rest of this method, and its 1→0 runs the same hook on this
        // same connection from `run_cleanup_locked`. A window remains
        // for a 1→0 that lands after this check — closing that needs
        // the attempt and the supervisor's state transition to be one
        // step, conditional on the lifecycle the attempt started in
        // still being the current one.
        //
        // It does not cover the respawned `while_open` task, which is
        // detached: its first request may land either side of this
        // line. A failure there raises the signal like any other, so
        // the supervisor comes back round to it; what this check
        // guarantees is only what the attempt itself put on the wire.
        //
        // What this does not see is a command the device *answered* and
        // rejected: `request_typed`-style callers decode above
        // `Connection::request`, so a protocol-level refusal of a stop
        // never reaches this counter. Only the hook knows that one, and
        // its signature returns `()`.
        if new_conn.wire_failures() != failures_at_handshake {
            // Published already, so failing is not enough on its own:
            // the replacement and the poll task respawned against it
            // would go on living, the task issuing requests at its
            // cadence and the conduit holding the port. A supervisor
            // clears that on its next attempt, but `LazyAcquire` has
            // none, and its documented failed-reconnect state is a
            // closed conduit that reopens once sessions are released.
            // Leave that state, in either mode.
            self.cancel_while_open("abandoning a replacement").await;
            new_conn.close().await;

            return Err(SessionError::Transport(TransportError::Io(
                io::Error::other(
                    "a request was dropped while recovering; the conduit is not usable",
                ),
            )));
        }

        if replayed {
            self.safety_state_owed.store(false, Ordering::SeqCst);
        }

        Ok(())
    }

    /// Trigger an immediate reconnect attempt outside the supervisor's
    /// usual cadence. Returns once the attempt completes (success or
    /// failure). Useful for the on-acquire eager path (Phase 0b
    /// follow-up) and for tests / a future operator CLI.
    ///
    /// Replacing the conduit means closing the current one first (see
    /// [`attempt_reconnect`](Self::attempt_reconnect)), so calling this
    /// on a healthy transport does interrupt it, and a failed attempt
    /// leaves it closed rather than leaving the old one in place.
    ///
    /// What happens after a failure depends on the mode. In
    /// `ServiceLifetime` the transport stays `Reconnecting` and the
    /// supervisor's next tick retries. `LazyAcquire` has no supervisor,
    /// so the flag is cleared instead and the closed conduit is what
    /// callers see: requests fail as closed rather than waiting on a
    /// retry that will never come, and the next 0→1 `acquire()` opens a
    /// fresh one.
    ///
    /// Success has a mode split too, and it is the same one. This
    /// reports what the attempt itself put on the wire; the poll task
    /// it respawns is detached, so a conduit that fails on that task's
    /// first request fails after this has returned `Ok`. In
    /// `ServiceLifetime` the supervisor hears that and recovers. In
    /// `LazyAcquire` nothing is listening, so recovery is the mode's
    /// ordinary one: every session is released and the next acquire
    /// opens a fresh conduit. A caller that needs a retry instead of
    /// that wants `start()`.
    ///
    /// # Errors
    ///
    /// Returns a [`SessionError`] if the reconnect attempt fails to
    /// open the transport or re-run the handshake.
    pub async fn reconnect_now(self: &Arc<Self>) -> Result<(), SessionError<C::Error>> {
        self.reconnecting.store(true, Ordering::SeqCst);
        self.available.store(false, Ordering::SeqCst);

        let mut manual = ManualReconnectGuard {
            reconnecting: &self.reconnecting,
            service_lifetime: &self.service_lifetime,
            armed: true,
        };
        let result = self.attempt_reconnect().await;
        manual.armed = false;
        if result.is_ok() {
            // Availability first, for the reason given on the
            // supervisor's own success arm.
            self.available.store(true, Ordering::SeqCst);
            self.reconnecting.store(false, Ordering::SeqCst);
        } else if self.supervisor_state.lock().await.is_none() {
            // `reconnecting` means "something is going to retry this",
            // and the supervisor is that something. Asking whether one
            // exists is the whole test: `LazyAcquire` never has one, a
            // `ServiceLifetime` transport has one until `shutdown()`
            // takes it, and the mode flag distinguishes neither — it
            // stays true after a shutdown, which is how a
            // `reconnect_now()` on a torn-down transport used to leave
            // the flag set for good.
            //
            // Left set with nobody to clear it, live sessions
            // short-circuit on `Reconnecting` forever and the next
            // `acquire()` skips the terminal check that would have told
            // its caller the transport is not serving, reporting the
            // defensive empty-slot error instead.
            //
            // Clearing it makes the state honest: the conduit is
            // closed, requests say so, and where a fresh open is still
            // possible the next one does it.
            self.reconnecting.store(false, Ordering::SeqCst);
        }
        result
    }

    /// Exit `ServiceLifetime` mode: cancel the while-open task, run
    /// [`Hooks::shutdown`], close the connection (releasing the port).
    /// Called from the service's SIGTERM handler, and by the reload
    /// loop between two runs of the service body.
    ///
    /// Live sessions are not force-closed; their requests fail from
    /// here on. The port is released regardless of how many of them
    /// are still outstanding, so a reload that re-opens the same port
    /// finds it free. The service is still responsible for ordering —
    /// stop accepting new HTTP requests and wait for in-flight clients
    /// to disconnect before calling `shutdown()`.
    ///
    /// No-op in `LazyAcquire` mode (returns `Ok(())` immediately).
    /// After a successful `shutdown()` the transport is back in
    /// `Closed` state; a fresh [`start`](Self::start) re-opens it.
    ///
    /// # Errors
    ///
    /// Currently infallible — teardown steps that misbehave (a task
    /// that ignores cancellation) are logged and absorbed, and the
    /// transport close cannot fail. The `Result` keeps the signature
    /// ready for transports whose close can.
    ///
    /// The conduit is closed explicitly rather than left to the last
    /// `Arc` drop, so this waits for an in-flight request to finish —
    /// bounded by the transport's own I/O timeout, per the contract on
    /// [`FrameTransport`](crate::FrameTransport).
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
                // What this does *not* reach: the supervisor runs each
                // attempt as its own task, and a supervisor stuck long
                // enough to be aborted is usually stuck awaiting one.
                // Aborting the parent drops that child's handle without
                // stopping it, so an attempt wedged in `open()` or a
                // hook outlives this teardown and can still be holding
                // a conduit the next lifecycle wants. Reaching it needs
                // the attempt handle to be lifecycle state rather than
                // a local of the loop that spawned it.
                handle.abort();
                warn!(
                    timeout = ?WHILE_OPEN_TEARDOWN_TIMEOUT,
                    "supervisor task did not respond to cancellation; aborted,                      and an attempt it was awaiting may still be running"
                );
            }
        }

        // Re-assert it, now that the supervisor is joined. The store
        // above is what stops clients promptly, but an attempt already
        // in flight when this call arrived runs to completion inside
        // that join, and a successful one ends by setting
        // `available = true` — after the store, undoing it. A later
        // `start()` would then see an available transport and promote
        // in place rather than cold-starting, leaving the slot this
        // method is about to empty, and every `acquire()` failing on
        // an empty slot for the rest of the process.
        //
        // Deterministic here because the supervisor is *joined*, not
        // merely cancelled. A `reconnect_now()` from another task is
        // not joined by anything and can still land after this; that
        // one needs the attempt itself to know whether the lifecycle it
        // started in is still current.
        self.available.store(false, Ordering::SeqCst);

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

        // Read the cell without taking it, so the slot still names this
        // conduit while the hook runs. The hook is user-supplied: if it
        // panics, the close below never happens, and a conduit that no
        // slot names is one no later open can find — live `Session`s
        // keep their own `Arc` to it, so the port stays held and the
        // reload on the way back up meets `Access is denied`. Left in
        // the slot, the next open's quiesce closes it.
        let cell = {
            let slot = self.slot.lock().await;
            slot.as_ref().map(Arc::clone)
        };
        if let Some(cell) = cell {
            let conn = cell.read().await.clone();
            (self.hooks.shutdown)(&conn).await;
            // Close explicitly rather than leaving it to the last
            // `Arc<Connection<C>>` drop. A `Session` handed out before
            // shutdown keeps its own clone of the cell alive (its
            // requests already refuse, but its reference does not), so
            // waiting for the refcount would leave the conduit open for
            // as long as some device holds a session it never closed —
            // and a service that re-opens the same port on the way back
            // up would find it taken.
            conn.close().await;
            drop(conn);
            drop(cell);
        }

        // Only now: the conduit is closed, so an empty slot is the
        // truth rather than a conduit nobody can reach.
        *self.slot.lock().await = None;

        self.reconnecting.store(false, Ordering::SeqCst);

        // The obligation belongs to the lifecycle that incurred it, and
        // this ends that lifecycle: `Hooks::shutdown` has just made its
        // own terminal safety assertion on the way past. Carrying the
        // flag into a later `start()` would be worse than losing it —
        // that start publishes a fresh conduit and clears
        // `reconnecting`, so the debt would sit outstanding while
        // clients command freely, and then fire a stale halt at the
        // first reconnect, under a live client, which is the one thing
        // the replay must never do.
        //
        // Losing it is not free either: a shutdown whose own stop did
        // not land has no successor to discharge it at all.
        self.safety_state_owed.store(false, Ordering::SeqCst);

        // The cadence clock is lifecycle state as well. Left
        // standing, a `start()` that reuses this transport hands its
        // new supervisor a timestamp from the previous lifecycle, and
        // the first recovery it needs waits out the remainder of a
        // cadence nobody is observing any more.
        *self.last_attempt.lock().await = None;

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
    ///
    /// # Errors
    ///
    /// Returns a [`SessionError`] if the 0→1 open or handshake fails
    /// (`LazyAcquire`), or the transport-shut-down I/O error described
    /// above (`ServiceLifetime` after shutdown).
    pub async fn acquire(self: &Arc<Self>) -> Result<Session<C>, SessionError<C::Error>> {
        let _guard = self.acquire_lock.lock().await;
        let prev = self.count.fetch_add(1, Ordering::SeqCst);

        let service_lifetime = self.service_lifetime.load(Ordering::SeqCst);

        if service_lifetime
            && !self.available.load(Ordering::SeqCst)
            && !self.reconnecting.load(Ordering::SeqCst)
        {
            // ServiceLifetime mode but `shutdown()` already ran. Roll
            // back the speculative increment and refuse — the service
            // is going down and acquiring a new session would defeat
            // the orderly teardown.
            //
            // Not gated on `prev == 0`: `shutdown()` does not
            // force-close live sessions, so a second client arriving
            // after it finds a refcount above zero and an empty slot.
            // Gated, it would fall through to the reuse path and get
            // the defensive "refcount > 0 but slot empty" error, which
            // reads as a bug in this crate rather than as the service
            // going down.
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

            self.release_any_held_conduit().await;

            let raw_transport = self.factory.open().await.map_err(SessionError::Transport)?;
            let connection = Arc::new(
                Connection::new(raw_transport, self.codec.clone())
                    .with_reconnect_signal(self.reconnect_signal.clone()),
            );

            (self.hooks.handshake)(&connection)
                .await
                .map_err(SessionError::Codec)?;

            // Even with no supervisor of its own to wake, this permit
            // outlives the acquire — and a later `start()` promoting
            // this very slot would spawn one that spends it on a
            // healthy live conduit.
            self.drop_pending_reconnect_signal("tolerated by the handshake");

            // Pay any outstanding safety stop before this conduit is
            // exposed. Failing the acquire beats handing back a session
            // on a transport whose mount the halt may not have stopped;
            // the drop guard rolls the refcount back and the next 0→1
            // tries again.
            self.discharge_owed_state(&connection).await?;

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
            // This open *is* the recovery, so clear any reconnecting
            // state it lands on — as `start()` does on its own cold
            // start. `LazyAcquire` has no supervisor to clear the flag
            // later: a failed `reconnect_now()` clears it on its own way
            // out, and this covers the open that races one still in
            // flight. Either way the fresh connection must not inherit a
            // flag that would short-circuit every request on it with
            // `Reconnecting` for the rest of the process's life.
            self.reconnecting.store(false, Ordering::SeqCst);

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
            // `acquire_lock`. In ServiceLifetime mode the post-shutdown
            // case is caught above, whatever the refcount; what is left
            // here is a caller invoking `acquire()` between `start()`
            // failing and the failure propagating, since that rollback
            // leaves `available` and `service_lifetime` false while a
            // successful `start()` would have populated the slot before
            // flipping the flag. Roll back the speculative increment
            // and surface an I/O error rather than panicking.
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
            let before = conn.wire_failures();

            let mut unlanded = UnlandedStateGuard {
                owed: &self.safety_state_owed,
                reconnecting: &self.reconnecting,
                available: &self.available,
                service_lifetime,
                armed: true,
            };
            (self.hooks.on_last_disconnect)(&conn).await;
            unlanded.armed = false;

            // A 1→0 that lands mid-reconnect runs against a conduit
            // that is dead, or closed by the attempt itself, so every
            // command fails and the safety state never reaches the
            // device. Record that it is owed, in both modes: what
            // discharges it differs — a reconnect in `ServiceLifetime`,
            // the next 0→1 open in `LazyAcquire` — but losing it is not
            // one of the options. The handshake that open runs is not a
            // substitute; it is not the safety hook.
            if conn.wire_failures() != before {
                debug!("last-disconnect state did not land; owed to the next open");
                self.safety_state_owed.store(true, Ordering::SeqCst);

                // Recording the debt is not enough on its own in
                // `ServiceLifetime`, where the conduit stays open and
                // the next client could command a mount the halt did
                // not stop. Declare the recovery so requests
                // short-circuit from this moment rather than from
                // whenever the supervisor gets to the notification —
                // and for the closed-conduit case there is no
                // notification at all. The supervisor picks it up on
                // its next tick either way, since the flag is what it
                // loops on.
                //
                // `LazyAcquire` needs none of that: this path closes
                // the conduit and empties the slot on its way out, so
                // there is nothing left to command, and setting
                // `reconnecting` would strand every later request on a
                // retry no supervisor exists to make.
                if service_lifetime {
                    self.reconnecting.store(true, Ordering::SeqCst);
                    self.available.store(false, Ordering::SeqCst);
                }
            }
        }

        if service_lifetime {
            // Port stays open; next acquire reuses the slot connection.
            return Ok(());
        }

        // LazyAcquire mode: close the conduit, then drop the slot's
        // cell. The close is what releases the OS handle — an aborted
        // while_open task can still hold an `Arc<Connection<C>>`, so
        // the cell drop alone does not prove the conduit is gone, and
        // the next `acquire()` opens the same port again.
        let cell = self.slot.lock().await.take();
        if let Some(cell) = cell {
            cell.read().await.close().await;
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use tokio::sync::Notify;

    /// The cold open drains a pending reconnect notification by
    /// enabling a `Notified` and dropping it without awaiting. That
    /// relies on `enable()` consuming the stored permit rather than
    /// handing it back on drop, which is the difference between a
    /// drain and a no-op — worth pinning here rather than trusting a
    /// reading of someone else's documentation.
    #[tokio::test]
    async fn enabling_and_dropping_a_notified_consumes_the_permit() {
        let signal = Notify::new();
        signal.notify_one();

        {
            let taken = signal.notified();
            tokio::pin!(taken);
            assert!(
                taken.as_mut().enable(),
                "the stored permit is there to take"
            );
        }

        assert!(
            tokio::time::timeout(Duration::from_millis(50), signal.notified())
                .await
                .is_err(),
            "nothing should be left for the next waiter"
        );
    }

    /// And the other half: a `Notified` that found no permit
    /// deregisters on drop, rather than staying in the wait list to
    /// catch a later notification meant for someone else.
    #[tokio::test]
    async fn dropping_an_unfilled_notified_leaves_the_next_one_to_it() {
        let signal = Notify::new();

        {
            let empty = signal.notified();
            tokio::pin!(empty);
            assert!(!empty.as_mut().enable(), "nothing stored yet");
        }

        signal.notify_one();

        assert!(
            tokio::time::timeout(Duration::from_millis(50), signal.notified())
                .await
                .is_ok(),
            "the notification must still be waiting for a real listener"
        );
    }
}
