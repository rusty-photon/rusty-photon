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

use std::sync::Arc;
use std::time::Duration;

use rusty_photon_shared_transport::{Hooks, SharedTransport, TransportFactory};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::codec::Fp2Codec;
use crate::config::Config;
use crate::error::{DsdFp2Error, Result};
use crate::protocol::{Command, MAX_BRIGHTNESS};

/// Polled state. Devices read this directly; the while-open task is the
/// only writer (aside from the handshake closure that seeds it).
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
    pub fn new(config: Config, factory: Arc<dyn TransportFactory>) -> Arc<Self> {
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

                    let motor = conn
                        .request(Command::GetMotorState)
                        .await
                        .map_err(DsdFp2Error::from)?
                        .parse_bool()?;
                    let cover = conn
                        .request(Command::GetCoverState)
                        .await
                        .map_err(DsdFp2Error::from)?
                        .parse_int()?;
                    let light = conn
                        .request(Command::GetLight)
                        .await
                        .map_err(DsdFp2Error::from)?
                        .parse_bool()?;
                    let brightness = conn
                        .request(Command::GetBrightness)
                        .await
                        .map_err(DsdFp2Error::from)?
                        .parse_u16()?;
                    let heater_temp = conn
                        .request(Command::GetHeaterTemp)
                        .await
                        .map_err(DsdFp2Error::from)?
                        .parse_temperature()?;
                    let heater_present = heater_temp > -40.0;

                    let mut state = cs.write().await;
                    state.firmware_board = Some(fw.board);
                    state.firmware_version = Some(fw.version);
                    state.motor_running = Some(motor);
                    state.cover_raw = Some(cover);
                    state.light_on = Some(light);
                    state.brightness = Some(brightness);
                    state.heater_temp_c = if heater_present {
                        Some(heater_temp)
                    } else {
                        None
                    };
                    Ok(())
                })
            }),
            on_last_disconnect: Box::new(|_conn| Box::pin(async {})),
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

        match poll_once(&ctx).await {
            Ok(new) => {
                let mut state = cached_state.write().await;
                state.motor_running = Some(new.motor_running);
                state.cover_raw = Some(new.cover_raw);
                state.light_on = Some(new.light_on);
                state.brightness = Some(new.brightness);
                state.heater_temp_c = if new.heater_present {
                    Some(new.heater_temp)
                } else {
                    None
                };
            }
            Err(e) => warn!("FP2 poll failed: {e}"),
        }
    }
}

struct PollSnapshot {
    motor_running: bool,
    cover_raw: i32,
    light_on: bool,
    brightness: u16,
    heater_temp: f64,
    heater_present: bool,
}

async fn poll_once(
    ctx: &rusty_photon_shared_transport::WhileOpen<Fp2Codec>,
) -> Result<PollSnapshot> {
    let motor = ctx
        .request(Command::GetMotorState)
        .await
        .map_err(DsdFp2Error::from)?
        .parse_bool()?;
    let cover = ctx
        .request(Command::GetCoverState)
        .await
        .map_err(DsdFp2Error::from)?
        .parse_int()?;
    let light = ctx
        .request(Command::GetLight)
        .await
        .map_err(DsdFp2Error::from)?
        .parse_bool()?;
    let brightness = ctx
        .request(Command::GetBrightness)
        .await
        .map_err(DsdFp2Error::from)?
        .parse_u16()?;
    let heater_temp = ctx
        .request(Command::GetHeaterTemp)
        .await
        .map_err(DsdFp2Error::from)?
        .parse_temperature()?;
    Ok(PollSnapshot {
        motor_running: motor,
        cover_raw: cover,
        light_on: light,
        brightness,
        heater_temp,
        heater_present: heater_temp > -40.0,
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
        FlatPanelManager::new(test_config(), Arc::new(factory))
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
}
