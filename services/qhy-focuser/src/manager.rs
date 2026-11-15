//! Thin manager wrapping `SharedTransport<QhyCodec>` plus the cached
//! focuser state the ASCOM device reads from.
//!
//! Refcount, slot, open/close transitions, command-lock arbitration, and
//! poll-task lifetime all live in
//! [`rusty_photon_shared_transport::SharedTransport`]. What stays here:
//!
//! * The Q-Focuser handshake (`GetVersion` → `SetSpeed` → `GetPosition` →
//!   `ReadTemperature`, seed the cache).
//! * The polling-interval loop body that refreshes position + temperature
//!   and detects move completion.
//! * The cached state the ASCOM `Focuser` properties read (`position`,
//!   `is_moving`, `temperature`, …).
//!
//! Each reading is stored with the instant it was read off the wire, and
//! the cache keeps the reading taken last ([`Stamped`]). Requests take
//! turns on the wire, but each stores its reading after its turn ends, so
//! a poll's position can land after the one `is_moving`'s refresh read at
//! the target; the poll's is then the older one and is dropped. It can
//! also land after a move this driver has since sent: a position read
//! before the device answered a move says nothing about that move, so it
//! cannot end it.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rusty_photon_shared_transport::{
    Connection, Hooks, Session, SessionError, SharedTransport, Stamped, StateAssertion,
    TransportFactory, WhileOpen,
};
use tokio::sync::RwLock;
use tokio::time::{interval, Instant};
use tracing::{debug, warn};

use crate::codec::{QhyCodec, QhyCodecError, QhyResponse};
use crate::config::Config;
use crate::error::{QhyFocuserError, Result};
use crate::protocol::{Command, TemperatureResponse};

/// Cached state from the QHY Q-Focuser device.
#[derive(Debug, Clone, Default)]
pub struct CachedState {
    /// Current focuser position, with the instant it was read off the wire.
    pub position: Stamped<i64>,
    /// Target position for the in-flight move.
    pub target_position: Option<i64>,
    /// True between move start and the position, read after the device
    /// answered the move, that finds the target reached (or a successful
    /// Abort).
    pub is_moving: bool,
    /// When the device last answered a move. A position read before it was
    /// read before the device had that move, and ends no move.
    pub move_answered_at: Option<Instant>,
    /// Moves sent and not yet answered. While one is, no position ends a
    /// move: it may have been read before the move reached the device.
    /// Counted by [`MoveInFlight`], which takes a move off the count
    /// however its request ends.
    pub moves_in_flight: Arc<AtomicU32>,
    /// Outer and chip temperatures (°C) and input voltage (V), the three
    /// values one `ReadTemperature` answer carries, with the instant it was
    /// read off the wire.
    pub temperature: Stamped<TemperatureResponse>,
    /// Firmware version reported by the handshake.
    pub firmware_version: Option<String>,
    /// Board version reported by the handshake.
    pub board_version: Option<String>,
}

/// One move sent and not yet answered, on [`CachedState::moves_in_flight`]
/// for as long as it lives.
///
/// Dropping it takes the move off the count: once its answer is recorded,
/// on a failure, or with the caller's future when a client hangs up
/// mid-request. A move nobody waits for any more therefore cannot keep a
/// position at its target from ending it, and `is_moving` at true.
struct MoveInFlight(Arc<AtomicU32>);

impl MoveInFlight {
    fn start(count: &Arc<AtomicU32>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(count))
    }
}

impl Drop for MoveInFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Manager that wraps the shared transport plus Q-Focuser-specific
/// cached state. One instance per process; the ASCOM device holds
/// `Arc<FocuserManager>`.
pub struct FocuserManager {
    transport: Arc<SharedTransport<QhyCodec>>,
    cached_state: Arc<RwLock<CachedState>>,
}

impl FocuserManager {
    pub fn new(config: &Config, factory: Arc<dyn TransportFactory>) -> Arc<Self> {
        let cached_state = Arc::new(RwLock::new(CachedState::default()));
        let hooks = build_hooks(
            &cached_state,
            config.focuser.speed,
            config.serial.polling_interval,
        );
        let transport = SharedTransport::new(factory, QhyCodec, hooks);
        Arc::new(Self {
            transport,
            cached_state,
        })
    }

    /// Access the shared transport so the device can acquire sessions.
    #[must_use]
    pub const fn transport(&self) -> &Arc<SharedTransport<QhyCodec>> {
        &self.transport
    }

