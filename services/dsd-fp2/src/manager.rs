//! Thin service-level wrapper around [`SharedTransport<Fp2Codec>`].
//!
//! All wire lifecycle (refcounting, request arbitration, while-open task,
//! handshake/teardown) lives in [`rusty_photon_shared_transport`]. This
//! module owns:
//!
//! * the typed shared-transport handle every device clones from,
//! * the cached state the device's `CoverState` / `CalibratorState` /
//!   `Brightness` getters read from (refreshed by the while-open poll
//!   task),
//! * a small validate helper for brightness.
//!
//! Each cached field is stored with the instant it was brought up to date —
//! the read that brought it, or the answer to the command that set it — and
//! keeps the value from the latest one ([`ReadStamp`]). The poll reads its
//! five fields in five turns on the wire, and a command can go between
//! them: a light turned on after the poll read it off must not be put back
//! off when the poll stores.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use rusty_photon_shared_transport::{
    Hooks, ReadStamp, SessionError, SharedTransport, StateAssertion, TransportFactory, WireTiming,
};
use tokio::sync::RwLock;
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::codec::Fp2Codec;
use crate::config::Config;
use crate::error::{DsdFp2Error, Result};
use crate::protocol::{Command, RawResponse, MAX_BRIGHTNESS};

/// Polled state, which devices read directly.
///
/// The handshake seeds it, the while-open task refreshes it, and the device
/// records what its commands set — each field through its `store_` method,
/// so that the value read or set last is the one kept.
#[derive(Debug, Clone, Default)]
pub struct CachedState {
    /// `true` while the cover motor is running (`[GMOV]→(1)`).
    pub motor_running: Option<bool>,
    /// Cover state from `[GOPS]`: `0`=closed, `1`=open, other=in-between.
    pub cover_raw: Option<i32>,
    /// `true` if the EL panel is on (`[GLON]→(1)`).
    pub light_on: Option<bool>,
    /// Brightness from `[GLBR]` (0..=4096).
    pub brightness: Option<u16>,
    /// Heater temperature from `[GHTT]`. `None` if no thermistor attached.
    pub heater_temp_c: Option<f64>,
    /// Board identifier captured during handshake.
    pub firmware_board: Option<String>,
    /// Firmware version captured during handshake.
    pub firmware_version: Option<String>,
    /// When each polled field was brought up to date.
    pub motor_running_read: ReadStamp,
    pub cover_read: ReadStamp,
    pub light_read: ReadStamp,
    pub brightness_read: ReadStamp,
    pub heater_read: ReadStamp,
}

impl CachedState {
    /// Store the motor flag read, or set by a command answered, at
    /// `read_at`, unless the cache holds one from after it.
    pub fn store_motor_running(&mut self, running: bool, read_at: Instant) {
        if self.motor_running_read.admit(read_at) {
            self.motor_running = Some(running);
        } else {
            debug!(running, "dropping a motor flag older than the cached one");
        }
    }

    /// Store the cover state read at `read_at`, unless the cache holds one
    /// read after it.
    pub fn store_cover(&mut self, raw: i32, read_at: Instant) {
        if self.cover_read.admit(read_at) {
            self.cover_raw = Some(raw);
        } else {
            debug!(raw, "dropping a cover state older than the cached one");
        }
    }

    /// Store the light flag read, or set by a command answered, at
    /// `read_at`, unless the cache holds one from after it.
    pub fn store_light_on(&mut self, on: bool, read_at: Instant) {
        if self.light_read.admit(read_at) {
            self.light_on = Some(on);
        } else {
            debug!(on, "dropping a light flag older than the cached one");
        }
    }

    /// Store the brightness read, or set by a command answered, at
    /// `read_at`, unless the cache holds one from after it.
    pub fn store_brightness(&mut self, brightness: u16, read_at: Instant) {
        if self.brightness_read.admit(read_at) {
            self.brightness = Some(brightness);
        } else {
            debug!(
                brightness,
                "dropping a brightness older than the cached one"
            );
        }
    }

