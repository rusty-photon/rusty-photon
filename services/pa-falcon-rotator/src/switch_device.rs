//! Falcon Status Switch ASCOM device implementation
//!
//! Exposes two read-only switches alongside the main Rotator device:
//! - id `0`: input voltage (raw ADC count from `VS`)
//! - id `1`: limit-hit boolean from `FA.limit_detect`
//!
//! See the design doc's
//! [Status Switch Device](../../../docs/services/falcon-rotator.md#status-switch-device)
//! section for the contract.
//!
//! Connection state is the device's [`Session<FalconCodec>`] slot, the
//! same shape as [`crate::rotator_device::FalconRotatorDevice`]. The two
//! devices share a single [`FalconManager`] and therefore a single
//! [`rusty_photon_shared_transport::SharedTransport`] — refcounting on
//! that transport is what makes both devices' connect/disconnect calls
//! cooperate on one open serial port.

use std::sync::Arc;

use ascom_alpaca::api::{Device, Switch};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use async_trait::async_trait;
use rusty_photon_shared_transport::Session;
use tokio::sync::RwLock;
use tracing::debug;

use crate::codec::FalconCodec;
use crate::config::SwitchConfig;
use crate::config_actions::FalconRotatorDriver;
use crate::error::FalconRotatorError;
use crate::manager::FalconManager;
use rusty_photon_driver::ConfigActionCtx;

/// Id 0: input voltage (raw ADC count from the Falcon's `VS` command).
const SWITCH_ID_VOLTAGE: usize = 0;
/// Id 1: limit-hit flag (mirrors `FA.limit_detect`).
const SWITCH_ID_LIMIT: usize = 1;

/// Voltage-switch metadata pinned by the design doc's Switch layout
/// table: `MaxSwitchValue = 1023` assumes a 10-bit ADC on the Falcon's
/// MCU; widening it is a follow-up tracked in the design doc once
/// hardware characterisation is in hand.
const VOLTAGE_SWITCH_NAME: &str = "Input Voltage (raw)";
const VOLTAGE_SWITCH_DESCRIPTION: &str =
    "Raw input-voltage ADC count from the Falcon's VS command; scale not yet calibrated";
const VOLTAGE_MIN_VALUE: f64 = 0.0;
const VOLTAGE_MAX_VALUE: f64 = 1023.0;
const VOLTAGE_STEP: f64 = 1.0;

/// Limit-hit-switch metadata: boolean (0/1) mirror of `FA.limit_detect`.
const LIMIT_SWITCH_NAME: &str = "Limit Hit";
const LIMIT_SWITCH_DESCRIPTION: &str = "Mirrors FA.limit_detect for the most recent status read";
const LIMIT_MIN_VALUE: f64 = 0.0;
const LIMIT_MAX_VALUE: f64 = 1.0;
const LIMIT_STEP: f64 = 1.0;

/// Guard that returns `NOT_CONNECTED` if the device is not connected.
/// Mirrors `ppba-driver`'s `ensure_connected!` macro: every device-bound
/// Switch method runs this **before** id validation so a disconnected
/// client always sees `NOT_CONNECTED` (1031) regardless of the id
/// passed in, matching the design doc's
/// [Error Model](../../../docs/services/falcon-rotator.md#error-model).
macro_rules! ensure_connected {
    ($self:ident) => {
        if !$self.connected().await.is_ok_and(|connected| connected) {
            debug!("Falcon Status Switch device not connected");
            return Err(ASCOMError::NOT_CONNECTED);
        }
    };
}

/// Typed switch-id discriminant. Constructed via [`SwitchId::try_from`];
/// every `Switch` trait method that takes a `usize` id parses it once at
/// the boundary, then matches on this enum exhaustively. Replaces the
/// previous untyped `validate_id` + `match id { _ => unreachable!(...) }`
/// pattern so the compiler proves all id cases are handled.
#[derive(Debug, Clone, Copy, strum::EnumCount)]
enum SwitchId {
    /// Id 0: input voltage (raw ADC count from `VS`).
    Voltage,
    /// Id 1: limit-hit flag (mirrors `FA.limit_detect`).
    Limit,
}

/// Number of switches advertised by this device. The design doc pins this at 2
/// (id 0 = voltage, id 1 = limit-hit); any other id is out of range. Derived
/// from [`SwitchId`]'s variant list, so the advertised `MaxSwitch` and the
/// ids [`SwitchId::try_from`] accepts cannot disagree.
const SWITCH_COUNT: usize = <SwitchId as strum::EnumCount>::COUNT;

