//! Falcon Rotator ASCOM device implementation
//!
//! Wraps [`FalconManager`] behind the ASCOM `Device` + `Rotator` traits.
//! Every property read maps to one serial command — see the design doc's
//! [Why no cache](../../../docs/services/falcon-rotator.md#why-no-cache)
//! section for the rationale.
//!
//! Connection state is the device's [`Session<FalconCodec>`] slot: when
//! it's `Some`, we hold a live handle to the shared transport; when it's
//! `None`, we don't. The "requested" bool that previously diverged from
//! the transport refcount is gone by construction (the race the
//! `connect_lock` defended against in `serial_manager.rs` is now
//! impossible because the flag *is* the resource).

use std::sync::Arc;

use ascom_alpaca::api::{Device, Rotator};
use ascom_alpaca::{ASCOMError, ASCOMResult};
use async_trait::async_trait;
use rusty_photon_shared_transport::Session;
use tokio::sync::RwLock;
use tracing::debug;

use crate::codec::FalconCodec;
use crate::config::RotatorConfig;
use crate::config_actions::FalconRotatorDriver;
use crate::error::FalconRotatorError;
use crate::manager::FalconManager;
use crate::units::{MechanicalDegrees, SkyDegrees};
use rusty_photon_driver::ConfigActionCtx;

/// Guard macro that returns `NOT_CONNECTED` if the device is not connected.
///
/// Mirrors the qhy-focuser / ppba-driver pattern: a single
/// `ensure_connected!` line at the top of each device-bound method so
/// disconnected reads/writes never reach the manager.
macro_rules! ensure_connected {
    ($self:ident) => {
        if !$self.connected().await.is_ok_and(|connected| connected) {
            debug!("Falcon Rotator device not connected");
            return Err(ASCOMError::NOT_CONNECTED);
        }
    };
}

/// Falcon Rotator device for ASCOM Alpaca.
#[derive(derive_more::Debug)]
pub struct FalconRotatorDevice {
    config: RotatorConfig,
    /// `Some` between successful acquire and explicit close. The session
    /// existing is the truth — no second-source bool to desync. The
    /// write lock spans every `set_connected` check-and-modify so two
    /// concurrent `Connected=true` requests can't both observe `None`
    /// and both call `acquire()` (PR #241 round-5 race fix).
    #[debug(skip)]
    session: Arc<RwLock<Option<Session<FalconCodec>>>>,
    #[debug(skip)]
    manager: Arc<FalconManager>,
    /// Shared (cloned) config-action context; `Some` on the normal path through
    /// `ServerBuilder`, `None` for focused unit-test devices.
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<FalconRotatorDriver>>,
}

impl FalconRotatorDevice {
    pub fn new(config: RotatorConfig, manager: Arc<FalconManager>) -> Self {
        Self {
            config,
            session: Arc::new(RwLock::new(None)),
            manager,
            config_ctx: None,
        }
    }

    /// Attach the shared config-action context, enabling `config.get` /
    /// `config.apply` / `config.schema` on this device.
    #[must_use]
    pub fn with_config_actions(mut self, ctx: ConfigActionCtx<FalconRotatorDriver>) -> Self {
        self.config_ctx = Some(ctx);
        self
    }

    /// Borrow the held session for one request. Returns `NotConnected` if
    /// the device's session slot is empty.
    async fn with_session<F, T>(&self, f: F) -> ASCOMResult<T>
    where
        F: AsyncFnOnce(&Session<FalconCodec>) -> Result<T, FalconRotatorError>,
    {
        let guard = self.session.read().await;
        let session = guard.as_ref().ok_or(FalconRotatorError::NotConnected)?;
        Ok(f(session).await?)
    }
}