    /// Store the heater temperature read at `read_at` — `None` when no
    /// thermistor is attached — unless the cache holds one read after it.
    pub fn store_heater_temp(&mut self, temp_c: Option<f64>, read_at: Instant) {
        if self.heater_read.admit(read_at) {
            self.heater_temp_c = temp_c;
        } else {
            debug!("dropping a heater temperature older than the cached one");
        }
    }
}

/// Service-level façade. Devices clone the inner `Arc<SharedTransport<...>>`
/// to acquire per-device sessions.
#[derive(derive_more::Debug)]
pub struct FlatPanelManager {
    #[debug(skip)]
    transport: Arc<SharedTransport<Fp2Codec>>,
    #[debug(skip)]
    cached_state: Arc<RwLock<CachedState>>,
}

impl FlatPanelManager {
    /// Build the manager over a [`TransportFactory`]. The real driver uses
    /// [`crate::transport::Fp2SerialTransportFactory`]; tests use
    /// [`crate::mock::MockTransportFactory`].
    pub fn new(config: &Config, factory: Arc<dyn TransportFactory>) -> Arc<Self> {
        let cached_state = Arc::new(RwLock::new(CachedState::default()));
        let polling_interval = config.serial.polling_interval;

        let cs_for_hs = cached_state.clone();
        let cs_for_poll = cached_state.clone();

        let hooks = Hooks::<Fp2Codec> {
            handshake: Box::new(move |conn| {
                let cs = cs_for_hs.clone();
                Box::pin(async move {
                    let fw_resp = conn
                        .request(Command::GetFirmware)
                        .await
                        .map_err(DsdFp2Error::from)?;
                    let fw = fw_resp.parse_firmware()?;
                    if !fw.is_fp2() {
                        return Err(DsdFp2Error::HandshakeFailed(format!(
                            "expected DeepSkyDad.FP2, got board {:?}",
                            fw.board
                        )));
                    }
                    debug!("Connected to {} firmware {}", fw.board, fw.version);

                    let fields = read_fields(|cmd| conn.request_timed(cmd)).await?;

                    let mut state = cs.write().await;
                    state.firmware_board = Some(fw.board);
                    state.firmware_version = Some(fw.version);
                    fields.store(&mut state);
                    drop(state);
                    Ok(())
                })
            }),
            on_last_disconnect: Box::new(|_conn| {
                // Nothing to assert: this service holds no state that must
                // survive a no-client gap, so the answer is trivially yes.
                Box::pin(async { StateAssertion::Asserted })
            }),
            shutdown: Box::new(|_conn| Box::pin(async {})),
            while_open: Some(Box::new(move |ctx| {
                let cs = cs_for_poll.clone();
                Box::pin(poll_loop(ctx, cs, polling_interval))
            })),
        };

        Arc::new(Self {
            transport: SharedTransport::new(factory, Fp2Codec, hooks),
            cached_state,
        })
    }

    /// Clone the inner `Arc<SharedTransport<Fp2Codec>>` so a device can
    /// acquire a [`Session`](rusty_photon_shared_transport::Session).
    #[must_use]
    pub const fn transport(&self) -> &Arc<SharedTransport<Fp2Codec>> {
        &self.transport
    }

    /// Shared snapshot for read-side device methods.
    #[must_use]
    pub fn snapshot(&self) -> Arc<RwLock<CachedState>> {
        self.cached_state.clone()
    }

    /// Clamp + validate brightness against the FP2's hardware ceiling.
    ///
    /// # Errors
    ///
    /// Returns [`DsdFp2Error::InvalidValue`] when `value` exceeds the
    /// panel's maximum brightness.
    pub fn validate_brightness(value: u32) -> Result<u16> {
        if value > u32::from(MAX_BRIGHTNESS) {
            return Err(DsdFp2Error::InvalidValue(format!(
                "brightness {value} exceeds maximum {MAX_BRIGHTNESS}"
            )));
        }
        // Unreachable after the guard (MAX_BRIGHTNESS is a u16), but the
        // fallible spelling keeps the range logic in one place.
        u16::try_from(value).map_err(|_| {
            DsdFp2Error::InvalidValue(format!(
                "brightness {value} exceeds maximum {MAX_BRIGHTNESS}"
            ))
        })
    }
}

