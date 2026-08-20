//! PPBA Switch device implementation.
//!
//! Implements the ASCOM Alpaca `Device` + `Switch` traits. Connection
//! state is the device's `Session<PpbaCodec>` slot — when it's `Some`,
//! we hold a live handle to the shared transport; when it's `None`,
//! we don't. The "requested" bool that previously diverged from the
//! transport's refcount is gone by construction.

use std::sync::Arc;

use ascom_alpaca::api::{Device, Switch};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use async_trait::async_trait;
use rusty_photon_shared_transport::Session;
use tokio::sync::RwLock;
use tracing::debug;

use crate::codec::PpbaCodec;
use crate::config::SwitchConfig;
use crate::config_actions::PpbaDriver;
use crate::error::{PpbaError, Result};
use crate::manager::PpbaManager;
use crate::protocol::PpbaCommand;
use crate::switches::{SwitchId, MAX_SWITCH};
use rusty_photon_driver::ConfigActionCtx;

/// Guard macro that returns `NOT_CONNECTED` if the device is not connected.
macro_rules! ensure_connected {
    ($self:ident) => {
        if !$self.connected().await.is_ok_and(|c| c) {
            debug!("Switch device not connected");
            return Err(ASCOMError::NOT_CONNECTED);
        }
    };
}

/// PPBA Switch device for ASCOM Alpaca.
#[derive(derive_more::Debug)]
pub struct PpbaSwitchDevice {
    config: SwitchConfig,
    /// `Some` between successful connect and explicit disconnect. The
    /// session existing is the truth — no second-source bool to desync.
    #[debug(skip)]
    session: Arc<RwLock<Option<Session<PpbaCodec>>>>,
    #[debug(skip)]
    manager: Arc<PpbaManager>,
    /// Shared (cloned) config-action context; `Some` on the normal path through
    /// `ServerBuilder`, `None` for focused unit-test devices.
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<PpbaDriver>>,
}