    /// Cheap, non-blocking snapshot — true between handshake completion
    /// and the start of teardown.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.transport.is_available()
    }

    /// Clone the current cached state for read-only consumers.
    pub async fn get_cached_state(&self) -> CachedState {
        self.cached_state.read().await.clone()
    }

    /// Issue an absolute-move command over the device's session and
    /// update the cached `is_moving` / `target_position`. The poll loop
    /// clears these once the device reports the target reached, in a
    /// position read after the device answered this command.
    ///
    /// On transport failure the cache is rolled back to its pre-move
    /// state so a transient wire failure doesn't wedge ASCOM `IsMoving`
    /// at `true` against a target the device never received. The rollback
    /// is gated by [`rollback_move_if_ours`] so a concurrent later call
    /// that already committed a *different* target to the cache isn't
    /// clobbered by this call's failure.
    ///
    /// # Errors
    ///
    /// Returns the [`Session::request_timed`] failure as a
    /// [`QhyFocuserError`]: a transport error, a response the codec cannot
    /// decode, or an exhausted skip budget.
    pub async fn move_absolute(&self, session: &Session<QhyCodec>, position: i64) -> Result<()> {
        // Set cache state before sending so a racing `is_moving` read
        // can't observe `is_moving == false` while the move is in flight.
        let in_flight = {
            let mut state = self.cached_state.write().await;
            state.target_position = Some(position);
            state.is_moving = true;
            MoveInFlight::start(&state.moves_in_flight)
        };
        let outcome = session
            .request_timed(Command::AbsoluteMove { position })
            .await;
        let mut state = self.cached_state.write().await;
        // Off the count only now, with the cache held until the answer's
        // date is in: a position read before the answer meets the count or
        // the date, never a moment with neither guard up.
        drop(in_flight);
        let timing = match outcome {
            Ok((_, timing)) => timing,
            Err(e) => {
                rollback_move_if_ours(&mut state, position);
                return Err(QhyFocuserError::from(e));
            }
        };
        // Date the move by its answer, so a position read before it, which
        // can equal the target with the focuser still on its way there,
        // cannot end the move.
        record_move_answer(&mut state, timing.received_at);
        drop(state);
        debug!(position, "absolute-move command sent");
        Ok(())
    }

    /// Abort the in-flight move and clear `is_moving` / `target_position`.
    ///
    /// # Errors
    ///
    /// Returns the [`Session::request`] failure as a [`QhyFocuserError`]: a
    /// transport error, a response the codec cannot decode, or an exhausted
    /// skip budget. The cached move state is left as it was.
    pub async fn abort(&self, session: &Session<QhyCodec>) -> Result<()> {
        session
            .request(Command::Abort)
            .await
            .map_err(QhyFocuserError::from)?;
        let mut state = self.cached_state.write().await;
        state.is_moving = false;
        state.target_position = None;
        drop(state);
        debug!("abort command sent");
        Ok(())
    }

    /// Force-refresh the cached position by issuing a `GetPosition` over
    /// the device's session — used by `is_moving` to avoid waiting up to
    /// one polling interval for move-completion detection.
    ///
    /// # Errors
    ///
    /// Returns the [`Session::request`] failure as a [`QhyFocuserError`] (a
    /// transport error, a response the codec cannot decode, or an exhausted
    /// skip budget), or [`QhyFocuserError::InvalidResponse`] if the reply is
    /// not a position frame.
    pub async fn refresh_position(&self, session: &Session<QhyCodec>) -> Result<()> {
        let (resp, timing) = session
            .request_timed(Command::GetPosition)
            .await
            .map_err(QhyFocuserError::from)?;
        let position = match resp {
            QhyResponse::Position(p) => p.position,
            other => {
                return Err(QhyFocuserError::InvalidResponse(format!(
                    "GetPosition returned non-Position frame: {other:?}"
                )));
            }
        };
        let mut state = self.cached_state.write().await;
        apply_position(&mut state, position, timing.received_at);
        Ok(())
    }
}

fn build_hooks(
    cached_state: &Arc<RwLock<CachedState>>,
    speed: u8,
    poll_interval: Duration,
) -> Hooks<QhyCodec> {
    let cs_handshake = Arc::clone(cached_state);
    let cs_poll = Arc::clone(cached_state);
    Hooks {
        handshake: Box::new(move |conn| {
            let cs = Arc::clone(&cs_handshake);
            Box::pin(handshake(conn, cs, speed))
        }),
        on_last_disconnect: Box::new(|_| {
            // Nothing to assert: this service holds no state that must
            // survive a no-client gap, so the answer is trivially yes.
            Box::pin(async { StateAssertion::Asserted })
        }),
        shutdown: Box::new(|_| Box::pin(async {})),
        while_open: Some(Box::new(move |ctx| {
            let cs = Arc::clone(&cs_poll);
            Box::pin(poll_loop(ctx, cs, poll_interval))
        })),
    }
}

