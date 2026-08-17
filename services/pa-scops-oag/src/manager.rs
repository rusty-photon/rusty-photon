//! Thin manager wrapping `SharedTransport<ScopsCodec>` plus the cached focuser
//! state the ASCOM device reads from.
//!
//! Refcount, slot, open/close transitions, command-lock arbitration, and
//! poll-task lifetime all live in
//! [`rusty_photon_shared_transport::SharedTransport`]. What stays here:
//!
//! * The Scops handshake (`#` identity check → `A` status to seed the cache).
//! * The polling-interval loop body that refreshes position + moving state from
//!   the `A` report.
//! * The cached state the ASCOM `Focuser` properties read (`position`,
//!   `is_moving`).

use std::sync::Arc;
use std::time::Duration;

use rusty_photon_shared_transport::{
    Connection, Hooks, Session, SessionError, SharedTransport, TransportFactory, WhileOpen,
};
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, warn};

use crate::codec::{ScopsCodec, ScopsCodecError, ScopsResponse};
use crate::config::Config;
use crate::error::{Result, ScopsOagError};
use crate::protocol::{validate_echo, Command, ScopsStatus};

/// Cached state from the Scops OAG device.
#[derive(Debug, Clone, Default)]
pub struct CachedState {
    /// Current focuser position.
    pub position: Option<i64>,
    /// Target position for the in-flight move.
    pub target_position: Option<i64>,
    /// True between move start and the poll/refresh that observes the device
    /// reporting idle (or a successful Halt).
    pub is_moving: bool,
    /// Firmware version reported by the handshake's status report.
    pub firmware_version: Option<String>,
}

/// Manager that wraps the shared transport plus Scops-specific cached state.
/// One instance per process; the ASCOM device holds `Arc<FocuserManager>`.
pub struct FocuserManager {
    transport: Arc<SharedTransport<ScopsCodec>>,
    cached_state: Arc<RwLock<CachedState>>,
}

impl FocuserManager {
    pub fn new(config: &Config, factory: Arc<dyn TransportFactory>) -> Arc<Self> {
        let cached_state = Arc::new(RwLock::new(CachedState::default()));
        let hooks = build_hooks(&cached_state, config.serial.polling_interval);
        let transport = SharedTransport::new(factory, ScopsCodec, hooks);
        Arc::new(Self {
            transport,
            cached_state,
        })
    }

    /// Access the shared transport so the device can acquire sessions.
    #[must_use]
    pub const fn transport(&self) -> &Arc<SharedTransport<ScopsCodec>> {
        &self.transport
    }

    /// Cheap, non-blocking snapshot — true between handshake completion and the
    /// start of teardown.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.transport.is_available()
    }

    /// Clone the current cached state for read-only consumers.
    pub async fn get_cached_state(&self) -> CachedState {
        self.cached_state.read().await.clone()
    }

    /// Issue an absolute-move command and update the cached `is_moving` /
    /// `target_position`. The poll loop clears these once the device reports
    /// idle.
    ///
    /// On transport / echo failure the cache is rolled back to its pre-move
    /// state so a transient wire failure doesn't wedge ASCOM `IsMoving` at
    /// `true`. The rollback is gated by [`rollback_move_if_ours`] so a concurrent
    /// later call that committed a different target isn't clobbered.
    pub async fn move_absolute(&self, session: &Session<ScopsCodec>, position: i64) -> Result<()> {
        // Set cache state before sending so a racing `is_moving` read can't
        // observe `is_moving == false` while the move is in flight.
        {
            let mut state = self.cached_state.write().await;
            state.target_position = Some(position);
            state.is_moving = true;
        }
        let cmd = Command::MoveAbsolute { position };
        match session.request(cmd.clone()).await {
            Ok(ScopsResponse::Echo(echo)) => {
                if let Err(e) = validate_echo(&cmd, &echo) {
                    let mut state = self.cached_state.write().await;
                    rollback_move_if_ours(&mut state, position);
                    return Err(e);
                }
            }
            Ok(other) => {
                let mut state = self.cached_state.write().await;
                rollback_move_if_ours(&mut state, position);
                return Err(ScopsOagError::InvalidResponse(format!(
                    "MoveAbsolute returned non-echo frame: {other:?}"
                )));
            }
            Err(e) => {
                let mut state = self.cached_state.write().await;
                rollback_move_if_ours(&mut state, position);
                return Err(ScopsOagError::from(e));
            }
        }
        debug!(position, "move command sent");
        Ok(())
    }

    /// Halt the in-flight move and clear `is_moving` / `target_position`.
    pub async fn abort(&self, session: &Session<ScopsCodec>) -> Result<()> {
        session
            .request(Command::Halt)
            .await
            .map_err(ScopsOagError::from)?;
        let mut state = self.cached_state.write().await;
        state.is_moving = false;
        state.target_position = None;
        debug!("halt command sent");
        Ok(())
    }

    /// Force-refresh the cached position + moving state by issuing an `A` status
    /// report — used by `is_moving` to avoid waiting up to one polling interval
    /// for move-completion detection.
    pub async fn refresh_status(&self, session: &Session<ScopsCodec>) -> Result<()> {
        let resp = session
            .request(Command::Status)
            .await
            .map_err(ScopsOagError::from)?;
        let status = match resp {
            ScopsResponse::Status(s) => s,
            other => {
                return Err(ScopsOagError::InvalidResponse(format!(
                    "Status returned non-status frame: {other:?}"
                )));
            }
        };
        let mut state = self.cached_state.write().await;
        apply_status(&mut state, &status);
        Ok(())
    }
}