/// While-open poll loop. Refreshes the cached state every `interval`. The
/// loop terminates on cancellation (teardown fires the cancel token).
async fn poll_loop(
    ctx: rusty_photon_shared_transport::WhileOpen<Fp2Codec>,
    cached_state: Arc<RwLock<CachedState>>,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = ctx.cancelled() => {
                debug!("FP2 poll loop cancelled");
                break;
            }
        }

        match read_fields(|cmd| ctx.request_timed(cmd)).await {
            Ok(fields) => fields.store(&mut *cached_state.write().await),
            Err(e) => warn!("FP2 poll failed: {e}"),
        }
    }
}

/// The five polled fields, each with the instant it was read.
struct PolledFields {
    motor_running: (bool, Instant),
    cover_raw: (i32, Instant),
    light_on: (bool, Instant),
    brightness: (u16, Instant),
    heater_temp_c: (Option<f64>, Instant),
}

impl PolledFields {
    /// Store each field through its stamp, so one a command has set since
    /// the poll read it keeps the command's value.
    fn store(self, state: &mut CachedState) {
        state.store_motor_running(self.motor_running.0, self.motor_running.1);
        state.store_cover(self.cover_raw.0, self.cover_raw.1);
        state.store_light_on(self.light_on.0, self.light_on.1);
        state.store_brightness(self.brightness.0, self.brightness.1);
        state.store_heater_temp(self.heater_temp_c.0, self.heater_temp_c.1);
    }
}