async fn handshake(
    conn: &Connection<QhyCodec>,
    cached_state: Arc<RwLock<CachedState>>,
    speed: u8,
) -> std::result::Result<(), QhyCodecError> {
    let version = match conn.request(Command::GetVersion).await? {
        QhyResponse::Version(v) => v,
        other => {
            return Err(QhyCodecError::InvalidResponse(format!(
                "GetVersion returned non-Version frame: {other:?}"
            )));
        }
    };
    debug!(
        firmware = %version.firmware_version,
        board = %version.board_version,
        "Q-Focuser handshake: version"
    );

    conn.request(Command::SetSpeed { speed }).await?;
    debug!(speed, "Q-Focuser handshake: speed set");

    let (position, position_read_at) = match conn.request_timed(Command::GetPosition).await? {
        (QhyResponse::Position(p), timing) => (p, timing.received_at),
        (other, _) => {
            return Err(QhyCodecError::InvalidResponse(format!(
                "GetPosition returned non-Position frame: {other:?}"
            )));
        }
    };
    debug!(
        position = position.position,
        "Q-Focuser handshake: position"
    );

    let (temp, temp_read_at) = match conn.request_timed(Command::ReadTemperature).await? {
        (QhyResponse::Temperature(t), timing) => (t, timing.received_at),
        (other, _) => {
            return Err(QhyCodecError::InvalidResponse(format!(
                "ReadTemperature returned non-Temperature frame: {other:?}"
            )));
        }
    };
    debug!(
        outer_temp = temp.outer_temp,
        voltage = temp.voltage,
        "Q-Focuser handshake: temperature"
    );

    let mut state = cached_state.write().await;
    state.firmware_version = Some(version.firmware_version);
    state.board_version = Some(version.board_version);
    apply_temperature(&mut state, temp, temp_read_at);
    // Reset move state on every new session: a previous session may
    // have disconnected mid-move, leaving the cache claiming
    // `is_moving = true, target = Some(X)`. Without this reset, the
    // poll loop's apply_position would never clear the stale flag (the
    // device is likely no longer moving toward X, so position will
    // never match target). The protocol has no "are you moving?" query,
    // so we default to "not moving" — the next user-initiated `Move()`
    // will set fresh state, and `Position` polling reflects reality
    // regardless of `is_moving`.
    state.is_moving = false;
    state.target_position = None;
    apply_position(&mut state, position.position, position_read_at);
    drop(state);
    Ok(())
}

async fn poll_loop(
    ctx: WhileOpen<QhyCodec>,
    cached_state: Arc<RwLock<CachedState>>,
    poll_interval: Duration,
) {
    let mut ticker = interval(poll_interval);
    // Skip the immediate first tick — handshake just populated the cache.
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = ctx.cancelled() => {
                debug!("Q-Focuser poll loop received cancellation");
                return;
            }
        }

        match ctx.request_timed(Command::GetPosition).await {
            Ok((QhyResponse::Position(p), timing)) => {
                let mut state = cached_state.write().await;
                apply_position(&mut state, p.position, timing.received_at);
            }
            Ok((other, _)) => warn!("poll: GetPosition returned unexpected variant: {other:?}"),
            Err(e) => session_err_to_warn("GetPosition", &e),
        }

        match ctx.request_timed(Command::ReadTemperature).await {
            Ok((QhyResponse::Temperature(t), timing)) => {
                let mut state = cached_state.write().await;
                apply_temperature(&mut state, t, timing.received_at);
            }
            Ok((other, _)) => warn!("poll: ReadTemperature returned unexpected variant: {other:?}"),
            Err(e) => session_err_to_warn("ReadTemperature", &e),
        }
    }
}

fn session_err_to_warn(op: &str, err: &SessionError<QhyCodecError>) {
    warn!(op, error = %err, "Q-Focuser poll request failed");
}

/// Roll back the cache from `move_absolute`'s pre-send commit ONLY if
/// the cache still claims this caller's target.
///
/// ASCOM `Move` is `&self` and the device-layer `move_()` takes a
/// *read* lock on the session slot, so two clients can be inside
/// `move_absolute` concurrently. If a later call wrote its own target
/// between our cache update and our wire failure, that later call's
/// commit must not be reverted by our rollback. The conditional check
/// makes the rollback a no-op in that interleaving.
fn rollback_move_if_ours(state: &mut CachedState, our_target: i64) {
    if state.target_position == Some(our_target) {
        state.is_moving = false;
        state.target_position = None;
    }
}

/// Record that the device answered a move at `answered_at`. The cache
/// keeps the latest answer: moves take turns on the wire, but each caller
/// records its answer after its turn ends, so two clients' answers can be
/// recorded out of wire order.
fn record_move_answer(state: &mut CachedState, answered_at: Instant) {
    state.move_answered_at = state.move_answered_at.max(Some(answered_at));
}