fn build_hooks(
    cached_state: &Arc<RwLock<CachedState>>,
    poll_interval: Duration,
) -> Hooks<ScopsCodec> {
    let cs_handshake = Arc::clone(cached_state);
    let cs_poll = Arc::clone(cached_state);
    Hooks {
        handshake: Box::new(move |conn| {
            let cs = Arc::clone(&cs_handshake);
            Box::pin(handshake(conn, cs))
        }),
        on_last_disconnect: Box::new(|_| Box::pin(async {})),
        shutdown: Box::new(|_| Box::pin(async {})),
        while_open: Some(Box::new(move |ctx| {
            let cs = Arc::clone(&cs_poll);
            Box::pin(poll_loop(ctx, cs, poll_interval))
        })),
    }
}

async fn handshake(
    conn: &Connection<ScopsCodec>,
    cached_state: Arc<RwLock<CachedState>>,
) -> std::result::Result<(), ScopsCodecError> {
    match conn.request(Command::Handshake).await? {
        ScopsResponse::Handshake => {}
        other => {
            return Err(ScopsCodecError::InvalidResponse(format!(
                "handshake returned non-handshake frame: {other:?}"
            )));
        }
    }

    let status = match conn.request(Command::Status).await? {
        ScopsResponse::Status(s) => s,
        other => {
            return Err(ScopsCodecError::InvalidResponse(format!(
                "Status returned non-status frame: {other:?}"
            )));
        }
    };
    debug!(
        firmware = %status.firmware_version,
        position = status.position,
        "Scops OAG handshake"
    );

    let mut state = cached_state.write().await;
    state.firmware_version = Some(status.firmware_version);
    state.position = Some(status.position);
    state.is_moving = status.is_moving;
    // A previous session may have disconnected mid-move; the device's `A`
    // report is authoritative for `is_moving`, so re-seed from it and drop any
    // stale target.
    state.target_position = None;
    Ok(())
}

async fn poll_loop(
    ctx: WhileOpen<ScopsCodec>,
    cached_state: Arc<RwLock<CachedState>>,
    poll_interval: Duration,
) {
    let mut ticker = interval(poll_interval);
    // Skip the immediate first tick — the handshake just populated the cache.
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = ctx.cancelled() => {
                debug!("Scops OAG poll loop received cancellation");
                return;
            }
        }

        match ctx.request(Command::Status).await {
            Ok(ScopsResponse::Status(s)) => {
                let mut state = cached_state.write().await;
                apply_status(&mut state, &s);
            }
            Ok(other) => warn!("poll: Status returned unexpected variant: {other:?}"),
            Err(e) => session_err_to_warn("Status", &e),
        }
    }
}

fn session_err_to_warn(op: &str, err: &SessionError<ScopsCodecError>) {
    warn!(op, error = %err, "Scops OAG poll request failed");
}

/// Roll back the cache from `move_absolute`'s pre-send commit ONLY if the cache
/// still claims this caller's target (protects concurrent `move_absolute` calls
/// from clobbering each other).
fn rollback_move_if_ours(state: &mut CachedState, our_target: i64) {
    if state.target_position == Some(our_target) {
        state.is_moving = false;
        state.target_position = None;
    }
}

