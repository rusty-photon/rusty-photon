//! ASCOM Alpaca `CoverCalibrator` device for the Deep Sky Dad FP2.
//!
//! Holds an `Arc<FlatPanelManager>` (the service-wide façade over
//! `SharedTransport<Fp2Codec>`) plus a per-device session slot.
//!
//! Cover and calibrator state derive from the cached snapshot the manager's
//! while-open task refreshes. Writes (open/close/calibrator-on/-off) go
//! through `Session::request` so they share the same request arbitration
//! lock as the poll loop.

use std::sync::Arc;

use ascom_alpaca::api::cover_calibrator::{CalibratorStatus, CoverStatus};
use ascom_alpaca::api::{CoverCalibrator, Device};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use async_trait::async_trait;
use rusty_photon_driver::ConfigActionCtx;
use rusty_photon_shared_transport::Session;
use tokio::sync::RwLock;
use tracing::debug;

use crate::codec::Fp2Codec;
use crate::config::CoverCalibratorConfig;
use crate::config_actions::DsdFp2Driver;
use crate::error::DsdFp2Error;
use crate::manager::FlatPanelManager;
use crate::protocol::{Command, CLOSED_ANGLE, MAX_BRIGHTNESS, OPEN_ANGLE};

/// Deep Sky Dad FP2 as an ASCOM `CoverCalibrator`.
#[derive(derive_more::Debug)]
pub struct DsdFp2Device {
    config: CoverCalibratorConfig,
    #[debug(skip)]
    manager: Arc<FlatPanelManager>,
    #[debug(skip)]
    session: Arc<RwLock<Option<Session<Fp2Codec>>>>,
    /// `Some` when the driver was built with a config source (the normal path
    /// through `ServerBuilder`); `None` for focused unit-test devices that
    /// don't exercise config actions.
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<DsdFp2Driver>>,
}

impl DsdFp2Device {
    #[must_use]
    pub fn new(config: CoverCalibratorConfig, manager: Arc<FlatPanelManager>) -> Self {
        Self {
            config,
            manager,
            session: Arc::new(RwLock::new(None)),
            config_ctx: None,
        }
    }

    /// Attach the config-action context, enabling `config.get` / `config.apply`.
    #[must_use]
    pub fn with_config_actions(mut self, ctx: ConfigActionCtx<DsdFp2Driver>) -> Self {
        self.config_ctx = Some(ctx);
        self
    }

    /// Hardware-clamped configurable maximum. ASCOM `MaxBrightness` and
    /// `calibrator_on`'s validation share this so they can't disagree.
    fn effective_max_brightness(&self) -> u32 {
        self.config.max_brightness.min(u32::from(MAX_BRIGHTNESS))
    }
}

#[async_trait]
impl Device for DsdFp2Device {
    fn static_name(&self) -> &str {
        &self.config.name
    }

    fn unique_id(&self) -> &str {
        &self.config.unique_id
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok(self.config.description.clone())
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(self.session.read().await.is_some() && self.manager.transport().is_available())
    }

    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        // Hold the write lock across the entire check-and-modify so two
        // concurrent `Connected=true` requests for this device can't both
        // observe `session.is_none()`, both call `transport.acquire()`,
        // and end up with the session refcount diverging from the
        // single per-device slot.
        let mut slot = self.session.write().await;
        let already_open = slot.is_some() && self.manager.transport().is_available();
        if already_open == connected {
            return Ok(());
        }
        if connected {
            // `?` does SessionError<DsdFp2Error> → DsdFp2Error via the
            // From impl in error.rs, then DsdFp2Error → ASCOMError via
            // the second From impl on `?`.
            let session = self
                .manager
                .transport()
                .acquire()
                .await
                .map_err(DsdFp2Error::from)?;
            *slot = Some(session);
            debug!("FP2 device connected");
        } else if let Some(session) = slot.take() {
            // `Session::close` returns `Result<_, TransportError>`;
            // `From<TransportError> for DsdFp2Error` handles the
            // first hop, and `From<DsdFp2Error> for ASCOMError`
            // does the second on `?`.
            session.close().await.map_err(DsdFp2Error::from)?;
            debug!("FP2 device disconnected");
        }
        Ok(())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("Deep Sky Dad FP2 Driver - ASCOM Alpaca CoverCalibrator".to_string())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }

    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(rusty_photon_driver::supported_actions(&self.config_ctx))
    }

    async fn action(&self, action: String, parameters: String) -> ASCOMResult<String> {
        rusty_photon_driver::dispatch::<DsdFp2Driver>(&self.config_ctx, action, parameters).await
    }
}