#[async_trait]
impl Device for FalconRotatorDevice {
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
        Ok(self.session.read().await.is_some() && self.manager.is_available())
    }

    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        // The write lock spans the whole check-and-modify so two concurrent
        // `Connected=true` requests can't both observe `None` and both
        // call `acquire()` (PR #241 round-5 / issue #251 fix shape). With
        // the session slot replacing the old `requested` bool, the flag
        // and the resource are the same value — there is no second source
        // to desync.
        let mut slot = self.session.write().await;
        match (connected, slot.is_some()) {
            (true, false) => {
                // `?` does SessionError → FalconRotatorError via the
                // .map_err (the SessionError generic carries
                // FalconCodecError), then FalconRotatorError → ASCOMError
                // via the From impl in error.rs.
                let session = self
                    .manager
                    .transport()
                    .acquire()
                    .await
                    .map_err(FalconRotatorError::from)?;
                *slot = Some(session);
                debug!("Rotator device connected");
            }
            (false, true) => {
                if let Some(session) = slot.take() {
                    // `Session::close` returns Result<_, TransportError>;
                    // `From<TransportError> for FalconRotatorError`
                    // handles the conversion, and the existing
                    // `From<FalconRotatorError> for ASCOMError` does the
                    // second hop on `?`.
                    session.close().await.map_err(FalconRotatorError::from)?;
                }
                // Only reset per-session driver state when this disconnect
                // truly closed the transport. If the status switch device
                // still holds a session, the manager stays available and
                // the shared state (sync_offset, target_position,
                // limit-detect edge tracker) must stay intact for it.
                // Mirrors the guard on `FalconStatusSwitchDevice::set_connected`.
                if !self.manager.is_available() {
                    self.manager.clear_session_state().await;
                }
                debug!("Rotator device disconnected");
            }
            _ => {}
        }
        Ok(())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("Pegasus Falcon Rotator Driver - ASCOM Alpaca interface".to_string())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }

    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(rusty_photon_driver::supported_actions(&self.config_ctx))
    }

    async fn action(&self, action: String, parameters: String) -> ASCOMResult<String> {
        rusty_photon_driver::dispatch::<FalconRotatorDriver>(&self.config_ctx, action, parameters)
            .await
    }
}

#[async_trait]
impl Rotator for FalconRotatorDevice {
    async fn can_reverse(&self) -> ASCOMResult<bool> {
        Ok(true)
    }

    async fn is_moving(&self) -> ASCOMResult<bool> {
        ensure_connected!(self);
        self.with_session(async |session| {
            let status = self.manager.read_status(session).await?;
            Ok(status.motion.is_moving)
        })
        .await
    }

    async fn position(&self) -> ASCOMResult<f64> {
        ensure_connected!(self);
        let offset = self.manager.sync_offset().await;
        self.with_session(async |session| {
            let status = self.manager.read_status(session).await?;
            Ok((status.position_deg + offset).value())
        })
        .await
    }

    async fn mechanical_position(&self) -> ASCOMResult<f64> {
        ensure_connected!(self);
        self.with_session(async |session| {
            let status = self.manager.read_status(session).await?;
            Ok(status.position_deg.value())
        })
        .await
    }

    async fn target_position(&self) -> ASCOMResult<f64> {
        ensure_connected!(self);
        if let Some(target) = self.manager.target_position().await {
            return Ok(target.value());
        }
        // No move outstanding: fall back to current Position. Matches the
        // design doc's TargetPosition row and the dominant ASCOM convention.
        let offset = self.manager.sync_offset().await;
        self.with_session(async |session| {
            let status = self.manager.read_status(session).await?;
            Ok((status.position_deg + offset).value())
        })
        .await
    }

    async fn reverse(&self) -> ASCOMResult<bool> {
        ensure_connected!(self);
        self.with_session(async |session| {
            let status = self.manager.read_status(session).await?;
            Ok(status.settings.motor_reverse)
        })
        .await
    }

    async fn set_reverse(&self, reverse: bool) -> ASCOMResult<()> {
        ensure_connected!(self);
        self.with_session(async |session| self.manager.set_reverse(session, reverse).await)
            .await
    }

    async fn step_size(&self) -> ASCOMResult<f64> {
        // Vendor product page: 86.6 steps per degree → 1.0 / 86.6 ≈ 0.01155°.
        Ok(0.01155)
    }

    async fn halt(&self) -> ASCOMResult<()> {
        ensure_connected!(self);
        self.with_session(async |session| self.manager.halt(session).await)
            .await
    }