/// Apply an `A` status report to the cache: position + moving state come from
/// the device, and a not-moving report clears the target. The Scops reports
/// `is_moving` directly (unlike qhy-focuser, which infers it from
/// position == target), so completion is read straight from the device.
const fn apply_status(state: &mut CachedState, status: &ScopsStatus) {
    state.position = Some(status.position);
    state.is_moving = status.is_moving;
    if !status.is_moving {
        state.target_position = None;
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    //! Behaviour-level tests for the manager, driven through the mock transport
    //! factory. Race / refcount / rollback / while-open invariants are tested
    //! once for everyone in `rusty-photon-shared-transport`.

    use super::*;
    use crate::mock::MockScopsTransportFactory;

    fn make_manager() -> Arc<FocuserManager> {
        let factory = Arc::new(MockScopsTransportFactory::default());
        FocuserManager::new(&Config::default(), factory)
    }

    #[tokio::test]
    async fn acquire_runs_handshake_and_seeds_cache() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();
        assert!(manager.is_available());

        let state = manager.get_cached_state().await;
        assert_eq!(state.firmware_version.as_deref(), Some("1.2"));
        assert_eq!(state.position, Some(0));
        assert!(!state.is_moving);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn handshake_resets_stale_move_state_from_prior_session() {
        let manager = make_manager();
        {
            let mut state = manager.cached_state.write().await;
            state.is_moving = true;
            state.target_position = Some(12345);
        }

        let session = manager.transport().acquire().await.unwrap();

        let state = manager.get_cached_state().await;
        assert!(!state.is_moving, "handshake must clear stale is_moving");
        assert_eq!(state.target_position, None);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn move_absolute_sets_target_and_is_moving() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();

        manager.move_absolute(&session, 20000).await.unwrap();
        let state = manager.get_cached_state().await;
        assert_eq!(state.target_position, Some(20000));
        assert!(state.is_moving);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_status_detects_move_completion_when_target_reached() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();

        // Mock snaps to target when within 1000 on each `A`; a 500-step move
        // completes on the first refresh.
        manager.move_absolute(&session, 500).await.unwrap();
        manager.refresh_status(&session).await.unwrap();

        let state = manager.get_cached_state().await;
        assert_eq!(state.position, Some(500));
        assert!(!state.is_moving);
        assert_eq!(state.target_position, None);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_status_advances_but_stays_moving_when_target_unreached() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();

        manager.move_absolute(&session, 20000).await.unwrap();
        manager.refresh_status(&session).await.unwrap();

        let state = manager.get_cached_state().await;
        assert_eq!(state.position, Some(1000));
        assert!(state.is_moving);
        assert_eq!(state.target_position, Some(20000));

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
    fn rollback_move_if_ours_clears_when_target_matches() {
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
    fn rollback_move_if_ours_skips_when_target_differs() {
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
    fn session_err_to_warn_logs_without_panicking() {
        session_err_to_warn(
            "Status",
            &SessionError::Transport(rusty_photon_shared_transport::TransportError::Eof),
        );
    }

    // ---- transport-failure rollback via an injectable factory --------------

    use async_trait::async_trait;
    use rusty_photon_shared_transport::{FrameTransport, TransportError};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default, Clone)]
    struct InjectableFactory {
        inner: MockScopsTransportFactory,
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

    fn make_manager_with_factory(factory: Arc<InjectableFactory>) -> Arc<FocuserManager> {
        let mut config = Config::default();
        config.serial.polling_interval = Duration::from_mins(5);
        FocuserManager::new(&config, factory)
    }

    #[tokio::test]
    async fn move_absolute_rolls_cache_back_on_transport_failure() {
        let factory = Arc::new(InjectableFactory::default());
        let fail_switch = factory.fail_next_send();
        let manager = make_manager_with_factory(Arc::clone(&factory));
        let session = manager.transport().acquire().await.unwrap();

        fail_switch.store(true, Ordering::SeqCst);
        let err = manager
            .move_absolute(&session, 5000)
            .await
            .expect_err("move_absolute should propagate the transport failure");
        assert!(
            matches!(err, ScopsOagError::Communication(_)),
            "expected Communication (Eof maps to it), got {err:?}"
        );

        let state = manager.get_cached_state().await;
        assert!(!state.is_moving, "cache should be rolled back");
        assert_eq!(state.target_position, None);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn acquire_returns_err_and_keeps_cache_empty_when_handshake_fails() {
        let factory = Arc::new(InjectableFactory::default());
        let fail_switch = factory.fail_next_send();
        let manager = make_manager_with_factory(Arc::clone(&factory));

        fail_switch.store(true, Ordering::SeqCst);
        let err = manager
            .transport()
            .acquire()
            .await
            .expect_err("handshake failure should propagate out of acquire");
        let mapped = ScopsOagError::from(err);
        assert!(
            matches!(mapped, ScopsOagError::Communication(_)),
            "expected Communication, got {mapped:?}"
        );

        assert!(!manager.is_available(), "RollbackGuard should have fired");
        let state = manager.get_cached_state().await;
        assert_eq!(state.firmware_version, None);
        assert_eq!(state.position, None);
    }
}