#[async_trait]
impl CoverCalibrator for DsdFp2Device {
    async fn cover_state(&self) -> ASCOMResult<CoverStatus> {
        if !self.connected().await? {
            return Ok(CoverStatus::Unknown);
        }
        let snap = self.manager.snapshot();
        let state = snap.read().await.clone();
        Ok(derive_cover_state(state.motor_running, state.cover_raw))
    }

    async fn calibrator_state(&self) -> ASCOMResult<CalibratorStatus> {
        if !self.connected().await? {
            return Ok(CalibratorStatus::Unknown);
        }
        let snap = self.manager.snapshot();
        let state = snap.read().await.clone();
        Ok(derive_calibrator_state(state.light_on))
    }

    async fn brightness(&self) -> ASCOMResult<u32> {
        if !self.connected().await? {
            return Err(ASCOMError::NOT_CONNECTED);
        }
        let snap = self.manager.snapshot();
        let state = snap.read().await.clone();
        Ok(u32::from(state.brightness.unwrap_or(0)))
    }

    async fn max_brightness(&self) -> ASCOMResult<u32> {
        Ok(self.effective_max_brightness())
    }

    async fn open_cover(&self) -> ASCOMResult<()> {
        execute_move(self, OPEN_ANGLE).await
    }

    async fn close_cover(&self) -> ASCOMResult<()> {
        execute_move(self, CLOSED_ANGLE).await
    }

    /// The FP2 firmware has no halt-motion opcode; once `[SMOV]` starts a
    /// move it runs to completion. The ASCOM `ICoverCalibratorV2` spec
    /// requires `HaltCover` to throw `MethodNotImplementedException`
    /// "if cover movement cannot be interrupted" — see
    /// <https://ascom-standards.org/newdocs/covercalibrator.html>. We
    /// honour that here.
    ///
    /// **Known `ConformU` divergence.** `ConformU` 4.3 flags this as an
    /// "issue" anyway because `CoverCalibratorTester.TestHaltCover` does
    /// not distinguish `MethodNotImplementedException` from other
    /// exceptions in its async-cover branch (it treats every exception
    /// as `Required.MustBeImplemented`). See
    /// `docs/services/dsd-fp2.md` "Known limitations" for the upstream
    /// bug report; the driver is intentionally spec-compliant.
    async fn halt_cover(&self) -> ASCOMResult<()> {
        Err(ASCOMError::new(
            ASCOMErrorCode::NOT_IMPLEMENTED,
            "HaltCover not implemented: FP2 firmware cannot interrupt an in-progress cover \
             movement. Per ICoverCalibratorV2, HaltCover MUST throw MethodNotImplementedException \
             when cover movement cannot be interrupted.",
        ))
    }

    async fn calibrator_on(&self, brightness: u32) -> ASCOMResult<()> {
        // Validate against the effective max (the lower of the config cap
        // and the hardware ceiling) so the value MaxBrightness advertises
        // and the value calibrator_on accepts agree.
        let effective_max = self.effective_max_brightness();
        if brightness > effective_max {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("brightness {brightness} exceeds effective max brightness {effective_max}"),
            ));
        }
        // Below `min_brightness` the panel's EL output is non-linear/unreliable
        // (see docs/services/dsd-fp2.md "Hardware Constraints"); reject it so a
        // caller picks either 0 (the ASCOM on-at-zero state) or a value in the
        // panel's usable range. Zero is exempt: it's on-at-zero, not a dim
        // request, so calibrator_state still reports Ready — call
        // calibrator_off() to actually turn the light off.
        let min_brightness = self.config.min_brightness;
        if brightness != 0 && brightness < min_brightness {
            // config.apply rejects min_brightness > max_brightness (see
            // config_actions.rs), but a hand-edited config file loaded at
            // startup isn't validated — so effective_max can still be below
            // min_brightness here. Don't suggest an unreachable remediation
            // ("a value >= min_brightness") when no such value exists.
            let remediation = if min_brightness > effective_max {
                format!(
                    "cover_calibrator.min_brightness ({min_brightness}) exceeds the effective \
                     max brightness ({effective_max}) — this is a driver misconfiguration; fix \
                     it via config.apply"
                )
            } else {
                format!(
                    "use 0 for the ASCOM on-at-zero state, calibrator_off() to turn the light \
                     off, or a value >= {min_brightness}"
                )
            };
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!(
                    "brightness {brightness} is below the configured minimum {min_brightness} \
                     (panel output is non-linear below this value); {remediation}"
                ),
            ));
        }
        let value = FlatPanelManager::validate_brightness(brightness)?;
        let slot = self.session.read().await;
        let session = slot.as_ref().ok_or(ASCOMError::NOT_CONNECTED)?;

        session
            .request(Command::SetBrightness(value))
            .await
            .map_err(DsdFp2Error::from)?
            .parse_ok()?;
        session
            .request(Command::SetLight(true))
            .await
            .map_err(DsdFp2Error::from)?
            .parse_ok()?;

        let snap = self.manager.snapshot();
        let mut state = snap.write().await;
        state.brightness = Some(value);
        state.light_on = Some(true);
        Ok(())
    }

    async fn calibrator_off(&self) -> ASCOMResult<()> {
        let slot = self.session.read().await;
        let session = slot.as_ref().ok_or(ASCOMError::NOT_CONNECTED)?;
        session
            .request(Command::SetLight(false))
            .await
            .map_err(DsdFp2Error::from)?
            .parse_ok()?;
        let snap = self.manager.snapshot();
        let mut state = snap.write().await;
        state.light_on = Some(false);
        Ok(())
    }
}

