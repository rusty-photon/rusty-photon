//! Thin manager wrapping `SharedTransport<PpbaCodec>` plus the cached
//! protocol state both ASCOM devices read from.
//!
//! The refcount, slot, open/close transitions, command-lock arbitration,
//! and poll-task lifetime all live in
//! [`rusty_photon_shared_transport::SharedTransport`]. What stays here:
//!
//! * The PPBA handshake (ping → PA → PS, seed the cache).
//! * The 5s poll loop body that refreshes PA + PS into the cache.
//! * The cached state both devices share (status, power stats, USB hub
//!   tracking, sensor sliding-window means).

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rusty_photon_shared_transport::{
    Connection, Hooks, Session, SessionError, SharedTransport, TransportFactory, WhileOpen,
};
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, warn};

use crate::codec::{PpbaCodec, PpbaCodecError, PpbaResponse};
use crate::config::Config;
use crate::error::{PpbaError, Result};
use crate::mean::SensorMean;
use crate::protocol::{PpbaCommand, PpbaPowerStats, PpbaStatus};

/// Cached device state shared between the switch device and the
/// observing-conditions device.
#[derive(Debug, Clone, Default)]
pub struct CachedState {
    pub status: Option<PpbaStatus>,
    pub power_stats: Option<PpbaPowerStats>,
    /// USB hub state — not part of the PA reply, tracked separately.
    pub usb_hub_enabled: bool,
    pub last_update: Option<SystemTime>,
    pub temp_mean: SensorMean,
    pub humidity_mean: SensorMean,
    pub dewpoint_mean: SensorMean,
}

/// Manager that wraps the shared transport plus PPBA-specific cached
/// state. One instance per process; both devices hold `Arc<PpbaManager>`.
pub struct PpbaManager {
    transport: Arc<SharedTransport<PpbaCodec>>,
    cached_state: Arc<RwLock<CachedState>>,
}

impl PpbaManager {
    pub fn new(config: &Config, factory: Arc<dyn TransportFactory>) -> Arc<Self> {
        // Seed sensor windows from config.
        let mut state = CachedState::default();
        let window = config.observingconditions.averaging_period;
        state.temp_mean.set_window(window);
        state.humidity_mean.set_window(window);
        state.dewpoint_mean.set_window(window);
        let cached_state = Arc::new(RwLock::new(state));

        let poll_interval = config.serial.polling_interval;
        let hooks = build_hooks(&cached_state, poll_interval);
        let transport = SharedTransport::new(factory, PpbaCodec, hooks);

        Arc::new(Self {
            transport,
            cached_state,
        })
    }