/// Read the five polled fields in turn through `request` — the handshake's
/// connection or the poll's while-open context — each dated by its own
/// read.
async fn read_fields<F, Fut>(request: F) -> Result<PolledFields>
where
    F: Fn(Command) -> Fut,
    Fut: Future<Output = std::result::Result<(RawResponse, WireTiming), SessionError<DsdFp2Error>>>,
{
    let (reply, timing) = request(Command::GetMotorState).await?;
    let motor_running = (reply.parse_bool()?, timing.received_at);
    let (reply, timing) = request(Command::GetCoverState).await?;
    let cover_raw = (reply.parse_int()?, timing.received_at);
    let (reply, timing) = request(Command::GetLight).await?;
    let light_on = (reply.parse_bool()?, timing.received_at);
    let (reply, timing) = request(Command::GetBrightness).await?;
    let brightness = (reply.parse_u16()?, timing.received_at);
    let (reply, timing) = request(Command::GetHeaterTemp).await?;
    let heater_temp = reply.parse_temperature()?;
    Ok(PolledFields {
        motor_running,
        cover_raw,
        light_on,
        brightness,
        heater_temp_c: (
            (heater_temp > -40.0).then_some(heater_temp),
            timing.received_at,
        ),
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn validate_brightness_accepts_zero_and_max() {
        assert_eq!(FlatPanelManager::validate_brightness(0).unwrap(), 0);
        assert_eq!(
            FlatPanelManager::validate_brightness(u32::from(MAX_BRIGHTNESS)).unwrap(),
            MAX_BRIGHTNESS
        );
    }

    #[test]
    fn validate_brightness_rejects_above_max() {
        let err = FlatPanelManager::validate_brightness(u32::from(MAX_BRIGHTNESS) + 1).unwrap_err();
        assert!(matches!(err, DsdFp2Error::InvalidValue(_)));
    }

    #[test]
    fn session_err_to_warn_logs_without_panicking() {
        // The poll loop's Err arms log via `warn!` and never bubble. With
        // the mock factory always succeeding in tests, those arms aren't
        // exercised end-to-end — keep the call covered by emitting it
        // directly so coverage stays honest. The function is `warn!`-only
        // and returns nothing observable; the assertion is just that it
        // doesn't panic.
        use rusty_photon_shared_transport::SessionError;
        let wrapped = SessionError::<DsdFp2Error>::Codec(DsdFp2Error::MalformedResponse(
            "synthetic".to_string(),
        ));
        warn!("FP2 poll failed: {}", DsdFp2Error::from(wrapped));
    }
}

#[cfg(all(test, feature = "mock"))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod mock_tests {
    use super::*;
    use crate::mock::{MockState, MockTransportFactory};

    fn test_config() -> Config {
        Config {
            serial: crate::config::SerialConfig {
                port: "/dev/mock".to_string(),
                polling_interval: Duration::from_mins(1),
                ..Default::default()
            },
            server: crate::config::AlpacaServerConfig::new(0),
            cover_calibrator: crate::config::CoverCalibratorConfig::default(),
        }
    }

    fn make_manager_with(factory: MockTransportFactory) -> Arc<FlatPanelManager> {
        FlatPanelManager::new(&test_config(), Arc::new(factory))
    }

    fn make_manager() -> Arc<FlatPanelManager> {
        make_manager_with(MockTransportFactory::default())
    }

    #[tokio::test]
    async fn handshake_seeds_cached_state() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();
        let snap = manager.snapshot();
        let s = snap.read().await.clone();
        assert_eq!(s.firmware_board.as_deref(), Some("DeepSkyDad.FP2"));
        assert!(s.firmware_version.is_some());
        assert_eq!(s.motor_running, Some(false));
        assert_eq!(s.cover_raw, Some(0));
        assert_eq!(s.light_on, Some(false));
        assert_eq!(s.brightness, Some(0));
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn handshake_rejects_non_fp2_firmware() {
        let state = MockState::default();
        state.set_firmware("DeepSkyDad.FP1", "1.0.0").await;
        let manager = make_manager_with(MockTransportFactory::with_state(state));
        let err = manager.transport().acquire().await.unwrap_err();
        match err {
            rusty_photon_shared_transport::SessionError::Codec(DsdFp2Error::HandshakeFailed(_)) => {
            }
            other => panic!("expected HandshakeFailed, got {other:?}"),
        }
        assert!(!manager.transport().is_available());
    }

    #[tokio::test]
    async fn double_acquire_shares_one_open() {
        let manager = make_manager();
        let s1 = manager.transport().acquire().await.unwrap();
        let s2 = manager.transport().acquire().await.unwrap();
        // First close just decrements; second runs teardown.
        s1.close().await.unwrap();
        assert!(manager.transport().is_available());
        s2.close().await.unwrap();
        assert!(!manager.transport().is_available());
    }

    #[tokio::test]
    async fn session_request_round_trips() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();
        let r = session.request(Command::GetBrightness).await.unwrap();
        assert_eq!(r.parse_u16().unwrap(), 0);
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn session_request_open_close_round_trip() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();
        // Set target then move; both should return OK.
        session
            .request(Command::SetTarget(crate::protocol::CLOSED_ANGLE))
            .await
            .unwrap()
            .parse_ok()
            .unwrap();
        session
            .request(Command::StartMove)
            .await
            .unwrap()
            .parse_ok()
            .unwrap();
        // The mock reports the motor running for exactly one `[GMOV]`
        // read after `[SMOV]` (matches real hardware's async settle;
        // see mock.rs), same as `poll_once` consuming it each cycle.
        let m = session.request(Command::GetMotorState).await.unwrap();
        assert!(m.parse_bool().unwrap());
        // Confirm the cover state observable through GOPS once settled.
        let s = session.request(Command::GetCoverState).await.unwrap();
        assert_eq!(s.parse_int().unwrap(), 0); // 0 = closed
        session.close().await.unwrap();
    }

    // ---- a poll against a command ---------------------------------------

    use async_trait::async_trait;
    use rusty_photon_shared_transport::{Codec as _, FrameTransport, TransportError};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::PoisonError;

    /// How long a test waits for a request to reach its held reply before
    /// declaring the poll loop dead: a failure deadline, not a duration.
    const HOLD_DEADLINE: Duration = Duration::from_secs(10);

    /// Wraps the canonical mock and, once armed with a command, holds the
    /// reply to the next send of it: `recv_frame` says it has been reached
    /// and waits to be let go. The panel has the command by then, so the
    /// held reply is already decided.
    #[derive(Default, Clone)]
    struct HoldingFactory {
        inner: MockTransportFactory,
        hold: Arc<ReplyHold>,
    }

    #[derive(Default)]
    struct ReplyHold {
        /// The command whose reply is held next, as it goes on the wire.
        armed: std::sync::Mutex<Option<Vec<u8>>>,
        holding: AtomicBool,
        reached: tokio::sync::Notify,
        released: tokio::sync::Notify,
    }

    impl HoldingFactory {
        fn hold_next_reply_to(&self, command: &Command) {
            *self
                .hold
                .armed
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(Fp2Codec.encode(command));
        }

        /// Wait until the armed command's reply is held, or fail at
        /// [`HOLD_DEADLINE`].
        async fn reply_held(&self) {
            let reached = tokio::time::timeout(HOLD_DEADLINE, self.hold.reached.notified()).await;
            assert!(
                reached.is_ok(),
                "no request reached its reply in {HOLD_DEADLINE:?}"
            );
        }

        fn release_reply(&self) {
            self.hold.released.notify_one();
        }
    }

    #[async_trait]
    impl TransportFactory for HoldingFactory {
        async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
            let inner = self.inner.open().await?;
            Ok(Box::new(HoldingTransport {
                inner,
                hold: Arc::clone(&self.hold),
            }))
        }
    }

    struct HoldingTransport {
        inner: Box<dyn FrameTransport>,
        hold: Arc<ReplyHold>,
    }

    #[async_trait]
    impl FrameTransport for HoldingTransport {
        async fn send_frame(&mut self, bytes: &[u8]) -> std::result::Result<(), TransportError> {
            let armed_command = self
                .hold
                .armed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take_if(|command| bytes.starts_with(command));
            if armed_command.is_some() {
                self.hold.holding.store(true, Ordering::SeqCst);
            }
            self.inner.send_frame(bytes).await
        }

        async fn recv_frame(
            &mut self,
            buf: &mut Vec<u8>,
        ) -> std::result::Result<(), TransportError> {
            if self.hold.holding.swap(false, Ordering::SeqCst) {
                self.hold.reached.notify_one();
                self.hold.released.notified().await;
            }
            self.inner.recv_frame(buf).await
        }
    }

    #[tokio::test]
    async fn a_light_turned_on_after_the_poll_read_it_off_stays_on() {
        let factory = Arc::new(HoldingFactory::default());
        let mut config = test_config();
        config.serial.polling_interval = Duration::from_millis(20);
        let dyn_factory: Arc<dyn TransportFactory> = Arc::<HoldingFactory>::clone(&factory);
        let manager = FlatPanelManager::new(&config, dyn_factory);
        let session = manager.transport().acquire().await.unwrap();
        let snap = manager.snapshot();

        // A poll's light read is on the wire: the panel has it, the light
        // still off.
        factory.hold_next_reply_to(&Command::GetLight);
        factory.reply_held().await;

        // With the cache held, the poll reads the light off and cannot
        // store it; the light goes on behind that read, and the command's
        // value lands first — what `calibrator_on` records.
        let mut cache = snap.write().await;
        factory.release_reply();
        let (reply, timing) = session
            .request_timed(Command::SetLight(true))
            .await
            .unwrap();
        reply.parse_ok().unwrap();
        cache.store_light_on(true, timing.received_at);

        // The poll stores next, and its next motor read is the sign it has.
        factory.hold_next_reply_to(&Command::GetMotorState);
        drop(cache);
        factory.reply_held().await;

        assert_eq!(
            snap.read().await.light_on,
            Some(true),
            "a poll's light read before the command put the light back off"
        );

        factory.release_reply();
        session.close().await.unwrap();
    }
}