    async fn move_(&self, position: f64) -> ASCOMResult<()> {
        ensure_connected!(self);
        if !position.is_finite() {
            return Err(FalconRotatorError::InvalidValue(format!(
                "Move delta must be finite, got {position}"
            ))
            .to_ascom_error());
        }
        let offset = self.manager.sync_offset().await;
        let wire_mech = self
            .with_session(async |session| {
                let status = self.manager.read_status(session).await?;
                // ASCOM `Move(delta)` is in sky coordinates: the new sky
                // position is (mech + offset) + delta, and the mechanical
                // wire value is therefore (mech + offset + delta) - offset
                // = mech + delta.
                let target_mech = status.position_deg.rotate(position);
                // Set TargetPosition only after the MD command has been
                // accepted — a failed echo on the wire must NOT leave a
                // stale target the client could read back via
                // TargetPosition (PR #241 round-5). Derive TargetPosition
                // from the wire-quantised value so a near-boundary input
                // (e.g. 359.999 → MD:0.00) doesn't leave a stored target
                // the device was never told to reach.
                self.manager.move_mechanical(session, target_mech).await
            })
            .await?;
        let target_sky = wire_mech + offset;
        self.manager.set_target_position(target_sky).await;
        Ok(())
    }

    async fn move_absolute(&self, position: f64) -> ASCOMResult<()> {
        ensure_connected!(self);
        if !position.is_finite() {
            return Err(FalconRotatorError::InvalidValue(format!(
                "MoveAbsolute target must be finite, got {position}"
            ))
            .to_ascom_error());
        }
        let offset = self.manager.sync_offset().await;
        let target_mech = SkyDegrees::new(position) - offset;
        let wire_mech = self
            .with_session(async |session| self.manager.move_mechanical(session, target_mech).await)
            .await?;
        let target_sky = wire_mech + offset;
        self.manager.set_target_position(target_sky).await;
        Ok(())
    }

    async fn move_mechanical(&self, position: f64) -> ASCOMResult<()> {
        ensure_connected!(self);
        if !position.is_finite() {
            return Err(FalconRotatorError::InvalidValue(format!(
                "MoveMechanical target must be finite, got {position}"
            ))
            .to_ascom_error());
        }
        // Per the design-doc mapping table, MoveMechanical does NOT
        // subtract the sync offset from the wire value — the caller asked
        // for a specific mechanical angle. Internal target_position is
        // still stored in sky coordinates so subsequent TargetPosition
        // reads stay frame-consistent across the three Move variants.
        let offset = self.manager.sync_offset().await;
        let wire_mech = self
            .with_session(async |session| {
                self.manager
                    .move_mechanical(session, MechanicalDegrees::new(position))
                    .await
            })
            .await?;
        let target_sky = wire_mech + offset;
        self.manager.set_target_position(target_sky).await;
        Ok(())
    }

    async fn sync(&self, position: f64) -> ASCOMResult<()> {
        ensure_connected!(self);
        // FalconManager::sync validates finiteness, reads FA, and
        // computes the driver-side offset. ASCOM Sync never touches the
        // device; the SD wire command is deliberately absent from the
        // protocol enum.
        self.with_session(async |session| self.manager.sync(session, position).await)
            .await
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};

    /// Test-only no-op factory: the connection-guard tests assert
    /// behaviour while disconnected, so `open` is never called.
    struct NoopFactory;