    /// Access the shared transport so devices can acquire sessions.
    #[must_use]
    pub const fn transport(&self) -> &Arc<SharedTransport<PpbaCodec>> {
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

    /// Reconfigure the sliding-window length on all three sensor means.
    pub async fn set_averaging_period(&self, period: Duration) {
        let mut state = self.cached_state.write().await;
        state.temp_mean.set_window(period);
        state.humidity_mean.set_window(period);
        state.dewpoint_mean.set_window(period);
        drop(state);
        debug!(?period, "sensor averaging period updated");
    }

    /// Update the cached USB hub flag — the PPBA's PA reply doesn't
    /// include it, so the driver tracks it locally after every PU: write.
    pub async fn set_usb_hub_state(&self, enabled: bool) {
        let mut state = self.cached_state.write().await;
        state.usb_hub_enabled = enabled;
        drop(state);
        debug!(enabled, "usb hub state updated");
    }

    /// Issue a protocol command on the device's session and return the
    /// decoded response. Used by both devices for set-commands and the
    /// PPBA-specific routing they do around them.
    ///
    /// # Errors
    ///
    /// Returns the [`Session::request`] failure as a [`PpbaError`]: a
    /// transport error, a response the codec cannot decode, or an exhausted
    /// skip budget.
    pub async fn send_command(
        &self,
        session: &Session<PpbaCodec>,
        cmd: PpbaCommand,
    ) -> Result<PpbaResponse> {
        session.request(cmd).await.map_err(PpbaError::from)
    }

    /// Refresh the status (PA) cache via the caller's session.
    ///
    /// Devices use this on-demand (e.g. before validating an auto-dew
    /// write, or after a successful set command). The poll loop does the
    /// same thing internally while a session is alive.
    ///
    /// No analogous `refresh_power_stats` exists: the power-stats cache
    /// is seeded by the handshake and kept fresh by the poll loop, and
    /// no caller needs an on-demand PS refresh today. Add one back if a
    /// future caller materialises.
    ///
    /// # Errors
    ///
    /// Returns the [`Session::request`] failure as a [`PpbaError`] — a
    /// transport error, a response the codec cannot decode, or an exhausted
    /// skip budget — or [`PpbaError::InvalidResponse`] if the reply is not a
    /// `PA` status frame; the cache is left untouched in either case.
    pub async fn refresh_status(&self, session: &Session<PpbaCodec>) -> Result<()> {
        let resp = session
            .request(PpbaCommand::Status)
            .await
            .map_err(PpbaError::from)?;
        let PpbaResponse::Status(status) = resp else {
            return Err(PpbaError::InvalidResponse(
                "PA command returned non-status frame".to_string(),
            ));
        };
        let mut state = self.cached_state.write().await;
        apply_status(&mut state, &status);
        Ok(())
    }
}

fn build_hooks(
    cached_state: &Arc<RwLock<CachedState>>,
    poll_interval: Duration,
) -> Hooks<PpbaCodec> {
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
    conn: &Connection<PpbaCodec>,
    cached_state: Arc<RwLock<CachedState>>,
) -> std::result::Result<(), PpbaCodecError> {
    // Ping first — fails fast on a wrong-protocol peer.
    let ping = conn.request(PpbaCommand::Ping).await?;
    if !matches!(ping, PpbaResponse::PingOk) {
        return Err(PpbaCodecError::InvalidResponse(
            "expected PPBA_OK from ping".to_string(),
        ));
    }

    let status_resp = conn.request(PpbaCommand::Status).await?;
    let PpbaResponse::Status(status) = status_resp else {
        return Err(PpbaCodecError::InvalidResponse(
            "handshake PA returned non-status frame".to_string(),
        ));
    };

    let power_resp = conn.request(PpbaCommand::PowerStats).await?;
    let PpbaResponse::PowerStats(power_stats) = power_resp else {
        return Err(PpbaCodecError::InvalidResponse(
            "handshake PS returned non-power-stats frame".to_string(),
        ));
    };

    let mut state = cached_state.write().await;
    apply_status(&mut state, &status);
    state.power_stats = Some(power_stats);
    drop(state);
    Ok(())
}

async fn poll_loop(
    ctx: WhileOpen<PpbaCodec>,
    cached_state: Arc<RwLock<CachedState>>,
    poll_interval: Duration,
) {
    let mut ticker = interval(poll_interval);
    // Skip the immediate first tick — the handshake just populated the
    // cache; first poll should wait one interval.
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = ctx.cancelled() => {
                debug!("ppba poll loop received cancellation");
                return;
            }
        }

        match ctx.request(PpbaCommand::Status).await {
            Ok(PpbaResponse::Status(status)) => {
                let mut state = cached_state.write().await;
                apply_status(&mut state, &status);
            }
            Ok(other) => warn!("ppba poll: PA returned unexpected frame variant: {other:?}"),
            Err(e) => session_err_to_warn("PA", &e),
        }

        match ctx.request(PpbaCommand::PowerStats).await {
            Ok(PpbaResponse::PowerStats(power_stats)) => {
                let mut state = cached_state.write().await;
                state.power_stats = Some(power_stats);
            }
            Ok(other) => warn!("ppba poll: PS returned unexpected frame variant: {other:?}"),
            Err(e) => session_err_to_warn("PS", &e),
        }
    }
}

fn session_err_to_warn(op: &str, err: &SessionError<PpbaCodecError>) {
    warn!(op, error = %err, "ppba poll request failed");
}