impl TryFrom<usize> for SwitchId {
    type Error = ASCOMError;

    /// Parse a raw `usize` switch id from the ASCOM `Switch` trait into
    /// the typed discriminant, or reject it with `INVALID_VALUE` per the
    /// ASCOM convention (id-range validation precedes operation-permission
    /// checks so out-of-range ids never hit `INVALID_OPERATION` paths).
    fn try_from(id: usize) -> ASCOMResult<Self> {
        match id {
            SWITCH_ID_VOLTAGE => Ok(Self::Voltage),
            SWITCH_ID_LIMIT => Ok(Self::Limit),
            _ => Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Switch id {id} out of range (valid: 0..{SWITCH_COUNT})"),
            )),
        }
    }
}

/// Falcon Status Switch device for ASCOM Alpaca.
#[derive(derive_more::Debug)]
pub struct FalconStatusSwitchDevice {
    config: SwitchConfig,
    /// `Some` between successful acquire and explicit close. The session
    /// existing is the truth — no second-source bool to desync.
    #[debug(skip)]
    session: Arc<RwLock<Option<Session<FalconCodec>>>>,
    #[debug(skip)]
    manager: Arc<FalconManager>,
    /// Shared (cloned) config-action context; `Some` on the normal path through
    /// `ServerBuilder`, `None` for focused unit-test devices.
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<FalconRotatorDriver>>,
}

impl FalconStatusSwitchDevice {
    pub fn new(config: SwitchConfig, manager: Arc<FalconManager>) -> Self {
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
impl Device for FalconStatusSwitchDevice {
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
        // Hold the write lock across the whole check-and-modify so two
        // concurrent `Connected=true` requests for this switch don't both
        // observe `None` and both call `acquire()` (same fix shape as
        // `FalconRotatorDevice` — PR #241 round-5).
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
                debug!("Status Switch device connected");
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
                // truly closed the transport. If the rotator device still
                // holds a session, the manager stays available and state
                // should stay intact for it.
                if !self.manager.is_available() {
                    self.manager.clear_session_state().await;
                }
                debug!("Status Switch device disconnected");
            }
            _ => {}
        }
        Ok(())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("Pegasus Falcon Rotator Status Switch - ASCOM Alpaca interface".to_string())
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
impl Switch for FalconStatusSwitchDevice {
    async fn max_switch(&self) -> ASCOMResult<usize> {
        Ok(SWITCH_COUNT)
    }

    async fn can_write(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);
        SwitchId::try_from(id)?;
        Ok(false)
    }

    async fn get_switch_name(&self, id: usize) -> ASCOMResult<String> {
        ensure_connected!(self);
        let name = match SwitchId::try_from(id)? {
            SwitchId::Voltage => VOLTAGE_SWITCH_NAME,
            SwitchId::Limit => LIMIT_SWITCH_NAME,
        };
        Ok(name.to_string())
    }

    async fn get_switch_description(&self, id: usize) -> ASCOMResult<String> {
        ensure_connected!(self);
        let description = match SwitchId::try_from(id)? {
            SwitchId::Voltage => VOLTAGE_SWITCH_DESCRIPTION,
            SwitchId::Limit => LIMIT_SWITCH_DESCRIPTION,
        };
        Ok(description.to_string())
    }