/// Update the cached position and clear `is_moving` if the target was
/// reached. Kept in one place so `refresh_position` and the poll loop
/// stay in lockstep on move-completion semantics.
///
/// A position read before the cached one is dropped: stored, a poll's
/// reading taken on the way to the target would replace the one
/// `is_moving`'s refresh took at the target, and the focuser would report
/// that it stopped short of where it says it arrived.
///
/// A position at the target ends the move unless it was read before the
/// device answered the move. Read before, it shows where the focuser was
/// before it had the move: a client that sends the focuser to the step a
/// poll has just read would see the move end with the focuser still
/// travelling, and `is_moving` would answer false without asking the
/// device again. While a move is unanswered nothing dates it yet, and no
/// position ends it.
fn apply_position(state: &mut CachedState, position: i64, read_at: Instant) {
    if !state.position.offer(position, read_at) {
        debug!(position, "dropping a position read before the cached one");
        return;
    }
    if !state.is_moving || state.target_position != Some(position) {
        return;
    }
    if state.moves_in_flight.load(Ordering::SeqCst) > 0 {
        debug!(position, "at the target, with a move not yet answered");
        return;
    }
    if state
        .move_answered_at
        .is_some_and(|answered_at| read_at < answered_at)
    {
        debug!(position, "at the target, read before the move was answered");
        return;
    }
    debug!(position, "move complete: target reached");
    state.is_moving = false;
    state.target_position = None;
}