fn apply_status(state: &mut CachedState, status: &PpbaStatus) {
    state.status = Some(status.clone());
    state.last_update = Some(SystemTime::now());
    state.temp_mean.add_sample(status.temperature);
    state.humidity_mean.add_sample(status.humidity);
    state.dewpoint_mean.add_sample(status.dewpoint);
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    //! Behaviour-level tests for the manager, driven through the mock
    //! transport factory. Race / refcount / rollback invariants are
    //! tested once for everyone in `rusty-photon-shared-transport` —
    //! they don't get re-tested here per the migration plan.

    use super::*;
    use crate::mock::MockPpbaTransportFactory;

    fn make_manager() -> Arc<PpbaManager> {
        let factory = Arc::new(MockPpbaTransportFactory::default());
        PpbaManager::new(&Config::default(), factory)
    }

    #[tokio::test]
    async fn acquire_runs_handshake_and_seeds_cache() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();
        assert!(manager.is_available());

        let state = manager.get_cached_state().await;
        let status = state.status.expect("status seeded by handshake");
        assert!((status.temperature - 25.0).abs() < f64::EPSILON);
        let stats = state.power_stats.expect("power stats seeded by handshake");
        assert!((stats.average_amps - 2.5).abs() < f64::EPSILON);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_status_updates_cache() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();

        // Mutate device state via a set command, then refresh and observe.
        manager
            .send_command(&session, PpbaCommand::SetQuad12V(false))
            .await
            .unwrap();
        manager.refresh_status(&session).await.unwrap();

        let state = manager.get_cached_state().await;
        assert!(!state.status.unwrap().switches.quad_12v);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn set_averaging_period_resizes_means() {
        let manager = make_manager();
        let new_window = Duration::from_mins(2);
        manager.set_averaging_period(new_window).await;
        let state = manager.get_cached_state().await;
        assert_eq!(state.temp_mean.window(), new_window);
        assert_eq!(state.humidity_mean.window(), new_window);
        assert_eq!(state.dewpoint_mean.window(), new_window);
    }

    #[tokio::test]
    async fn set_usb_hub_state_tracks_locally() {
        let manager = make_manager();
        manager.set_usb_hub_state(true).await;
        assert!(manager.get_cached_state().await.usb_hub_enabled);
        manager.set_usb_hub_state(false).await;
        assert!(!manager.get_cached_state().await.usb_hub_enabled);
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
            "PA",
            &SessionError::Transport(rusty_photon_shared_transport::TransportError::Eof),
        );
    }

    // ========================================================================
    // Transport-failure error branches: send_command, refresh_status, and
    // the acquire/handshake rollback path. Mirrors the InjectableFactory
    // pattern used in qhy-focuser; the canonical race / refcount / rollback
    // invariants remain tested once in rusty-photon-shared-transport.
    // ========================================================================

    use async_trait::async_trait;
    use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Wraps the canonical mock factory but gates `send_frame` behind a
    /// shared atomic — flipping it makes the very next send return EOF.
    /// Used to inject a wire-level failure after handshake has succeeded
    /// (or, by arming the flag *before* acquire, during handshake).
    #[derive(Default, Clone)]
    struct InjectableFactory {
        inner: MockPpbaTransportFactory,
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

    /// Manager built with the `InjectableFactory` and a long poll interval
    /// (300s) so the poll loop can't consume the armed failure before the
    /// test's foreground request does.
    fn make_manager_with_factory(factory: Arc<InjectableFactory>) -> Arc<PpbaManager> {
        let mut config = Config::default();
        config.serial.polling_interval = Duration::from_mins(5);
        PpbaManager::new(&config, factory)
    }

    #[tokio::test]
    async fn send_command_propagates_transport_failure() {
        let factory = Arc::new(InjectableFactory::default());
        let fail_switch = factory.fail_next_send();
        let manager = make_manager_with_factory(Arc::clone(&factory));
        let session = manager.transport().acquire().await.unwrap();

        fail_switch.store(true, Ordering::SeqCst);
        let err = manager
            .send_command(&session, PpbaCommand::SetQuad12V(false))
            .await
            .expect_err("send_command should propagate the transport failure");
        // TransportError::Eof routes through transport_to_ppba to
        // PpbaError::Communication("Connection closed").
        assert!(
            matches!(err, PpbaError::Communication(_)),
            "expected Communication, got {err:?}"
        );

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_status_propagates_transport_failure() {
        let factory = Arc::new(InjectableFactory::default());
        let fail_switch = factory.fail_next_send();
        let manager = make_manager_with_factory(Arc::clone(&factory));
        let session = manager.transport().acquire().await.unwrap();

        fail_switch.store(true, Ordering::SeqCst);
        let err = manager
            .refresh_status(&session)
            .await
            .expect_err("refresh_status should propagate the transport failure");
        assert!(
            matches!(err, PpbaError::Communication(_)),
            "expected Communication, got {err:?}"
        );

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn acquire_returns_err_and_keeps_cache_empty_when_handshake_send_fails() {
        // Failing the first handshake send (Ping) must:
        // - propagate Err from acquire(),
        // - leave is_available() == false (the RollbackGuard fired),
        // - leave the cache untouched (no PA / PS seeded from a partial
        //   handshake).
        let factory = Arc::new(InjectableFactory::default());
        let fail_switch = factory.fail_next_send();
        let manager = make_manager_with_factory(Arc::clone(&factory));

        fail_switch.store(true, Ordering::SeqCst);
        let err = manager
            .transport()
            .acquire()
            .await
            .expect_err("handshake failure should propagate out of acquire");
        let mapped = PpbaError::from(err);
        assert!(
            matches!(mapped, PpbaError::Communication(_)),
            "expected Communication, got {mapped:?}"
        );

        assert!(
            !manager.is_available(),
            "RollbackGuard should have rolled the refcount back"
        );

        let state = manager.get_cached_state().await;
        assert!(
            state.status.is_none(),
            "handshake should not have seeded status"
        );
        assert!(
            state.power_stats.is_none(),
            "handshake should not have seeded power stats"
        );
        assert!(state.last_update.is_none());
    }
}