impl PpbaSwitchDevice {
    #[must_use]
    pub fn new(config: SwitchConfig, manager: Arc<PpbaManager>) -> Self {
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
    pub fn with_config_actions(mut self, ctx: ConfigActionCtx<PpbaDriver>) -> Self {
        self.config_ctx = Some(ctx);
        self
    }

    async fn get_switch_value_internal(&self, id: usize) -> Result<f64> {
        let switch_id = SwitchId::from_id(id).ok_or(PpbaError::InvalidSwitchId(id))?;
        let cached = self.manager.get_cached_state().await;

        match switch_id {
            SwitchId::Quad12V => {
                let status = cached.status.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(if status.switches.quad_12v { 1.0 } else { 0.0 })
            }
            SwitchId::AdjustableOutput => {
                let status = cached.status.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(if status.switches.adjustable_output {
                    1.0
                } else {
                    0.0
                })
            }
            SwitchId::DewHeaterA => {
                let status = cached.status.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(f64::from(status.dew_a))
            }
            SwitchId::DewHeaterB => {
                let status = cached.status.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(f64::from(status.dew_b))
            }
            SwitchId::UsbHub => Ok(if cached.usb_hub_enabled { 1.0 } else { 0.0 }),
            SwitchId::AutoDew => {
                let status = cached.status.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(if status.switches.auto_dew { 1.0 } else { 0.0 })
            }
            SwitchId::AverageCurrent => {
                let stats = cached.power_stats.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(stats.average_amps)
            }
            SwitchId::AmpHours => {
                let stats = cached.power_stats.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(stats.amp_hours)
            }
            SwitchId::WattHours => {
                let stats = cached.power_stats.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(stats.watt_hours)
            }
            SwitchId::Uptime => {
                let stats = cached.power_stats.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(stats.uptime_hours())
            }
            SwitchId::InputVoltage => {
                let status = cached.status.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(status.voltage)
            }
            SwitchId::TotalCurrent => {
                let status = cached.status.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(status.current)
            }
            SwitchId::Temperature => {
                let status = cached.status.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(status.temperature)
            }
            SwitchId::Humidity => {
                let status = cached.status.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(status.humidity)
            }
            SwitchId::Dewpoint => {
                let status = cached.status.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(status.dewpoint)
            }
            SwitchId::PowerWarning => {
                let status = cached.status.as_ref().ok_or(PpbaError::NotConnected)?;
                Ok(if status.power_warning { 1.0 } else { 0.0 })
            }
        }
    }

    async fn set_switch_value_internal(&self, id: usize, value: f64) -> Result<()> {
        let switch_id = SwitchId::from_id(id).ok_or(PpbaError::InvalidSwitchId(id))?;
        let info = switch_id.info();

        if !info.can_write {
            return Err(PpbaError::SwitchNotWritable(id));
        }

        let guard = self.session.read().await;
        let session = guard.as_ref().ok_or(PpbaError::NotConnected)?;

        // Dew heaters: re-check auto-dew off device, not cache, because the
        // user could have toggled it from a parallel control path between
        // polls. The PPBA reports auto-dew in PA, so refresh first.
        if matches!(switch_id, SwitchId::DewHeaterA | SwitchId::DewHeaterB) {
            self.manager.refresh_status(session).await?;
            let cached = self.manager.get_cached_state().await;
            if let Some(status) = &cached.status {
                if status.switches.auto_dew {
                    return Err(PpbaError::AutoDewEnabled(id));
                }
            }
        }

        // `!is_finite()` is checked explicitly: NaN compares false against
        // both bounds, so it would otherwise slip through and silently
        // actuate (bool switches read NaN >= 0.5 as off, PWM clamps to 0).
        if !value.is_finite() || value < info.min_value || value > info.max_value {
            return Err(PpbaError::InvalidValue(format!(
                "Value {} out of range [{}, {}] for switch {}",
                value, info.min_value, info.max_value, info.name
            )));
        }

        let command = match switch_id {
            SwitchId::Quad12V => PpbaCommand::SetQuad12V(value >= 0.5),
            SwitchId::AdjustableOutput => PpbaCommand::SetAdjustable(value >= 0.5),
            SwitchId::DewHeaterA => PpbaCommand::SetDewA(value.into()),
            SwitchId::DewHeaterB => PpbaCommand::SetDewB(value.into()),
            SwitchId::UsbHub => {
                let enabled = value >= 0.5;
                self.manager
                    .send_command(session, PpbaCommand::SetUsbHub(enabled))
                    .await?;
                drop(guard);
                self.manager.set_usb_hub_state(enabled).await;
                return Ok(());
            }
            SwitchId::AutoDew => PpbaCommand::SetAutoDew(value >= 0.5),
            _ => return Err(PpbaError::SwitchNotWritable(id)),
        };

        self.manager.send_command(session, command).await?;
        // Refresh status so the cached view reflects the new device state
        // (matches the legacy driver's post-set refresh).
        self.manager.refresh_status(session).await?;
        drop(guard);
        Ok(())
    }
}

#[async_trait]
impl Device for PpbaSwitchDevice {
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

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the write lock deliberately spans the whole check-and-modify so two concurrent connects cannot both observe an empty slot and double-acquire"
    )]
    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        // The write lock spans the whole check-and-modify so two concurrent
        // `Connected=true` requests can't both observe `None` and both
        // call `acquire()` (issue #251). With the session slot replacing
        // the old `requested` bool, the flag and the resource are the same
        // value — there is no second source to desync.
        let mut slot = self.session.write().await;
        match (connected, slot.is_some()) {
            (true, false) => {
                // `?` does SessionError → PpbaError via the manual
                // .map_err, then PpbaError → ASCOMError via the From
                // impl in error.rs.
                let session = self
                    .manager
                    .transport()
                    .acquire()
                    .await
                    .map_err(PpbaError::from)?;
                *slot = Some(session);
                debug!("Switch device connected");
            }
            (false, true) => {
                if let Some(session) = slot.take() {
                    // `Session::close` returns Result<_, TransportError>;
                    // `From<TransportError> for PpbaError` handles the
                    // conversion, and the existing `From<PpbaError> for
                    // ASCOMError` does the second hop on `?`.
                    session.close().await.map_err(PpbaError::from)?;
                }
                debug!("Switch device disconnected");
            }
            _ => {}
        }
        Ok(())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok(
            "PPBA Driver - Switch interface for Pegasus Astro Pocket Powerbox Advance Gen2"
                .to_string(),
        )
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }

    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(rusty_photon_driver::supported_actions(&self.config_ctx))
    }

    async fn action(&self, action: String, parameters: String) -> ASCOMResult<String> {
        rusty_photon_driver::dispatch::<PpbaDriver>(&self.config_ctx, action, parameters).await
    }
}