/// Store the temperatures and the voltage read at `read_at`, unless the
/// cache holds a reading taken after them.
fn apply_temperature(state: &mut CachedState, temp: TemperatureResponse, read_at: Instant) {
    if !state.temperature.offer(temp, read_at) {
        debug!("dropping a temperature reading taken before the cached one");
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    //! Behaviour-level tests for the manager, driven through the mock
    //! transport factory. Race / refcount / rollback invariants are
    //! tested once for everyone in `rusty-photon-shared-transport` —
    //! they don't get re-tested here per the migration plan.

    use super::*;
    use crate::mock::MockQhyTransportFactory;

    fn make_manager() -> Arc<FocuserManager> {
        let factory = Arc::new(MockQhyTransportFactory::default());
        FocuserManager::new(&Config::default(), factory)
    }

    #[tokio::test]
    async fn acquire_runs_handshake_and_seeds_cache() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();
        assert!(manager.is_available());

        let state = manager.get_cached_state().await;
        assert_eq!(state.firmware_version.as_deref(), Some("2.1.0"));
        assert_eq!(state.board_version.as_deref(), Some("1.0"));
        assert_eq!(state.position.value(), Some(0));
        let temperature = state.temperature.get().unwrap();
        assert!((temperature.outer_temp - 25.0).abs() < 1e-6);
        assert!((temperature.voltage - 12.5).abs() < 1e-6);
        assert!(!state.is_moving);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn handshake_resets_stale_move_state_from_prior_session() {
        // Simulate a previous session that disconnected mid-move: the
        // cache still claims `is_moving = true, target = Some(X)`. A
        // reconnect runs the handshake — without an explicit reset the
        // poll loop would never clear `is_moving` (the device, having
        // been disconnected, is no longer moving toward X and the
        // position will never match the stale target).
        let manager = make_manager();

        // Seed the "stale from a prior session" state.
        {
            let mut state = manager.cached_state.write().await;
            state.is_moving = true;
            state.target_position = Some(12345);
        }

        let session = manager.transport().acquire().await.unwrap();

        // Handshake must have cleared the stale move state.
        let state = manager.get_cached_state().await;
        assert!(
            !state.is_moving,
            "handshake must clear is_moving from prior session"
        );
        assert_eq!(
            state.target_position, None,
            "handshake must clear target_position from prior session"
        );

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn move_absolute_sets_target_and_is_moving() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();

        manager.move_absolute(&session, 5000).await.unwrap();
        let state = manager.get_cached_state().await;
        assert_eq!(state.target_position, Some(5000));
        assert!(state.is_moving);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_position_detects_move_completion_when_target_reached() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();

        // Mock snaps to target when |target - position| <= 1000 on each
        // GetPosition; with a starting position of 0 a 500-step move
        // completes on the first refresh.
        manager.move_absolute(&session, 500).await.unwrap();
        manager.refresh_position(&session).await.unwrap();

        let state = manager.get_cached_state().await;
        assert_eq!(state.position.value(), Some(500));
        assert!(!state.is_moving);
        assert_eq!(state.target_position, None);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn abort_clears_move_state() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();

        manager.move_absolute(&session, 50000).await.unwrap();
        manager.abort(&session).await.unwrap();

        let state = manager.get_cached_state().await;
        assert!(!state.is_moving);
        assert_eq!(state.target_position, None);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn close_releases_transport() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();
        assert!(manager.is_available());
        session.close().await.unwrap();
        assert!(!manager.is_available());
    }

    #[test]
    fn session_err_to_warn_logs_without_panicking() {
        // `session_err_to_warn` is only invoked from the poll loop's Err
        // arms — which never fire in tests because the mock factory always
        // succeeds. The function emits a `warn!` and returns nothing
        // observable, so a direct call is the simplest way to keep the
        // log-only helper covered.
        session_err_to_warn(
            "GetPosition",
            &SessionError::Transport(rusty_photon_shared_transport::TransportError::Eof),
        );
    }

    // ========================================================================
    // move_absolute cache rollback on transport failure
    //
    // Verifies that a transient transport failure during AbsoluteMove
    // doesn't leave the cache stuck at `is_moving = true` with an
    // unreachable target (poll would never clear it).
    // ========================================================================

    use async_trait::async_trait;
    use rusty_photon_shared_transport::{
        Codec as _, FrameTransport, TransportError, TransportFactory,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Wraps the canonical mock factory but gates `send_frame` behind a
    /// shared atomic — flipping it makes the very next send return EOF.
    /// Used to inject a wire-level failure after handshake has succeeded.
    #[derive(Default, Clone)]
    struct InjectableFactory {
        inner: MockQhyTransportFactory,
        fail_next_send: Arc<AtomicBool>,
    }

    impl InjectableFactory {
        fn fail_next_send(&self) -> Arc<AtomicBool> {
            Arc::clone(&self.fail_next_send)
        }
    }

    #[async_trait]
    impl TransportFactory for InjectableFactory {
        async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
            let inner = self.inner.open().await?;
            Ok(Box::new(InjectableTransport {
                inner,
                fail_next_send: Arc::clone(&self.fail_next_send),
            }))
        }
    }

    struct InjectableTransport {
        inner: Box<dyn FrameTransport>,
        fail_next_send: Arc<AtomicBool>,
    }

    #[async_trait]
    impl FrameTransport for InjectableTransport {
        async fn send_frame(&mut self, bytes: &[u8]) -> std::result::Result<(), TransportError> {
            if self.fail_next_send.swap(false, Ordering::SeqCst) {
                return Err(TransportError::Eof);
            }
            self.inner.send_frame(bytes).await
        }

        async fn recv_frame(
            &mut self,
            buf: &mut Vec<u8>,
        ) -> std::result::Result<(), TransportError> {
            self.inner.recv_frame(buf).await
        }
    }

    // ---- a poll against a refresh ---------------------------------------

    use transport_doubles::HoldingFactory;

    #[tokio::test]
    async fn a_poll_read_short_of_the_target_does_not_replace_the_refreshed_arrival() {
        let factory = Arc::new(HoldingFactory::new(MockQhyTransportFactory::default()));
        let mut config = Config::default();
        config.serial.polling_interval = Duration::from_millis(20);
        let dyn_factory: Arc<dyn TransportFactory> = Arc::<HoldingFactory<_>>::clone(&factory);
        let manager = FocuserManager::new(&config, dyn_factory);
        let session = manager.transport().acquire().await.unwrap();

        // The mock steps 1000 per position read: the poll's read lands at
        // 1000, short of the target.
        manager.move_absolute(&session, 1500).await.unwrap();
        factory.hold_next_reply_to(QhyCodec.encode(&Command::GetPosition));
        factory.reply_held().await;

        // With the cache held, the poll reads 1000 and cannot store it;
        // `is_moving`'s refresh goes on the wire behind it, reads the
        // target and lands first — what a poll task slow to run again on a
        // loaded host does.
        let mut cache = manager.cached_state.write().await;
        factory.release_reply();
        let (refreshed, timing) = session.request_timed(Command::GetPosition).await.unwrap();
        let QhyResponse::Position(refreshed) = refreshed else {
            panic!("GetPosition answered by {refreshed:?}");
        };
        assert_eq!(refreshed.position, 1500, "the refresh read the target");
        apply_position(&mut cache, refreshed.position, timing.received_at);
        assert!(!cache.is_moving, "the refresh saw the move complete");

        // The poll stores next, and its next position read is the sign it
        // has.
        factory.hold_next_reply_to(QhyCodec.encode(&Command::GetPosition));
        drop(cache);
        factory.reply_held().await;

        let state = manager.get_cached_state().await;
        assert_eq!(
            state.position.value(),
            Some(1500),
            "a poll's position read before the refresh replaced the one at the target"
        );
        assert!(!state.is_moving);

        factory.release_reply();
        session.close().await.unwrap();
    }

    #[test]
    fn a_position_read_before_the_cached_one_is_dropped_and_ends_no_move() {
        let earlier = Instant::now();
        let later = earlier.checked_add(Duration::from_millis(1)).unwrap();
        let mut state = CachedState {
            target_position: Some(1500),
            is_moving: true,
            ..Default::default()
        };

        // The older reading is the one at the target: stored, it would end
        // a move the newer reading shows under way.
        apply_position(&mut state, 1000, later);
        apply_position(&mut state, 1500, earlier);

        assert_eq!(
            state.position.value(),
            Some(1000),
            "the older position replaced the newer one"
        );
        assert_eq!(state.position.read_at(), Some(later));
        assert!(state.is_moving, "a dropped position ended the move");
        assert_eq!(state.target_position, Some(1500));
    }

    #[test]
    fn a_temperature_reading_taken_before_the_cached_one_is_dropped() {
        let earlier = Instant::now();
        let later = earlier.checked_add(Duration::from_millis(1)).unwrap();
        let mut state = CachedState::default();
        let outer = |outer_temp| TemperatureResponse {
            outer_temp,
            chip_temp: 30.0,
            voltage: 12.5,
        };

        apply_temperature(&mut state, outer(20.0), later);
        apply_temperature(&mut state, outer(25.0), earlier);

        assert_eq!(
            state.temperature.get(),
            Some(&outer(20.0)),
            "the older reading replaced the newer one"
        );
        assert_eq!(state.temperature.read_at(), Some(later));
    }

    /// Build a manager whose poll loop is effectively disabled (300s
    /// interval), so only the test's own requests go on the wire: an armed
    /// failure or a held reply meets the request the test sends, not a
    /// poll's.
    fn make_manager_with_factory<F>(factory: Arc<F>) -> Arc<FocuserManager>
    where
        F: TransportFactory + 'static,
    {
        let mut config = Config::default();
        config.serial.polling_interval = Duration::from_mins(5);
        FocuserManager::new(&config, factory)
    }

    // ---- a position read before a move ----------------------------------

    #[tokio::test]
    async fn a_position_read_before_a_move_was_sent_does_not_end_it() {
        let manager = make_manager_with_factory(Arc::new(InjectableFactory::default()));
        let session = manager.transport().acquire().await.unwrap();

        // The focuser is on its way to 5000, and a reader has 1000 in hand
        // that it has not stored yet: what a poll task slow to run again on
        // a loaded host holds.
        manager.move_absolute(&session, 5000).await.unwrap();
        let (in_hand, timing) = session.request_timed(Command::GetPosition).await.unwrap();
        let QhyResponse::Position(in_hand) = in_hand else {
            panic!("GetPosition answered by {in_hand:?}");
        };
        assert_eq!(in_hand.position, 1000, "the reader's position");

        // A client sends the focuser to the very step the reader holds,
        // and the reader stores it afterwards, as the poll loop does.
        manager.move_absolute(&session, 1000).await.unwrap();
        apply_position(
            &mut *manager.cached_state.write().await,
            in_hand.position,
            timing.received_at,
        );

        let state = manager.get_cached_state().await;
        assert!(
            state.is_moving,
            "a position read before the move was sent ended it"
        );
        assert_eq!(state.target_position, Some(1000));
        assert_eq!(state.position.value(), Some(1000), "the position is stored");

        // A position read after the move's answer finds the focuser at the
        // target, and ends the move.
        manager.refresh_position(&session).await.unwrap();
        let state = manager.get_cached_state().await;
        assert!(!state.is_moving);
        assert_eq!(state.target_position, None);

        session.close().await.unwrap();
    }

    #[test]
    fn a_position_at_the_target_read_before_the_move_was_answered_ends_no_move() {
        let read_at = Instant::now();
        let answered_at = read_at.checked_add(Duration::from_millis(1)).unwrap();
        let mut state = CachedState {
            target_position: Some(1000),
            is_moving: true,
            move_answered_at: Some(answered_at),
            ..Default::default()
        };

        apply_position(&mut state, 1000, read_at);

        assert!(
            state.is_moving,
            "a position read before the move was answered ended it"
        );
        assert_eq!(state.target_position, Some(1000));
        assert_eq!(state.position.value(), Some(1000), "the position is stored");
        assert_eq!(state.position.read_at(), Some(read_at));
    }

    #[test]
    fn a_position_at_the_target_read_as_the_move_was_answered_ends_it() {
        let answered_at = Instant::now();
        let mut state = CachedState {
            target_position: Some(1000),
            is_moving: true,
            move_answered_at: Some(answered_at),
            ..Default::default()
        };

        // Only a strictly earlier read is refused, the rule `ReadStamp`
        // keeps for readings: reads at one instant, every read on a paused
        // test clock, still end a move.
        apply_position(&mut state, 1000, answered_at);

        assert!(
            !state.is_moving,
            "a position read at the answer's instant did not end the move"
        );
        assert_eq!(state.target_position, None);
    }

    #[test]
    fn the_cache_keeps_the_later_of_two_move_answers() {
        let earlier = Instant::now();
        let later = earlier.checked_add(Duration::from_millis(1)).unwrap();
        let mut state = CachedState::default();

        // Two clients' moves, recorded in the other order than they were
        // answered.
        record_move_answer(&mut state, later);
        record_move_answer(&mut state, earlier);

        assert_eq!(
            state.move_answered_at,
            Some(later),
            "the earlier answer replaced the later one"
        );
    }

    #[tokio::test]
    async fn a_position_at_the_target_does_not_end_a_move_the_device_has_not_answered() {
        let factory = Arc::new(HoldingFactory::new(MockQhyTransportFactory::default()));
        let manager = make_manager_with_factory(Arc::clone(&factory));
        let session = manager.transport().acquire().await.unwrap();

        factory.hold_next_reply_to(QhyCodec.encode(&Command::AbsoluteMove { position: 1000 }));
        let mut moving = Box::pin(manager.move_absolute(&session, 1000));
        tokio::select! {
            () = factory.reply_held() => {}
            outcome = &mut moving => panic!("the move ended with its reply held: {outcome:?}"),
        }

        // The device has the move and has not answered it. A reader stores
        // a position at the target in the meantime, which it may have read
        // before the move was sent.
        apply_position(
            &mut *manager.cached_state.write().await,
            1000,
            Instant::now(),
        );
        let state = manager.get_cached_state().await;
        assert!(
            state.is_moving,
            "a position ended a move the device had not answered"
        );
        assert_eq!(state.target_position, Some(1000));

        // Answered, the move is ended by the next position at the target.
        factory.release_reply();
        moving.await.unwrap();
        manager.refresh_position(&session).await.unwrap();
        let state = manager.get_cached_state().await;
        assert!(!state.is_moving);
        assert_eq!(state.target_position, None);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_move_whose_caller_stopped_waiting_does_not_hold_is_moving() {
        let factory = Arc::new(HoldingFactory::new(MockQhyTransportFactory::default()));
        let manager = make_manager_with_factory(Arc::clone(&factory));
        let session = manager.transport().acquire().await.unwrap();

        factory.hold_next_reply_to(QhyCodec.encode(&Command::AbsoluteMove { position: 1000 }));
        let mut moving = Box::pin(manager.move_absolute(&session, 1000));
        tokio::select! {
            () = factory.reply_held() => {}
            outcome = &mut moving => panic!("the move ended with its reply held: {outcome:?}"),
        }

        // The client hangs up: its handler's future goes, with the move on
        // the wire and nobody left to record its answer.
        drop(moving);
        assert!(manager.get_cached_state().await.is_moving);

        // The exchange runs to its reply all the same, and the next
        // position at the target ends the move.
        factory.release_reply();
        manager.refresh_position(&session).await.unwrap();
        let state = manager.get_cached_state().await;
        assert!(
            !state.is_moving,
            "a move nobody waits for held is_moving at true"
        );
        assert_eq!(state.target_position, None);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn move_absolute_rolls_cache_back_on_transport_failure() {
        // Slow the poll loop down so it can't fire and consume the
        // armed failure before move_absolute does.
        let factory = Arc::new(InjectableFactory::default());
        let fail_switch = factory.fail_next_send();
        let manager = make_manager_with_factory(Arc::clone(&factory));
        let session = manager.transport().acquire().await.unwrap();

        // Arm: the next send (the AbsoluteMove issued by move_absolute)
        // will return EOF from the transport.
        fail_switch.store(true, Ordering::SeqCst);

        let err = manager
            .move_absolute(&session, 5000)
            .await
            .expect_err("move_absolute should propagate the transport failure");
        assert!(
            matches!(err, QhyFocuserError::Communication(_)),
            "expected Communication (Eof maps to it), got {err:?}"
        );

        // The whole point of this test: the cache must reflect "no move
        // in progress" so a polling refresh / ASCOM IsMoving read sees
        // the truth.
        let state = manager.get_cached_state().await;
        assert!(
            !state.is_moving,
            "cache should be rolled back to is_moving=false"
        );
        assert_eq!(
            state.target_position, None,
            "target_position should be rolled back"
        );

        session.close().await.unwrap();
    }

    // ========================================================================
    // rollback_move_if_ours — verifies the conditional check that protects
    // against concurrent move_absolute calls clobbering each other.
    //
    // The integration of the helper is covered by
    // move_absolute_rolls_cache_back_on_transport_failure above (matching-
    // target case). These tests exercise the conditional directly without
    // needing to set up a deterministic concurrent ordering of two tokio
    // tasks racing into Connection::request.
    // ========================================================================

    #[test]
    fn rollback_move_if_ours_clears_when_cache_target_matches() {
        let mut state = CachedState {
            target_position: Some(5000),
            is_moving: true,
            ..Default::default()
        };
        rollback_move_if_ours(&mut state, 5000);
        assert_eq!(state.target_position, None);
        assert!(!state.is_moving);
    }

    #[test]
    fn rollback_move_if_ours_skips_when_cache_target_differs() {
        // Simulates the concurrent-move race: a later call already
        // committed its own target (9000) to the cache, and now an
        // earlier call's rollback is running with our_target = 5000.
        // The conditional must skip — the later commit stays intact.
        let mut state = CachedState {
            target_position: Some(9000),
            is_moving: true,
            ..Default::default()
        };
        rollback_move_if_ours(&mut state, 5000);
        assert_eq!(state.target_position, Some(9000));
        assert!(state.is_moving);
    }

    #[test]
    fn rollback_move_if_ours_skips_when_cache_target_is_none() {
        // Defensive — if the cache is already cleared (e.g. by a
        // concurrent abort), the rollback is also a no-op.
        let mut state = CachedState {
            target_position: None,
            is_moving: false,
            ..Default::default()
        };
        rollback_move_if_ours(&mut state, 5000);
        assert_eq!(state.target_position, None);
        assert!(!state.is_moving);
    }

    #[tokio::test]
    async fn abort_propagates_transport_failure_and_leaves_cache_unchanged() {
        // abort's contract: on success, clear is_moving / target_position.
        // On transport failure, propagate the error and leave the cache
        // untouched (the device may still be moving — better to reflect
        // that uncertainty than lie about completion).
        let factory = Arc::new(InjectableFactory::default());
        let fail_switch = factory.fail_next_send();
        let manager = make_manager_with_factory(Arc::clone(&factory));
        let session = manager.transport().acquire().await.unwrap();

        // Put the cache into a "moving" state via a successful move.
        manager.move_absolute(&session, 5000).await.unwrap();
        assert!(manager.get_cached_state().await.is_moving);

        fail_switch.store(true, Ordering::SeqCst);
        let err = manager
            .abort(&session)
            .await
            .expect_err("abort should propagate the transport failure");
        assert!(
            matches!(err, QhyFocuserError::Communication(_)),
            "expected Communication, got {err:?}"
        );

        let state = manager.get_cached_state().await;
        assert!(
            state.is_moving,
            "cache should NOT have been cleared on abort failure"
        );
        assert_eq!(state.target_position, Some(5000));

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_position_propagates_transport_failure() {
        let factory = Arc::new(InjectableFactory::default());
        let fail_switch = factory.fail_next_send();
        let manager = make_manager_with_factory(Arc::clone(&factory));
        let session = manager.transport().acquire().await.unwrap();

        fail_switch.store(true, Ordering::SeqCst);
        let err = manager
            .refresh_position(&session)
            .await
            .expect_err("refresh_position should propagate the transport failure");
        assert!(
            matches!(err, QhyFocuserError::Communication(_)),
            "expected Communication, got {err:?}"
        );

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_position_advances_position_but_stays_moving_when_target_unreached() {
        // Covers the apply_position branch where is_moving=true and
        // position != target — the function must update position but
        // leave is_moving=true so ASCOM IsMoving keeps reporting `true`
        // mid-move. The mock advances 1000 steps per GetPosition, so a
        // 5000-step move's first refresh stays in flight.
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();

        manager.move_absolute(&session, 5000).await.unwrap();
        manager.refresh_position(&session).await.unwrap();

        let state = manager.get_cached_state().await;
        assert_eq!(state.position.value(), Some(1000));
        assert!(state.is_moving);
        assert_eq!(state.target_position, Some(5000));

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn acquire_returns_err_and_keeps_cache_empty_when_handshake_send_fails() {
        // Failing the first handshake send (GetVersion) must:
        // - propagate Err from acquire(),
        // - leave is_available() == false (the RollbackGuard fired),
        // - leave the cache at Default::default() (no fields populated
        //   from a partial handshake).
        let factory = Arc::new(InjectableFactory::default());
        let fail_switch = factory.fail_next_send();
        let manager = make_manager_with_factory(Arc::clone(&factory));

        fail_switch.store(true, Ordering::SeqCst);
        let err = manager
            .transport()
            .acquire()
            .await
            .expect_err("handshake failure should propagate out of acquire");
        // The send returned TransportError::Eof; that arrives at the
        // device layer as SessionError::Codec(QhyCodecError::Transport(
        // TransportError::Eof)), which transport_to_focuser maps to
        // QhyFocuserError::Communication("Connection closed").
        let mapped = QhyFocuserError::from(err);
        assert!(
            matches!(mapped, QhyFocuserError::Communication(_)),
            "expected Communication, got {mapped:?}"
        );

        assert!(
            !manager.is_available(),
            "RollbackGuard should have rolled the refcount back"
        );

        let state = manager.get_cached_state().await;
        assert_eq!(state.firmware_version, None);
        assert_eq!(state.board_version, None);
        assert_eq!(state.position.value(), None);
        assert_eq!(state.temperature.get(), None);
    }
}