    async fn get_switch(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);
        let switch = SwitchId::try_from(id)?;
        // ASCOM rule: GetSwitch returns false at MinSwitchValue, true
        // otherwise. For the voltage switch (Min = 0) that means
        // "true iff raw > 0". For the limit-hit switch (Min = 0, Max = 1)
        // the 0.5 threshold is the conventional midpoint test, matching
        // the design doc's contract.
        let value = self.get_switch_value(id).await?;
        let threshold = match switch {
            SwitchId::Voltage => VOLTAGE_MIN_VALUE,
            SwitchId::Limit => 0.5,
        };
        Ok(value > threshold)
    }

    async fn get_switch_value(&self, id: usize) -> ASCOMResult<f64> {
        ensure_connected!(self);
        match SwitchId::try_from(id)? {
            SwitchId::Voltage => {
                self.with_session(async |session| {
                    let v = self.manager.read_voltage_raw(session).await?;
                    Ok(f64::from(v))
                })
                .await
            }
            SwitchId::Limit => {
                self.with_session(async |session| {
                    let status = self.manager.read_status(session).await?;
                    Ok(if status.motion.limit_detect { 1.0 } else { 0.0 })
                })
                .await
            }
        }
    }

    async fn min_switch_value(&self, id: usize) -> ASCOMResult<f64> {
        ensure_connected!(self);
        Ok(match SwitchId::try_from(id)? {
            SwitchId::Voltage => VOLTAGE_MIN_VALUE,
            SwitchId::Limit => LIMIT_MIN_VALUE,
        })
    }

    async fn max_switch_value(&self, id: usize) -> ASCOMResult<f64> {
        ensure_connected!(self);
        Ok(match SwitchId::try_from(id)? {
            SwitchId::Voltage => VOLTAGE_MAX_VALUE,
            SwitchId::Limit => LIMIT_MAX_VALUE,
        })
    }

    async fn switch_step(&self, id: usize) -> ASCOMResult<f64> {
        ensure_connected!(self);
        Ok(match SwitchId::try_from(id)? {
            SwitchId::Voltage => VOLTAGE_STEP,
            SwitchId::Limit => LIMIT_STEP,
        })
    }

    async fn state_change_complete(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);
        SwitchId::try_from(id)?;
        // Read-only switches never change asynchronously.
        Ok(true)
    }

    // Both advertised switches are read-only (`CanWrite = false`). ConformU
    // (and the ASCOM Switch spec) treats "no writable switches" as a
    // capability gap rather than a state-dependent rejection, so the wire
    // error is `NOT_IMPLEMENTED` (1024) not `INVALID_OPERATION` (1035).
    // `connection_guard_precedes_id_validation` still holds: when
    // disconnected, the guard fires before id validation regardless of the
    // not-implemented body below.

    async fn set_switch(&self, id: usize, _state: bool) -> ASCOMResult<()> {
        ensure_connected!(self);
        SwitchId::try_from(id)?;
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn set_switch_value(&self, id: usize, _value: f64) -> ASCOMResult<()> {
        ensure_connected!(self);
        SwitchId::try_from(id)?;
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn set_switch_name(&self, id: usize, _name: String) -> ASCOMResult<()> {
        ensure_connected!(self);
        SwitchId::try_from(id)?;
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    // ISwitchV3 async surface. The trait defaults return `Ok(false)` for
    // `can_async` and `NOT_IMPLEMENTED` for the three writers, *without*
    // running id validation. ConformU flags both: it expects an
    // InvalidValueException when called with `id >= MaxSwitch` regardless
    // of whether the device supports the operation. Overriding here
    // chains `ensure_connected!` + `SwitchId::try_from` before the trait-default
    // body so out-of-range ids return `INVALID_VALUE` (or `NOT_CONNECTED`
    // when disconnected, matching the rest of the surface).

    async fn can_async(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);
        SwitchId::try_from(id)?;
        Ok(false)
    }

    async fn set_async(&self, id: usize, _state: bool) -> ASCOMResult<()> {
        ensure_connected!(self);
        SwitchId::try_from(id)?;
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn set_async_value(&self, id: usize, _value: f64) -> ASCOMResult<()> {
        ensure_connected!(self);
        SwitchId::try_from(id)?;
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn cancel_async(&self, id: usize) -> ASCOMResult<()> {
        ensure_connected!(self);
        SwitchId::try_from(id)?;
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};

    /// Test-only no-op factory: never used at runtime (the
    /// connection-guard tests assert behaviour while disconnected, so
    /// `open` is never called).
    struct NoopFactory;

    #[async_trait]
    impl TransportFactory for NoopFactory {
        #[allow(clippy::unimplemented)]
        async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
            unimplemented!("test factory should never open a transport")
        }
    }

    fn disconnected_device() -> FalconStatusSwitchDevice {
        let manager = FalconManager::new(Arc::new(NoopFactory));
        FalconStatusSwitchDevice::new(SwitchConfig::default(), manager)
    }

    #[test]
    fn switch_id_try_from_accepts_zero_as_voltage() {
        assert!(matches!(SwitchId::try_from(0).unwrap(), SwitchId::Voltage));
    }

    #[test]
    fn switch_id_try_from_accepts_one_as_limit() {
        assert!(matches!(SwitchId::try_from(1).unwrap(), SwitchId::Limit));
    }

    #[test]
    fn switch_id_try_from_rejects_two() {
        let err = SwitchId::try_from(2).unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        // Operator-facing wording, pinned byte-for-byte: the upper bound is
        // the advertised MaxSwitch, so a drift in the switch count shows up
        // here rather than only in a client's log.
        assert_eq!(err.message, "Switch id 2 out of range (valid: 0..2)");
    }

    #[test]
    fn switch_id_try_from_rejects_large_id() {
        let err = SwitchId::try_from(usize::MAX).unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        // usize::MAX is width-dependent, so build the expected message rather
        // than hard-coding a 64-bit literal; the "0..2" bound is the invariant.
        assert_eq!(
            err.message,
            format!("Switch id {} out of range (valid: 0..2)", usize::MAX)
        );
    }

    #[tokio::test]
    async fn max_switch_reports_two_and_needs_no_connection() {
        // `MaxSwitch = 2` is the design doc's Switch layout contract, and
        // unlike every other Switch method this one has no connection guard.
        let device = disconnected_device();
        assert_eq!(device.max_switch().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn static_name_comes_from_config() {
        let device = disconnected_device();
        assert_eq!(device.static_name(), "Pegasus Falcon Status");
    }

    #[tokio::test]
    async fn unique_id_comes_from_config() {
        let manager = FalconManager::new(Arc::new(NoopFactory));
        let device = FalconStatusSwitchDevice::new(
            SwitchConfig {
                unique_id: "test-switch-unique-id".to_string(),
                ..SwitchConfig::default()
            },
            manager,
        );
        assert_eq!(device.unique_id(), "test-switch-unique-id");
    }

    #[tokio::test]
    async fn description_comes_from_config() {
        let device = disconnected_device();
        let desc = device.description().await.unwrap();
        assert!(desc.contains("voltage"), "got: {desc}");
        assert!(desc.contains("limit"), "got: {desc}");
    }

    #[tokio::test]
    async fn driver_info_mentions_alpaca() {
        let device = disconnected_device();
        let info = device.driver_info().await.unwrap();
        assert!(info.contains("Alpaca"), "got: {info}");
    }

    #[tokio::test]
    async fn driver_version_matches_cargo_pkg_version() {
        let device = disconnected_device();
        assert_eq!(
            device.driver_version().await.unwrap(),
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn debug_format_includes_struct_name() {
        let device = disconnected_device();
        let s = format!("{device:?}");
        assert!(s.contains("FalconStatusSwitchDevice"), "got: {s}");
    }

    #[tokio::test]
    async fn connected_reports_false_when_transport_unavailable() {
        let device = disconnected_device();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn can_write_returns_not_connected_when_disconnected() {
        let device = disconnected_device();
        let err = device.can_write(0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn state_change_complete_returns_not_connected_when_disconnected() {
        let device = disconnected_device();
        let err = device.state_change_complete(0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn set_switch_returns_not_connected_when_disconnected() {
        let device = disconnected_device();
        let err = device.set_switch(0, true).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn set_switch_value_returns_not_connected_when_disconnected() {
        let device = disconnected_device();
        let err = device.set_switch_value(0, 0.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn set_switch_name_returns_not_connected_when_disconnected() {
        let device = disconnected_device();
        let err = device
            .set_switch_name(0, "x".to_string())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
    }

    #[tokio::test]
    async fn connection_guard_precedes_id_validation() {
        // Disconnected + out-of-range id should report NOT_CONNECTED, not
        // INVALID_VALUE — the guard runs first per the design doc's error model.
        let device = disconnected_device();
        let err = device.set_switch_value(99, 0.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
    }
}

/// Connected-device tests for the seven Switch getters. Gated on
/// `feature = "mock"` so the rich
/// [`MockFalconTransportFactory`](crate::mock::MockFalconTransportFactory)
/// can stand in for the real Falcon.
#[cfg(all(test, feature = "mock"))]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unwrap_used)]
mod mock_tests {
    use super::*;
    use crate::mock::MockFalconTransportFactory;

    async fn connected_device() -> (FalconStatusSwitchDevice, Arc<MockFalconTransportFactory>) {
        let factory = Arc::new(MockFalconTransportFactory::default());
        let manager = FalconManager::new(Arc::<MockFalconTransportFactory>::clone(&factory));
        let device = FalconStatusSwitchDevice::new(SwitchConfig::default(), manager);
        device.set_connected(true).await.unwrap();
        (device, factory)
    }

    // ---- max_switch -----------------------------------------------------

    #[tokio::test]
    async fn max_switch_reports_two() {
        let (device, _) = connected_device().await;
        assert_eq!(device.max_switch().await.unwrap(), SWITCH_COUNT);
    }

    // ---- get_switch_name -----------------------------------------------

    #[tokio::test]
    async fn get_switch_name_id_0_is_input_voltage_raw() {
        let (device, _) = connected_device().await;
        let name = device.get_switch_name(SWITCH_ID_VOLTAGE).await.unwrap();
        assert_eq!(name, VOLTAGE_SWITCH_NAME);
    }

    #[tokio::test]
    async fn get_switch_name_id_1_is_limit_hit() {
        let (device, _) = connected_device().await;
        let name = device.get_switch_name(SWITCH_ID_LIMIT).await.unwrap();
        assert_eq!(name, LIMIT_SWITCH_NAME);
    }

    #[tokio::test]
    async fn get_switch_name_out_of_range_returns_invalid_value() {
        let (device, _) = connected_device().await;
        let err = device.get_switch_name(2).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    // ---- get_switch_description ----------------------------------------

    #[tokio::test]
    async fn get_switch_description_id_0_mentions_voltage() {
        let (device, _) = connected_device().await;
        let desc = device
            .get_switch_description(SWITCH_ID_VOLTAGE)
            .await
            .unwrap();
        assert!(
            desc.to_lowercase().contains("voltage"),
            "expected description to mention voltage, got: {desc}"
        );
    }

    #[tokio::test]
    async fn get_switch_description_id_1_mentions_limit() {
        let (device, _) = connected_device().await;
        let desc = device
            .get_switch_description(SWITCH_ID_LIMIT)
            .await
            .unwrap();
        assert!(
            desc.to_lowercase().contains("limit"),
            "expected description to mention limit, got: {desc}"
        );
    }

    // ---- min / max / step (pins the design-doc Switch layout table) ----

    #[tokio::test]
    async fn voltage_switch_range_is_zero_to_1023_step_1() {
        let (device, _) = connected_device().await;
        assert_eq!(
            device.min_switch_value(SWITCH_ID_VOLTAGE).await.unwrap(),
            0.0
        );
        assert_eq!(
            device.max_switch_value(SWITCH_ID_VOLTAGE).await.unwrap(),
            1023.0
        );
        assert_eq!(device.switch_step(SWITCH_ID_VOLTAGE).await.unwrap(), 1.0);
    }

    #[tokio::test]
    async fn limit_switch_range_is_zero_to_one_step_1() {
        let (device, _) = connected_device().await;
        assert_eq!(device.min_switch_value(SWITCH_ID_LIMIT).await.unwrap(), 0.0);
        assert_eq!(device.max_switch_value(SWITCH_ID_LIMIT).await.unwrap(), 1.0);
        assert_eq!(device.switch_step(SWITCH_ID_LIMIT).await.unwrap(), 1.0);
    }

    #[tokio::test]
    async fn metadata_getters_reject_out_of_range_id() {
        let (device, _) = connected_device().await;
        for err in [
            device.min_switch_value(2).await.unwrap_err(),
            device.max_switch_value(2).await.unwrap_err(),
            device.switch_step(2).await.unwrap_err(),
            device.get_switch_description(2).await.unwrap_err(),
        ] {
            assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        }
    }

    // ---- get_switch_value ----------------------------------------------

    #[tokio::test]
    async fn get_switch_value_id_0_returns_raw_voltage() {
        let (device, factory) = connected_device().await;
        factory.set_voltage_raw(812).await;
        let value = device.get_switch_value(SWITCH_ID_VOLTAGE).await.unwrap();
        assert_eq!(value, 812.0);
    }

    #[tokio::test]
    async fn get_switch_value_id_1_is_one_when_limit_detected() {
        let (device, factory) = connected_device().await;
        factory.set_limit_detect(true).await;
        let value = device.get_switch_value(SWITCH_ID_LIMIT).await.unwrap();
        assert_eq!(value, 1.0);
    }

    #[tokio::test]
    async fn get_switch_value_id_1_is_zero_when_limit_clear() {
        let (device, _) = connected_device().await;
        let value = device.get_switch_value(SWITCH_ID_LIMIT).await.unwrap();
        assert_eq!(value, 0.0);
    }

    #[tokio::test]
    async fn get_switch_value_issues_vs_for_id_0() {
        let (device, factory) = connected_device().await;
        factory.clear_command_log().await;
        let _ = device.get_switch_value(SWITCH_ID_VOLTAGE).await.unwrap();
        let log = factory.command_log().await;
        assert!(
            log.iter().any(|c| c == "VS"),
            "expected VS on the wire, got: {log:?}"
        );
    }

    #[tokio::test]
    async fn get_switch_value_issues_fa_for_id_1() {
        let (device, factory) = connected_device().await;
        factory.clear_command_log().await;
        let _ = device.get_switch_value(SWITCH_ID_LIMIT).await.unwrap();
        let log = factory.command_log().await;
        assert!(
            log.iter().any(|c| c == "FA"),
            "expected FA on the wire, got: {log:?}"
        );
    }

    // ---- get_switch (boolean projection of get_switch_value) -----------

    #[tokio::test]
    async fn get_switch_id_0_is_true_when_raw_above_zero() {
        let (device, factory) = connected_device().await;
        factory.set_voltage_raw(1).await;
        assert!(device.get_switch(SWITCH_ID_VOLTAGE).await.unwrap());
    }

    #[tokio::test]
    async fn get_switch_id_0_is_false_when_raw_is_zero() {
        let (device, factory) = connected_device().await;
        factory.set_voltage_raw(0).await;
        assert!(!device.get_switch(SWITCH_ID_VOLTAGE).await.unwrap());
    }

    #[tokio::test]
    async fn get_switch_id_1_is_true_when_limit_set() {
        let (device, factory) = connected_device().await;
        factory.set_limit_detect(true).await;
        assert!(device.get_switch(SWITCH_ID_LIMIT).await.unwrap());
    }

    #[tokio::test]
    async fn get_switch_id_1_is_false_when_limit_clear() {
        let (device, _) = connected_device().await;
        assert!(!device.get_switch(SWITCH_ID_LIMIT).await.unwrap());
    }

    // ---- write rejections (already enforced for disconnected; check
    // NOT_IMPLEMENTED fires when the device IS connected) ---------------

    #[tokio::test]
    async fn set_switch_returns_not_implemented_when_connected() {
        let (device, _) = connected_device().await;
        let err = device
            .set_switch(SWITCH_ID_VOLTAGE, true)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn set_switch_value_returns_not_implemented_when_connected() {
        let (device, _) = connected_device().await;
        let err = device
            .set_switch_value(SWITCH_ID_VOLTAGE, 0.0)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn set_switch_name_returns_not_implemented_when_connected() {
        let (device, _) = connected_device().await;
        let err = device
            .set_switch_name(SWITCH_ID_VOLTAGE, "x".to_string())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
    }

    // ---- ISwitchV3 async surface: id validation precedes the
    // not-implemented body. See the ConformU-driven override above. -------

    #[tokio::test]
    async fn can_async_returns_false_for_valid_id() {
        let (device, _) = connected_device().await;
        assert!(!device.can_async(SWITCH_ID_VOLTAGE).await.unwrap());
        assert!(!device.can_async(SWITCH_ID_LIMIT).await.unwrap());
    }

    #[tokio::test]
    async fn can_async_rejects_out_of_range_id() {
        let (device, _) = connected_device().await;
        let err = device.can_async(2).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[tokio::test]
    async fn set_async_returns_not_implemented_for_valid_id() {
        let (device, _) = connected_device().await;
        let err = device.set_async(SWITCH_ID_VOLTAGE, true).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn set_async_rejects_out_of_range_id() {
        let (device, _) = connected_device().await;
        let err = device.set_async(2, true).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[tokio::test]
    async fn set_async_value_returns_not_implemented_for_valid_id() {
        let (device, _) = connected_device().await;
        let err = device
            .set_async_value(SWITCH_ID_VOLTAGE, 0.0)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn set_async_value_rejects_out_of_range_id() {
        let (device, _) = connected_device().await;
        let err = device.set_async_value(2, 0.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[tokio::test]
    async fn cancel_async_returns_not_implemented_for_valid_id() {
        let (device, _) = connected_device().await;
        let err = device.cancel_async(SWITCH_ID_VOLTAGE).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn cancel_async_rejects_out_of_range_id() {
        let (device, _) = connected_device().await;
        let err = device.cancel_async(2).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }
}