/// Drive the cover to a target angle (`open_cover` / `close_cover`).
async fn execute_move(device: &DsdFp2Device, angle: u16) -> ASCOMResult<()> {
    let slot = device.session.read().await;
    let session = slot.as_ref().ok_or(ASCOMError::NOT_CONNECTED)?;

    session
        .request(Command::SetTarget(angle))
        .await
        .map_err(DsdFp2Error::from)?
        .parse_ok()?;
    session
        .request(Command::StartMove)
        .await
        .map_err(DsdFp2Error::from)?
        .parse_ok()?;

    // Mark motor as running locally so `cover_state` reports `Moving`
    // immediately, before the next poll observes it.
    let snap = device.manager.snapshot();
    snap.write().await.motor_running = Some(true);
    Ok(())
}

/// Derive `CoverStatus` from cached state.
const fn derive_cover_state(motor_running: Option<bool>, cover_raw: Option<i32>) -> CoverStatus {
    match (motor_running, cover_raw) {
        (Some(true), _) => CoverStatus::Moving,
        (Some(false), Some(0)) => CoverStatus::Closed,
        (Some(false), Some(1)) => CoverStatus::Open,
        // Any other raw value, or state not yet polled.
        _ => CoverStatus::Unknown,
    }
}

/// Derive `CalibratorStatus` from cached state. There's no `On` variant in
/// the ASCOM enum — `Ready` is what callers expect when the lamp is lit
/// and stable, which the FP2 always is (no warm-up).
const fn derive_calibrator_state(light_on: Option<bool>) -> CalibratorStatus {
    match light_on {
        Some(true) => CalibratorStatus::Ready,
        Some(false) => CalibratorStatus::Off,
        None => CalibratorStatus::Unknown,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn derive_cover_state_table_matches_spec() {
        // Motor running → Moving regardless of GOPS
        assert_eq!(derive_cover_state(Some(true), Some(0)), CoverStatus::Moving);
        assert_eq!(derive_cover_state(Some(true), Some(1)), CoverStatus::Moving);
        assert_eq!(derive_cover_state(Some(true), None), CoverStatus::Moving);

        // Motor stopped → use GOPS
        assert_eq!(
            derive_cover_state(Some(false), Some(0)),
            CoverStatus::Closed
        );
        assert_eq!(derive_cover_state(Some(false), Some(1)), CoverStatus::Open);
        // GOPS in-between → Unknown
        assert_eq!(
            derive_cover_state(Some(false), Some(255)),
            CoverStatus::Unknown
        );

        // No data → Unknown
        assert_eq!(derive_cover_state(None, None), CoverStatus::Unknown);
        assert_eq!(derive_cover_state(Some(false), None), CoverStatus::Unknown);
    }

    #[test]
    fn derive_calibrator_state_table_matches_spec() {
        assert_eq!(derive_calibrator_state(Some(true)), CalibratorStatus::Ready);
        assert_eq!(derive_calibrator_state(Some(false)), CalibratorStatus::Off);
        assert_eq!(derive_calibrator_state(None), CalibratorStatus::Unknown);
    }
}

#[cfg(all(test, feature = "mock"))]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unwrap_used)]
mod mock_tests {
    use super::*;
    use crate::config::{
        AlpacaServerConfig, CliOverrides, Config, CoverCalibratorConfig, SerialConfig,
    };
    use crate::mock::MockTransportFactory;
    use rusty_photon_service_lifecycle::ReloadSignal;
    use std::time::Duration;

    fn test_config() -> Config {
        Config {
            serial: SerialConfig {
                port: "/dev/mock".to_string(),
                polling_interval: Duration::from_mins(1),
                ..Default::default()
            },
            server: AlpacaServerConfig::new(0),
            cover_calibrator: test_cover_calibrator(),
        }
    }

    /// `CoverCalibratorConfig::default()` now has an empty `unique_id` (minted on
    /// first run by `materialize_identity`), which `config_actions::validate`
    /// rejects. Tests model a post-materialization config with a populated id.
    fn test_cover_calibrator() -> CoverCalibratorConfig {
        CoverCalibratorConfig {
            unique_id: "dsd-fp2-001".to_string(),
            ..CoverCalibratorConfig::default()
        }
    }

    fn make_device() -> (DsdFp2Device, MockTransportFactory) {
        let factory = MockTransportFactory::default();
        let manager = FlatPanelManager::new(&test_config(), Arc::new(factory.clone()));
        let device = DsdFp2Device::new(test_cover_calibrator(), manager);
        (device, factory)
    }

    fn make_device_with_cap(cap: u32) -> (DsdFp2Device, MockTransportFactory) {
        let factory = MockTransportFactory::default();
        let manager = FlatPanelManager::new(&test_config(), Arc::new(factory.clone()));
        let cc_config = CoverCalibratorConfig {
            max_brightness: cap,
            ..test_cover_calibrator()
        };
        let device = DsdFp2Device::new(cc_config, manager);
        (device, factory)
    }

    fn make_device_with_min(floor: u32) -> (DsdFp2Device, MockTransportFactory) {
        let factory = MockTransportFactory::default();
        let manager = FlatPanelManager::new(&test_config(), Arc::new(factory.clone()));
        let cc_config = CoverCalibratorConfig {
            min_brightness: floor,
            ..test_cover_calibrator()
        };
        let device = DsdFp2Device::new(cc_config, manager);
        (device, factory)
    }

    #[tokio::test]
    async fn device_starts_disconnected() {
        let (device, _) = make_device();
        assert!(!device.connected().await.unwrap());
        // Pre-connect reads return Unknown without error.
        assert_eq!(device.cover_state().await.unwrap(), CoverStatus::Unknown);
        assert_eq!(
            device.calibrator_state().await.unwrap(),
            CalibratorStatus::Unknown
        );
    }

    #[tokio::test]
    async fn brightness_read_when_disconnected_errors() {
        let (device, _) = make_device();
        let err = device.brightness().await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn set_connected_acquires_and_releases() {
        let (device, _) = make_device();
        device.set_connected(true).await.unwrap();
        assert!(device.connected().await.unwrap());
        device.set_connected(false).await.unwrap();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn set_connected_is_idempotent() {
        let (device, _) = make_device();
        device.set_connected(true).await.unwrap();
        device.set_connected(true).await.unwrap(); // no-op
        assert!(device.connected().await.unwrap());
        device.set_connected(false).await.unwrap();
        device.set_connected(false).await.unwrap(); // no-op
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn calibrator_on_then_off_round_trips() {
        let (device, factory) = make_device();
        device.set_connected(true).await.unwrap();
        device.calibrator_on(2048).await.unwrap();
        assert_eq!(
            device.calibrator_state().await.unwrap(),
            CalibratorStatus::Ready
        );
        assert_eq!(device.brightness().await.unwrap(), 2048);
        assert_eq!(factory.state().brightness().await, 2048);
        assert!(factory.state().light_on().await);

        device.calibrator_off().await.unwrap();
        assert_eq!(
            device.calibrator_state().await.unwrap(),
            CalibratorStatus::Off
        );
        // Brightness retained as commanded (firmware behaviour).
        assert_eq!(device.brightness().await.unwrap(), 2048);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn calibrator_on_rejects_brightness_above_max() {
        let (device, _) = make_device();
        device.set_connected(true).await.unwrap();
        let err = device
            .calibrator_on(u32::from(MAX_BRIGHTNESS) + 1)
            .await
            .unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::INVALID_VALUE);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn open_cover_then_close_cover_change_observable_state() {
        let (device, _) = make_device();
        device.set_connected(true).await.unwrap();

        // Default is closed (mock `cover_angle = 270`). Close again to
        // exercise the wire path even though it's a no-op for the cover
        // angle; the manager's snapshot is still directly poked to
        // Moving.
        device.close_cover().await.unwrap();
        // After close, our local optimistic write set motor_running =
        // Some(true). The mock reports the motor running for exactly one
        // `[GMOV]` poll after `[SMOV]` before settling (mock.rs), so the
        // cache stays consistent with the optimistic write across the
        // next poll cycle. For now we just verify the call succeeded and
        // exercise the open path too.
        device.open_cover().await.unwrap();
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn writes_when_disconnected_return_not_connected() {
        let (device, _) = make_device();
        let err = device.open_cover().await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
        let err = device.close_cover().await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
        // Above the min_brightness floor (250) so this exercises the
        // not-connected path rather than the brightness-floor rejection.
        let err = device.calibrator_on(2048).await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
        let err = device.calibrator_off().await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn max_brightness_caps_at_hardware_limit() {
        let (device, _) = make_device();
        // Config default is MAX_BRIGHTNESS; the impl caps anyway.
        assert_eq!(
            device.max_brightness().await.unwrap(),
            u32::from(MAX_BRIGHTNESS)
        );
    }

    #[tokio::test]
    async fn calibrator_on_rejects_brightness_above_configured_cap() {
        let (device, _) = make_device_with_cap(2048);
        device.set_connected(true).await.unwrap();
        // MaxBrightness must agree with what calibrator_on accepts.
        assert_eq!(device.max_brightness().await.unwrap(), 2048);

        // 2048 is at the cap — allowed.
        device.calibrator_on(2048).await.unwrap();

        // 2049 is above the configured cap but below the hardware ceiling
        // — must be rejected with INVALID_VALUE so the MaxBrightness
        // promise isn't violated.
        let err = device.calibrator_on(2049).await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::INVALID_VALUE);

        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn calibrator_on_rejects_brightness_below_configured_min() {
        let (device, _) = make_device_with_min(250);
        device.set_connected(true).await.unwrap();

        // Below the floor — rejected.
        let err = device.calibrator_on(100).await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::INVALID_VALUE);

        // At the floor — allowed.
        device.calibrator_on(250).await.unwrap();
        assert_eq!(device.brightness().await.unwrap(), 250);

        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn calibrator_on_accepts_zero_even_below_configured_min() {
        let (device, _) = make_device_with_min(250);
        device.set_connected(true).await.unwrap();

        // Zero is the ASCOM "on at zero" state, not a dim request — the
        // floor must not reject it even though 0 < min_brightness.
        device.calibrator_on(0).await.unwrap();
        assert_eq!(device.brightness().await.unwrap(), 0);

        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn calibrator_on_reports_misconfiguration_when_min_exceeds_max() {
        // config.apply rejects min_brightness > max_brightness (config_actions
        // tests), but a device built straight from a hand-edited config file
        // isn't validated — so this inconsistent state is reachable at
        // runtime. The error message must not suggest an unreachable
        // remediation ("a value >= min_brightness" when no such value fits
        // under max_brightness).
        let factory = MockTransportFactory::default();
        let manager = FlatPanelManager::new(&test_config(), Arc::new(factory));
        let cc_config = CoverCalibratorConfig {
            min_brightness: 3000,
            max_brightness: 2048,
            ..test_cover_calibrator()
        };
        let device = DsdFp2Device::new(cc_config, manager);
        device.set_connected(true).await.unwrap();

        let err = device.calibrator_on(100).await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::INVALID_VALUE);
        assert!(err.message.contains("misconfiguration"), "{}", err.message);
        assert!(!err.message.contains(">= 3000"), "{}", err.message);

        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn metadata_fields_round_trip() {
        let (device, _) = make_device();
        assert_eq!(device.static_name(), "Deep Sky Dad FP2");
        assert_eq!(device.unique_id(), "dsd-fp2-001");
        let desc = device.description().await.unwrap();
        assert!(desc.contains("Deep Sky Dad"));
        let info = device.driver_info().await.unwrap();
        assert!(info.contains("CoverCalibrator"));
        let ver = device.driver_version().await.unwrap();
        assert!(!ver.is_empty());
    }

    // --- Config actions ---------------------------------------------------

    /// Build a device wired with a config-action context backed by a temp file
    /// pre-seeded with `effective`. Returns the reload handle (clone) so tests
    /// can assert the fire-after-response reload, and the `TempDir`/path.
    fn device_with_config_actions(
        effective: Config,
    ) -> (
        DsdFp2Device,
        ReloadSignal,
        tempfile::TempDir,
        std::path::PathBuf,
    ) {
        let factory = MockTransportFactory::default();
        let manager = FlatPanelManager::new(&effective, Arc::new(factory));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dsd-fp2.json");
        std::fs::write(&path, serde_json::to_string(&effective).unwrap()).unwrap();
        let reload = ReloadSignal::new();
        let device = DsdFp2Device::new(effective.cover_calibrator.clone(), manager)
            .with_config_actions(ConfigActionCtx {
                effective,
                path: path.clone(),
                overrides: CliOverrides::default(),
                reload: reload.clone(),
            });
        (device, reload, dir, path)
    }

    #[tokio::test]
    async fn supported_actions_lists_config_actions_when_configured() {
        let (device, _reload, _dir, _path) = device_with_config_actions(test_config());
        let actions = device.supported_actions().await.unwrap();
        assert!(actions.contains(&"config.get".to_string()));
        assert!(actions.contains(&"config.apply".to_string()));
    }

    #[tokio::test]
    async fn supported_actions_empty_without_config_ctx() {
        let (device, _) = make_device();
        assert!(device.supported_actions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn config_get_returns_effective_config_and_overrides() {
        let (device, _reload, _dir, _path) = device_with_config_actions(test_config());
        // Config actions must work while disconnected.
        assert!(!device.connected().await.unwrap());
        let body = device
            .action("config.get".to_string(), String::new())
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            value
                .pointer("/config/serial/port")
                .and_then(|v| v.as_str()),
            Some("/dev/mock")
        );
        assert!(value
            .get("overrides")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn config_get_redacts_password_hash() {
        let mut effective = test_config();
        effective.server.auth = Some(rp_auth::config::AuthConfig {
            username: "obs".to_string(),
            password_hash: "$argon2id$v=19$real".to_string(),
        });
        let (device, _reload, _dir, _path) = device_with_config_actions(effective);
        let body = device
            .action("config.get".to_string(), String::new())
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            value
                .pointer("/config/server/auth/password_hash")
                .and_then(|v| v.as_str()),
            Some(crate::config_actions::REDACTED)
        );
    }

    #[tokio::test]
    async fn config_apply_persists_and_fires_reload() {
        let (device, reload, _dir, path) = device_with_config_actions(test_config());
        let mut changed = test_config();
        changed.cover_calibrator.max_brightness = 2048;
        let params = serde_json::to_string(&changed).unwrap();

        let body = device
            .action("config.apply".to_string(), params)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            value.get("status").and_then(|v| v.as_str()),
            Some("applying")
        );
        let reload_paths = value.get("reload").and_then(|v| v.as_array()).unwrap();
        assert!(reload_paths
            .iter()
            .any(|p| p.as_str() == Some("cover_calibrator.max_brightness")));

        // Persisted to disk with the new value.
        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            persisted
                .pointer("/cover_calibrator/max_brightness")
                .and_then(serde_json::Value::as_u64),
            Some(2048)
        );

        // The reload is fired after the response — it must arrive.
        tokio::time::timeout(Duration::from_secs(2), reload.recv())
            .await
            .expect("config.apply should fire the reload");
    }

    #[tokio::test]
    async fn config_apply_without_change_returns_ok() {
        let (device, _reload, _dir, _path) = device_with_config_actions(test_config());
        let params = serde_json::to_string(&test_config()).unwrap();
        let body = device
            .action("config.apply".to_string(), params)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value.get("status").and_then(|v| v.as_str()), Some("ok"));
    }

    #[tokio::test]
    async fn config_apply_invalid_leaves_file_unchanged() {
        let (device, _reload, _dir, path) = device_with_config_actions(test_config());
        let before = std::fs::read_to_string(&path).unwrap();
        let mut bad = test_config();
        bad.serial.baud_rate = 0; // fails validation
        let params = serde_json::to_string(&bad).unwrap();

        let body = device
            .action("config.apply".to_string(), params)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            value.get("status").and_then(|v| v.as_str()),
            Some("invalid")
        );
        assert!(!value
            .get("errors")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn config_apply_rejects_non_json() {
        let (device, _reload, _dir, _path) = device_with_config_actions(test_config());
        let err = device
            .action("config.apply".to_string(), "not json".to_string())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[tokio::test]
    async fn unknown_action_returns_action_not_implemented() {
        let (device, _reload, _dir, _path) = device_with_config_actions(test_config());
        let err = device
            .action("config.frobnicate".to_string(), String::new())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::ACTION_NOT_IMPLEMENTED);
    }
}