    #[async_trait]
    impl TransportFactory for NoopFactory {
        #[allow(clippy::unimplemented)]
        async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
            unimplemented!("test factory should never open a transport")
        }
    }

    fn disconnected_device() -> FalconRotatorDevice {
        let manager = FalconManager::new(Arc::new(NoopFactory));
        FalconRotatorDevice::new(RotatorConfig::default(), manager)
    }

    #[tokio::test]
    async fn can_reverse_is_always_true() {
        let device = disconnected_device();
        assert!(device.can_reverse().await.unwrap());
    }

    #[tokio::test]
    async fn step_size_matches_vendor_product_page() {
        let device = disconnected_device();
        let step = device.step_size().await.unwrap();
        assert!((step - 0.01155).abs() < 1e-9);
    }

    #[tokio::test]
    async fn static_name_comes_from_config() {
        let device = disconnected_device();
        assert_eq!(device.static_name(), "Pegasus Falcon Rotator");
    }

    #[tokio::test]
    async fn unique_id_comes_from_config() {
        let manager = FalconManager::new(Arc::new(NoopFactory));
        let device = FalconRotatorDevice::new(
            RotatorConfig {
                unique_id: "test-rotator-unique-id".to_string(),
                ..RotatorConfig::default()
            },
            manager,
        );
        assert_eq!(device.unique_id(), "test-rotator-unique-id");
    }

    #[tokio::test]
    async fn driver_version_matches_cargo_pkg_version() {
        let device = disconnected_device();
        assert_eq!(
            device.driver_version().await.unwrap(),
            env!("CARGO_PKG_VERSION")
        );
    }

    #[tokio::test]
    async fn description_comes_from_config() {
        let device = disconnected_device();
        assert_eq!(
            device.description().await.unwrap(),
            "Pegasus Astro Falcon Rotator (firmware >= 1.3)"
        );
    }

    #[tokio::test]
    async fn driver_info_mentions_alpaca() {
        let device = disconnected_device();
        let info = device.driver_info().await.unwrap();
        assert!(info.contains("Alpaca"), "got: {info}");
    }

    #[test]
    fn debug_format_includes_struct_name() {
        let device = disconnected_device();
        let s = format!("{device:?}");
        assert!(s.contains("FalconRotatorDevice"), "got: {s}");
    }

    // ---- Disconnected guards: each method must short-circuit with NOT_CONNECTED.

    #[tokio::test]
    async fn position_when_disconnected_returns_not_connected() {
        let device = disconnected_device();
        let err = device.position().await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn mechanical_position_when_disconnected_returns_not_connected() {
        let device = disconnected_device();
        let err = device.mechanical_position().await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn target_position_when_disconnected_returns_not_connected() {
        let device = disconnected_device();
        let err = device.target_position().await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn is_moving_when_disconnected_returns_not_connected() {
        let device = disconnected_device();
        let err = device.is_moving().await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn reverse_get_when_disconnected_returns_not_connected() {
        let device = disconnected_device();
        let err = device.reverse().await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn set_reverse_when_disconnected_returns_not_connected() {
        let device = disconnected_device();
        let err = device.set_reverse(true).await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn halt_when_disconnected_returns_not_connected() {
        let device = disconnected_device();
        let err = device.halt().await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn move_when_disconnected_returns_not_connected() {
        let device = disconnected_device();
        let err = device.move_(10.0).await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn move_absolute_when_disconnected_returns_not_connected() {
        let device = disconnected_device();
        let err = device.move_absolute(45.0).await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn move_mechanical_when_disconnected_returns_not_connected() {
        let device = disconnected_device();
        let err = device.move_mechanical(45.0).await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn sync_when_disconnected_returns_not_connected() {
        let device = disconnected_device();
        let err = device.sync(45.0).await.unwrap_err();
        assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_CONNECTED);
    }
}

/// Mock-backed integration tests for the rotator device. Exercises the
/// arithmetic and command routing through [`FalconManager`] against the
/// deterministic [`MockFalconTransportFactory`](crate::mock::MockFalconTransportFactory).
/// Race / refcount / rollback invariants are tested once in
/// `rusty-photon-shared-transport`'s own test suite — not duplicated here.
#[cfg(all(test, feature = "mock"))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod mock_tests {
    use super::*;
    use crate::mock::MockFalconTransportFactory;

    fn connected_device_with(
        factory: Arc<MockFalconTransportFactory>,
    ) -> (
        FalconRotatorDevice,
        Arc<FalconManager>,
        Arc<MockFalconTransportFactory>,
    ) {
        let manager = FalconManager::new(Arc::<MockFalconTransportFactory>::clone(&factory));
        let device = FalconRotatorDevice::new(RotatorConfig::default(), Arc::clone(&manager));
        (device, manager, factory)
    }

    #[tokio::test]
    async fn move_absolute_subtracts_sync_offset_from_wire_value() {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, factory) = connected_device_with(factory);
        device.set_connected(true).await.unwrap();
        factory.set_mech_position_deg(0.0).await;
        device.sync(255.20).await.unwrap();
        factory.clear_command_log().await;

        device.move_absolute(180.0).await.unwrap();
        // target_mech = (180 - 255.20) mod 360 = -75.20 mod 360 = 284.80
        let log = factory.command_log().await;
        assert!(log.contains(&"MD:284.80".to_string()), "got: {log:?}");
    }

    #[tokio::test]
    async fn move_mechanical_does_not_subtract_sync_offset() {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, factory) = connected_device_with(factory);
        device.set_connected(true).await.unwrap();
        factory.set_mech_position_deg(0.0).await;
        device.sync(255.20).await.unwrap();
        factory.clear_command_log().await;

        device.move_mechanical(90.0).await.unwrap();
        let log = factory.command_log().await;
        assert!(log.contains(&"MD:90.00".to_string()), "got: {log:?}");
    }

    #[tokio::test]
    async fn move_with_relative_delta_targets_current_mech_plus_delta() {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, factory) = connected_device_with(factory);
        device.set_connected(true).await.unwrap();
        factory.set_mech_position_deg(350.0).await;
        device.sync(320.0).await.unwrap(); // offset = (320 - 350) mod 360 = 330
        factory.clear_command_log().await;

        device.move_(20.0).await.unwrap();
        // target_sky = (350 + 330 + 20) mod 360 = 700 mod 360 = 340
        // target_mech = (340 - 330) mod 360 = 10
        let log = factory.command_log().await;
        assert!(log.contains(&"MD:10.00".to_string()), "got: {log:?}");
    }

    #[tokio::test]
    async fn target_position_after_move_returns_requested_sky_angle() {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, factory) = connected_device_with(factory);
        device.set_connected(true).await.unwrap();
        factory.set_mech_position_deg(0.0).await;
        device.sync(45.0).await.unwrap();

        device.move_absolute(180.0).await.unwrap();

        let target = device.target_position().await.unwrap();
        assert!((target - 180.0).abs() < 1e-9, "got {target}");
    }

    #[tokio::test]
    async fn target_position_with_no_move_falls_back_to_position() {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, factory) = connected_device_with(factory);
        device.set_connected(true).await.unwrap();
        factory.set_mech_position_deg(90.0).await;

        let target = device.target_position().await.unwrap();
        assert!((target - 90.0).abs() < 1e-9, "got {target}");
    }

    #[tokio::test]
    async fn position_adds_sync_offset_to_mechanical() {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, factory) = connected_device_with(factory);
        device.set_connected(true).await.unwrap();
        factory.set_mech_position_deg(142.30).await;
        device.sync(37.50).await.unwrap();

        let pos = device.position().await.unwrap();
        assert!((pos - 37.50).abs() < 1e-9, "got {pos}");
        let mech = device.mechanical_position().await.unwrap();
        assert!((mech - 142.30).abs() < 1e-9, "got {mech}");
    }

    #[tokio::test]
    async fn move_absolute_rejects_non_finite() {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, _factory) = connected_device_with(factory);
        device.set_connected(true).await.unwrap();

        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = device.move_absolute(bad).await.unwrap_err();
            assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::INVALID_VALUE);
        }
    }

    #[tokio::test]
    async fn move_rejects_non_finite() {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, _factory) = connected_device_with(factory);
        device.set_connected(true).await.unwrap();

        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = device.move_(bad).await.unwrap_err();
            assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::INVALID_VALUE);
        }
    }

    #[tokio::test]
    async fn move_mechanical_rejects_non_finite() {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, _factory) = connected_device_with(factory);
        device.set_connected(true).await.unwrap();

        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = device.move_mechanical(bad).await.unwrap_err();
            assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::INVALID_VALUE);
        }
    }

    #[tokio::test]
    async fn move_absolute_target_position_matches_wire_value_at_boundary() {
        // 359.999 quantises to 360.00 then normalises to 0.00 on the wire.
        // TargetPosition must follow the wire value so a client comparing
        // it against Position after IsMoving=false doesn't see a stale
        // 359.999 vs a fresh 0.00 — PR #241 round-7 Copilot comment.
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, _factory) = connected_device_with(factory);
        device.set_connected(true).await.unwrap();

        device.move_absolute(359.999).await.unwrap();

        let target = device.target_position().await.unwrap();
        assert!(
            (target - 0.0).abs() < 1e-9,
            "expected TargetPosition to track the wire-quantised value (0.0), got {target}"
        );
    }

    #[tokio::test]
    async fn sync_rejects_non_finite() {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, _factory) = connected_device_with(factory);
        device.set_connected(true).await.unwrap();

        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = device.sync(bad).await.unwrap_err();
            assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::INVALID_VALUE);
        }
    }

    #[tokio::test]
    async fn halt_sends_fh() {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, factory) = connected_device_with(factory);
        device.set_connected(true).await.unwrap();
        factory.clear_command_log().await;

        device.halt().await.unwrap();
        let log = factory.command_log().await;
        assert!(log.contains(&"FH".to_string()), "got: {log:?}");
    }

    #[tokio::test]
    async fn disconnect_then_reconnect_resets_sync_offset() {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, factory) = connected_device_with(factory);
        device.set_connected(true).await.unwrap();
        factory.set_mech_position_deg(120.0).await;
        device.sync(30.0).await.unwrap();

        device.set_connected(false).await.unwrap();
        device.set_connected(true).await.unwrap();

        // After reconnect, the sync offset must be reset by
        // `clear_session_state` (the rotator was the only session holder,
        // so its disconnect was the 1→0 transition that closed the
        // transport). Position == MechanicalPosition confirms it.
        let pos = device.position().await.unwrap();
        let mech = device.mechanical_position().await.unwrap();
        assert!(
            (pos - mech).abs() < 1e-9,
            "expected sync_offset reset on reconnect: position={pos}, mechanical={mech}"
        );
    }

    #[tokio::test]
    async fn rotator_disconnect_preserves_state_while_switch_still_connected() {
        // Regression test for the bug Copilot flagged on PR #282: if the
        // status switch is still holding a session, the rotator's
        // disconnect must NOT clear shared driver state (sync_offset,
        // target_position, limit-detect edge tracker) — the switch's
        // `read_status` calls still rely on the edge tracker, and any
        // `TargetPosition` reads via the manager would see stale `None`.
        //
        // The fix gates `clear_session_state` on `!manager.is_available()`
        // so it only runs when this disconnect was the one that actually
        // closed the transport.
        use crate::switch_device::FalconStatusSwitchDevice;
        use crate::SwitchConfig;

        let factory = Arc::new(MockFalconTransportFactory::default());
        let manager = FalconManager::new(Arc::<MockFalconTransportFactory>::clone(&factory));
        let rotator = FalconRotatorDevice::new(RotatorConfig::default(), Arc::clone(&manager));
        let switch = FalconStatusSwitchDevice::new(SwitchConfig::default(), Arc::clone(&manager));

        // Connect both devices; seed sync_offset via the rotator.
        rotator.set_connected(true).await.unwrap();
        switch.set_connected(true).await.unwrap();
        factory.set_mech_position_deg(120.0).await;
        rotator.sync(30.0).await.unwrap();
        let offset_before = manager.sync_offset().await.value();
        assert!(
            (offset_before - 270.0).abs() < 1e-9,
            "offset_before={offset_before}"
        );

        // Disconnect ONLY the rotator. The switch is still holding a
        // session, so the transport remains available and the shared
        // state must stay intact.
        rotator.set_connected(false).await.unwrap();
        assert!(
            manager.is_available(),
            "switch still holds a session — transport must stay open"
        );
        let offset_after = manager.sync_offset().await.value();
        assert!(
            (offset_after - offset_before).abs() < 1e-9,
            "sync_offset must not be cleared while another device still holds a session; \
             before={offset_before}, after={offset_after}"
        );

        // Cleanup.
        switch.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn connect_then_disconnect_round_trip() {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, _factory) = connected_device_with(factory);
        assert!(!device.connected().await.unwrap());
        device.set_connected(true).await.unwrap();
        assert!(device.connected().await.unwrap());
        device.set_connected(false).await.unwrap();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn set_connected_is_idempotent() {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let (device, _manager, _factory) = connected_device_with(factory);
        device.set_connected(true).await.unwrap();
        device.set_connected(true).await.unwrap();
        assert!(device.connected().await.unwrap());
        device.set_connected(false).await.unwrap();
        device.set_connected(false).await.unwrap();
        assert!(!device.connected().await.unwrap());
    }
}