#[async_trait]
impl Switch for PpbaSwitchDevice {
    async fn max_switch(&self) -> ASCOMResult<usize> {
        Ok(MAX_SWITCH)
    }

    async fn can_write(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);

        let switch_id = SwitchId::from_id(id)
            .ok_or_else(|| ASCOMError::new(ASCOMErrorCode::INVALID_VALUE, "Invalid switch ID"))?;

        if matches!(switch_id, SwitchId::DewHeaterA | SwitchId::DewHeaterB) {
            // If the cache hasn't been populated yet, refresh under the
            // device's session.
            let cached = self.manager.get_cached_state().await;
            if let Some(status) = &cached.status {
                return Ok(!status.switches.auto_dew);
            }
            let guard = self.session.read().await;
            let session = guard
                .as_ref()
                .ok_or_else(|| ASCOMError::new(ASCOMErrorCode::NOT_CONNECTED, "not connected"))?;
            self.manager.refresh_status(session).await?;
            drop(guard);
            let cached = self.manager.get_cached_state().await;
            if let Some(status) = &cached.status {
                return Ok(!status.switches.auto_dew);
            }
        }

        Ok(switch_id.info().can_write)
    }

    async fn get_switch(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);

        let value = self.get_switch_value_internal(id).await?;

        let switch_id = SwitchId::from_id(id)
            .ok_or_else(|| ASCOMError::new(ASCOMErrorCode::INVALID_VALUE, "Invalid switch ID"))?;
        Ok(value > switch_id.info().min_value)
    }

    async fn set_switch(&self, id: usize, state: bool) -> ASCOMResult<()> {
        ensure_connected!(self);

        let switch_id = SwitchId::from_id(id)
            .ok_or_else(|| ASCOMError::new(ASCOMErrorCode::INVALID_VALUE, "Invalid switch ID"))?;
        let info = switch_id.info();

        let value = if state {
            info.max_value
        } else {
            info.min_value
        };

        self.set_switch_value_internal(id, value).await?;
        Ok(())
    }

    async fn get_switch_description(&self, id: usize) -> ASCOMResult<String> {
        ensure_connected!(self);
        let switch_id = SwitchId::from_id(id)
            .ok_or_else(|| ASCOMError::new(ASCOMErrorCode::INVALID_VALUE, "Invalid switch ID"))?;
        Ok(switch_id.info().description.to_string())
    }

    async fn get_switch_name(&self, id: usize) -> ASCOMResult<String> {
        ensure_connected!(self);
        let switch_id = SwitchId::from_id(id)
            .ok_or_else(|| ASCOMError::new(ASCOMErrorCode::INVALID_VALUE, "Invalid switch ID"))?;
        Ok(switch_id.info().name.to_string())
    }

    async fn set_switch_name(&self, _id: usize, _name: String) -> ASCOMResult<()> {
        Err(ASCOMError::new(
            ASCOMErrorCode::NOT_IMPLEMENTED,
            "Setting switch names is not supported",
        ))
    }

    async fn get_switch_value(&self, id: usize) -> ASCOMResult<f64> {
        ensure_connected!(self);

        Ok(self.get_switch_value_internal(id).await?)
    }

    async fn set_switch_value(&self, id: usize, value: f64) -> ASCOMResult<()> {
        ensure_connected!(self);

        self.set_switch_value_internal(id, value).await?;
        Ok(())
    }

    async fn min_switch_value(&self, id: usize) -> ASCOMResult<f64> {
        ensure_connected!(self);
        let switch_id = SwitchId::from_id(id)
            .ok_or_else(|| ASCOMError::new(ASCOMErrorCode::INVALID_VALUE, "Invalid switch ID"))?;
        Ok(switch_id.info().min_value)
    }

    async fn max_switch_value(&self, id: usize) -> ASCOMResult<f64> {
        ensure_connected!(self);
        let switch_id = SwitchId::from_id(id)
            .ok_or_else(|| ASCOMError::new(ASCOMErrorCode::INVALID_VALUE, "Invalid switch ID"))?;
        Ok(switch_id.info().max_value)
    }

    async fn switch_step(&self, id: usize) -> ASCOMResult<f64> {
        ensure_connected!(self);
        let switch_id = SwitchId::from_id(id)
            .ok_or_else(|| ASCOMError::new(ASCOMErrorCode::INVALID_VALUE, "Invalid switch ID"))?;
        Ok(switch_id.info().step)
    }

    async fn can_async(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);
        if id >= MAX_SWITCH {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Invalid switch ID: {id}"),
            ));
        }
        Ok(false)
    }

    async fn state_change_complete(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);
        if id >= MAX_SWITCH {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Invalid switch ID: {id}"),
            ));
        }
        Ok(true)
    }

    async fn cancel_async(&self, id: usize) -> ASCOMResult<()> {
        ensure_connected!(self);
        if id >= MAX_SWITCH {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Invalid switch ID: {id}"),
            ));
        }
        Ok(())
    }

    async fn set_async(&self, id: usize, state: bool) -> ASCOMResult<()> {
        ensure_connected!(self);
        if id >= MAX_SWITCH {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Invalid switch ID: {id}"),
            ));
        }
        self.set_switch(id, state).await
    }

    async fn set_async_value(&self, id: usize, value: f64) -> ASCOMResult<()> {
        ensure_connected!(self);
        if id >= MAX_SWITCH {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Invalid switch ID: {id}"),
            ));
        }
        self.set_switch_value(id, value).await
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    //! Unit tests cover ASCOM error mapping, switch metadata, and
    //! happy-path connect/read/write/disconnect. Race / refcount /
    //! rollback invariants are tested once in
    //! `rusty-photon-shared-transport`'s `tests/race.rs` and
    //! `tests/rollback.rs`; not duplicated per-service.

    use super::*;
    use crate::config::Config;
    use crate::mock::MockPpbaTransportFactory;
    use ascom_alpaca::ASCOMErrorCode;
    use async_trait::async_trait;
    use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};

    /// Factory whose `open()` always fails. Used to exercise the
    /// `set_connected(true)` acquire-failure mapping into ASCOM errors —
    /// the BDD suite can't reach this path because its mock always
    /// succeeds.
    struct FailingPpbaTransportFactory;

    #[async_trait]
    impl TransportFactory for FailingPpbaTransportFactory {
        async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
            Err(TransportError::Open(std::io::Error::other(
                "mock factory error",
            )))
        }
    }

    fn make_device() -> PpbaSwitchDevice {
        let factory = Arc::new(MockPpbaTransportFactory::default());
        let config = Config::default();
        let manager = PpbaManager::new(&config, factory);
        PpbaSwitchDevice::new(config.switch, manager)
    }

    fn make_device_with_failing_factory() -> PpbaSwitchDevice {
        let factory = Arc::new(FailingPpbaTransportFactory);
        let config = Config::default();
        let manager = PpbaManager::new(&config, factory);
        PpbaSwitchDevice::new(config.switch, manager)
    }

    async fn connected_device() -> PpbaSwitchDevice {
        let device = make_device();
        device.set_connected(true).await.unwrap();
        device
    }

    #[tokio::test]
    async fn starts_disconnected() {
        let device = make_device();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn connect_then_disconnect_round_trip() {
        let device = make_device();
        device.set_connected(true).await.unwrap();
        assert!(device.connected().await.unwrap());
        device.set_connected(false).await.unwrap();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn set_connected_is_idempotent() {
        let device = make_device();
        device.set_connected(true).await.unwrap();
        device.set_connected(true).await.unwrap();
        assert!(device.connected().await.unwrap());
        device.set_connected(false).await.unwrap();
        device.set_connected(false).await.unwrap();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn operations_fail_when_not_connected() {
        let device = make_device();
        assert_eq!(
            device.get_switch(0).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.set_switch(0, true).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.set_switch_value(0, 1.0).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.can_write(0).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
    }

    #[tokio::test]
    async fn get_switch_value_invalid_id_maps_to_invalid_value() {
        let device = connected_device().await;
        let err = device.get_switch_value(99).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_switch_value_read_only_maps_to_not_implemented() {
        let device = connected_device().await;
        let err = device.set_switch_value(10, 5.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_switch_value_out_of_range_maps_to_invalid_value() {
        let device = connected_device().await;
        let err = device.set_switch_value(0, 5.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_switch_value_nan_maps_to_invalid_value() {
        // NaN compares false against both range bounds, so without the
        // explicit is_finite() check it would silently actuate.
        let device = connected_device().await;
        let err = device.set_switch_value(0, f64::NAN).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_switch_value_auto_dew_enabled_blocks_dew_heater_write() {
        let device = connected_device().await;
        // Turn auto-dew ON via the switch path.
        device.set_switch_value(5, 1.0).await.unwrap();
        // Now writing the dew heater must fail with INVALID_OPERATION.
        let err = device.set_switch_value(2, 128.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert!(err.message.contains("auto-dew"));
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn read_controllable_switches() {
        let device = connected_device().await;
        // Default mock state: quad=true, adj=false, dewA=128, dewB=64, autodew=false
        assert!((device.get_switch_value(0).await.unwrap() - 1.0).abs() < f64::EPSILON);
        assert!((device.get_switch_value(1).await.unwrap() - 0.0).abs() < f64::EPSILON);
        assert!((device.get_switch_value(2).await.unwrap() - 128.0).abs() < f64::EPSILON);
        assert!((device.get_switch_value(3).await.unwrap() - 64.0).abs() < f64::EPSILON);
        assert!((device.get_switch_value(4).await.unwrap() - 0.0).abs() < f64::EPSILON);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_controllable_switch_mutates_device_state() {
        let device = connected_device().await;
        device.set_switch_value(0, 0.0).await.unwrap();
        assert!((device.get_switch_value(0).await.unwrap() - 0.0).abs() < f64::EPSILON);
        device.set_switch_value(1, 1.0).await.unwrap();
        assert!((device.get_switch_value(1).await.unwrap() - 1.0).abs() < f64::EPSILON);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn metadata_when_connected() {
        let device = connected_device().await;
        assert!(device.can_write(0).await.unwrap());
        assert!(!device.can_write(10).await.unwrap());
        let name = device.get_switch_name(0).await.unwrap();
        assert!(!name.is_empty());
        let (min, max, step) = (
            device.min_switch_value(0).await.unwrap(),
            device.max_switch_value(0).await.unwrap(),
            device.switch_step(0).await.unwrap(),
        );
        assert!((min - 0.0).abs() < f64::EPSILON);
        assert!((max - 1.0).abs() < f64::EPSILON);
        assert!((step - 1.0).abs() < f64::EPSILON);
        assert!(!device.can_async(0).await.unwrap());
        assert!(device.state_change_complete(0).await.unwrap());
        device.cancel_async(0).await.unwrap();
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn max_switch_returns_constant() {
        let device = make_device();
        assert_eq!(device.max_switch().await.unwrap(), MAX_SWITCH);
    }

    #[tokio::test]
    async fn set_switch_name_returns_not_implemented() {
        let device = make_device();
        let err = device
            .set_switch_name(0, "x".to_string())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn set_connected_acquire_failure_maps_to_invalid_operation() {
        // The factory's open() returns TransportError::Open, which
        // propagates as PpbaError::ConnectionFailed and falls to
        // PpbaError::to_ascom_error's catch-all -> INVALID_OPERATION.
        let device = make_device_with_failing_factory();
        let err = device.set_connected(true).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert!(
            err.message.contains("mock factory error"),
            "expected message to carry the underlying io error, got: {}",
            err.message
        );
        // No session got stored on failure — the device stays disconnected.
        assert!(!device.connected().await.unwrap());
    }

    // PpbaError → ASCOMError mapping tests moved to error.rs once the
    // canonical mapping landed there (centralised so both devices share
    // the same classification).
}
