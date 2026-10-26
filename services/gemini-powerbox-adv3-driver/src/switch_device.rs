//! The Advanced 3's Switch device.
//!
//! Implements the ASCOM Alpaca `Device` + `Switch` traits over the 26-switch
//! table in `docs/services/gemini-powerbox-adv3-driver.md`. Connection state
//! is the device's `Session<GeminiCodec>` slot — when it's `Some`, we hold a
//! live handle to the shared transport; when it's `None`, we don't.
//!
//! * **Reads never touch the wire.** Every value comes from the frame the
//!   read-only handshake seeds and the poll loop refreshes; a cache miss is
//!   `NOT_CONNECTED`.
//! * **Every write is confirmed against a fresh frame.** A set draws no
//!   reply, so the codec sends it with a `>G#` and the field the write
//!   addressed is compared with what was asked. A mismatch is asked once
//!   more before it is believed — a frame the hub started during the few
//!   milliseconds the two commands take to arrive predates the set — and a
//!   mismatch that survives is `INVALID_OPERATION`, naming the switch and
//!   both values.
//! * **The dew rows move their writability with the mode.** A channel's
//!   duty is writable in Manual only, its enabled flag in Auto and Switch
//!   only; the mode row is what changes that, and it refuses Auto while a
//!   sensor is absent.

use std::sync::Arc;

use ascom_alpaca::api::{Device, Switch};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult, SetupRequest, SetupResponse};
use async_trait::async_trait;
use rusty_photon_setup::SetupPages;
use rusty_photon_shared_transport::Session;
use tokio::sync::RwLock;
use tracing::debug;

use crate::codec::GeminiCodec;
use crate::config::SwitchConfig;
use crate::error::{GeminiError, Result};
use crate::manager::GeminiManager;
use crate::protocol::{DewChannel, DewDuty, DewMode, FirmwareVersion, SetCommand, StatusFrame};
use crate::setup::{GeminiDriver, SWITCH_DEVICE_TYPE};
use crate::switches::{SwitchId, HUMIDITY_SENTINEL, MAX_SWITCH, TEMPERATURE_SENTINEL};

/// Guard macro that returns `NOT_CONNECTED` if the device is not connected.
macro_rules! ensure_connected {
    ($self:ident) => {
        if !$self.connected().await.is_ok_and(|c| c) {
            debug!("Switch device not connected");
            return Err(ASCOMError::NOT_CONNECTED);
        }
    };
}

/// Resolve a switch id, mapping an unknown one to `INVALID_VALUE`.
macro_rules! switch_id_or_invalid {
    ($id:expr) => {
        SwitchId::from_id($id).ok_or_else(|| {
            ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Invalid switch ID: {}", $id),
            )
        })?
    };
}

/// ASCOM switch values are `f64`; boolean device state is 1.0 / 0.0.
const fn bool_value(state: bool) -> f64 {
    if state {
        1.0
    } else {
        0.0
    }
}

/// The sensor a channel needs for Auto mode and does not have, if any.
const fn missing_sensor(frame: &StatusFrame) -> Option<&'static str> {
    if !frame.ambient_sensor_attached {
        Some("AHT20 ambient temperature and humidity sensor")
    } else if !frame.lens_probe_attached {
        Some("DS18B20 lens temperature probe")
    } else {
        None
    }
}

/// The Advanced 3 Switch device for ASCOM Alpaca.
#[derive(derive_more::Debug)]
pub struct GeminiSwitchDevice {
    config: SwitchConfig,
    /// `Some` between successful connect and explicit disconnect. The
    /// session existing is the truth — no second-source bool to desync.
    #[debug(skip)]
    session: Arc<RwLock<Option<Session<GeminiCodec>>>>,
    #[debug(skip)]
    manager: Arc<GeminiManager>,
    /// The setup pages behind `/setup/v1/switch/0/setup`: `Some` when the
    /// driver was built with a config source (the normal path through
    /// `ServerBuilder`); `None` for focused unit-test devices.
    #[debug(skip)]
    pages: Option<Arc<SetupPages<GeminiDriver>>>,
}

impl GeminiSwitchDevice {
    #[must_use]
    pub fn new(config: SwitchConfig, manager: Arc<GeminiManager>) -> Self {
        Self {
            config,
            session: Arc::new(RwLock::new(None)),
            manager,
            pages: None,
        }
    }

    /// Attach the shared setup pages, so the device's setup page renders and
    /// saves the `switch` section.
    #[must_use]
    pub fn with_setup_pages(mut self, pages: Arc<SetupPages<GeminiDriver>>) -> Self {
        self.pages = Some(pages);
        self
    }

    /// The last frame, or `NotConnected` while no poll has landed.
    async fn frame(&self) -> Result<StatusFrame> {
        self.manager
            .get_cached_state()
            .await
            .status
            .ok_or(GeminiError::NotConnected)
    }

    /// Serve one switch value from the cache.
    ///
    /// Nothing here reaches the wire. A reading whose sensor the frame says
    /// is absent is the row's sentinel — a value no working sensor produces
    /// in air, inside the row's declared range — and switches 24 and 25 say
    /// which it is.
    async fn get_switch_value_internal(&self, id: usize) -> Result<f64> {
        let switch_id = SwitchId::from_id(id).ok_or(GeminiError::InvalidSwitchId(id))?;
        let frame = self.frame().await?;
        Ok(match switch_id {
            SwitchId::Dc1 => 1.0,
            SwitchId::Dc(output) => bool_value(frame.dc_on(output)),
            SwitchId::Usb(port) => bool_value(frame.usb_on(port)),
            SwitchId::DewDuty(channel) => f64::from(frame.dew(channel).duty),
            SwitchId::DewEnabled(channel) => bool_value(frame.dew(channel).enabled),
            SwitchId::DewMode(channel) => f64::from(frame.dew(channel).mode.digit()),
            SwitchId::InputVoltage => frame.input_voltage,
            SwitchId::OutputCurrent => frame.output_current,
            SwitchId::OutputPower => frame.output_power,
            SwitchId::Temperature if frame.ambient_sensor_attached => frame.ambient_temperature,
            SwitchId::Humidity if frame.ambient_sensor_attached => frame.humidity,
            SwitchId::Dewpoint if frame.ambient_sensor_attached => frame.dew_point,
            SwitchId::LensTemperature if frame.lens_probe_attached => frame.lens_temperature,
            SwitchId::Temperature | SwitchId::Dewpoint | SwitchId::LensTemperature => {
                TEMPERATURE_SENTINEL
            }
            SwitchId::Humidity => HUMIDITY_SENTINEL,
            SwitchId::AmbientSensorAttached => bool_value(frame.ambient_sensor_attached),
            SwitchId::LensProbeAttached => bool_value(frame.lens_probe_attached),
        })
    }

    /// The channel's mode, from the cache, refreshed once if the cache is
    /// still empty.
    async fn dew_mode(
        &self,
        session: &Session<GeminiCodec>,
        channel: DewChannel,
    ) -> Result<DewMode> {
        let frame = match self.manager.get_cached_state().await.status {
            Some(frame) => frame,
            None => self.manager.refresh_status(session).await?,
        };
        Ok(frame.dew(channel).mode)
    }

    /// The refusal a dew row gives while the channel's mode makes it
    /// read-only: `NOT_IMPLEMENTED`, naming the mode switch to change.
    fn mode_gate(&self, switch_id: SwitchId, channel: DewChannel, mode: DewMode) -> GeminiError {
        GeminiError::DewModeGate {
            switch_id: switch_id.id(),
            name: switch_id.effective_name(&self.config.labels),
            channel: channel.name(),
            mode: mode.label(),
            mode_switch: SwitchId::DewMode(channel).effective_name(&self.config.labels),
        }
    }

    /// Translate a validated write into the command for it, or the reason
    /// nothing is sent.
    async fn command_for(
        &self,
        session: &Session<GeminiCodec>,
        switch_id: SwitchId,
        value: f64,
    ) -> Result<SetCommand> {
        // The boolean rows are validated to [0, 1] by the caller, so `> 0.0`
        // is "on" without an equality comparison between floats.
        Ok(match switch_id {
            SwitchId::Dc(output) => SetCommand::Dc(output, value > 0.0),
            SwitchId::Usb(port) => SetCommand::Usb(port, value > 0.0),
            SwitchId::DewDuty(channel) => {
                let mode = self.dew_mode(session, channel).await?;
                if mode != DewMode::Manual {
                    return Err(self.mode_gate(switch_id, channel, mode));
                }
                SetCommand::DewDuty(channel, DewDuty::from_switch_value(value)?)
            }
            SwitchId::DewEnabled(channel) => {
                let mode = self.dew_mode(session, channel).await?;
                if mode == DewMode::Manual {
                    return Err(self.mode_gate(switch_id, channel, mode));
                }
                SetCommand::DewEnabled(channel, value > 0.0)
            }
            SwitchId::DewMode(channel) => {
                let mode = DewMode::from_switch_value(value)?;
                if mode == DewMode::Auto {
                    if let Some(sensor) = missing_sensor(&self.frame().await?) {
                        return Err(GeminiError::AutoNeedsSensor {
                            channel: channel.name(),
                            sensor,
                        });
                    }
                }
                SetCommand::DewMode(channel, mode)
            }
            _ => return Err(GeminiError::SwitchNotWritable(switch_id.id())),
        })
    }

    async fn set_switch_value_internal(&self, id: usize, value: f64) -> Result<()> {
        let switch_id = SwitchId::from_id(id).ok_or(GeminiError::InvalidSwitchId(id))?;
        let info = switch_id.info();

        if !info.can_write {
            return Err(GeminiError::SwitchNotWritable(id));
        }

        // `!is_finite()` is checked explicitly: NaN compares false against
        // both bounds, so it would otherwise slip through and actuate. A
        // value between steps is taken and rounded to the nearest step
        // (every writable row steps by 1): ConformU writes the quarter
        // points of every range and expects them accepted.
        if !value.is_finite() || value < info.min_value || value > info.max_value {
            return Err(GeminiError::InvalidValue(format!(
                "Value {} out of range [{}, {}] for switch {}",
                value, info.min_value, info.max_value, info.name
            )));
        }
        let value = value.round();

        let guard = self.session.read().await;
        let session = guard.as_ref().ok_or(GeminiError::NotConnected)?;

        let set = self.command_for(session, switch_id, value).await?;

        debug!(id, value, command = %set.wire(), "sending switch write");
        let frame = self.manager.send_set(session, set).await?;
        if frame.confirms(&set) {
            self.manager.store_status(&frame).await;
            drop(guard);
            return Ok(());
        }

        // A periodic frame the hub started before the set arrived predates
        // it and is read first; one more look tells that from a refusal.
        debug!(
            id,
            reported = frame.reported(&set),
            "readback does not show the write; asking once more"
        );
        let frame = self.manager.refresh_status(session).await?;
        drop(guard);
        if frame.confirms(&set) {
            return Ok(());
        }
        Err(GeminiError::WriteNotConfirmed {
            name: switch_id.effective_name(&self.config.labels),
            asked: value,
            reported: frame.reported(&set),
        })
    }

    /// Whether a dew row is writable in the channel's current mode. Served
    /// from the cached frame, refreshing once under the device's session if
    /// the cache has not been populated yet.
    async fn dew_row_writable(&self, switch_id: SwitchId) -> ASCOMResult<bool> {
        let (channel, writable_in): (DewChannel, fn(DewMode) -> bool) = match switch_id {
            SwitchId::DewDuty(channel) => (channel, |mode| mode == DewMode::Manual),
            SwitchId::DewEnabled(channel) => (channel, |mode| mode != DewMode::Manual),
            other => return Ok(other.info().can_write),
        };
        let guard = self.session.read().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| ASCOMError::new(ASCOMErrorCode::NOT_CONNECTED, "not connected"))?;
        let mode = self.dew_mode(session, channel).await?;
        drop(guard);
        Ok(writable_in(mode))
    }
}

#[async_trait]
impl Device for GeminiSwitchDevice {
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
        // Connecting actuates nothing: the handshake behind `acquire()` is
        // `>H#` / `>V#` / `>G#`, every one of them a read (tenet 3).
        let mut slot = self.session.write().await;
        match (connected, slot.is_some()) {
            (true, false) => {
                let session = self
                    .manager
                    .transport()
                    .acquire()
                    .await
                    .map_err(GeminiError::from)?;
                *slot = Some(session);
                debug!("Switch device connected");
            }
            (false, true) => {
                if let Some(session) = slot.take() {
                    session.close().await.map_err(GeminiError::from)?;
                }
                debug!("Switch device disconnected");
            }
            _ => {}
        }
        Ok(())
    }

    /// Names the driver and, once the handshake has read it, the hub's
    /// firmware.
    async fn driver_info(&self) -> ASCOMResult<String> {
        let firmware = self
            .manager
            .get_cached_state()
            .await
            .firmware
            .map_or_else(String::new, |v| {
                format!(" (firmware {})", FirmwareVersion(v))
            });
        Ok(format!(
            "Gemini Power & Data Hubs Advanced 3 Driver - Switch interface{firmware}"
        ))
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }

    /// No actions: the configuration lives on the setup pages.
    async fn action(&self, action: String, _parameters: String) -> ASCOMResult<String> {
        Err(ASCOMError::new(
            ASCOMErrorCode::ACTION_NOT_IMPLEMENTED,
            format!("this device has no action {action:?}"),
        ))
    }

    /// The device's setup page, `/setup/v1/switch/0/setup`, for `GET` and
    /// `POST`, through the shared handler.
    fn setup_request(
        &self,
        request: SetupRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SetupResponse> + Send + '_>> {
        match &self.pages {
            Some(pages) => {
                rusty_photon_driver::device_setup_request(pages, SWITCH_DEVICE_TYPE, 0, request)
            }
            None => Box::pin(async {
                SetupResponse::text(
                    axum::http::StatusCode::NOT_FOUND,
                    "This device was built without its setup pages",
                )
            }),
        }
    }
}

#[async_trait]
impl Switch for GeminiSwitchDevice {
    async fn max_switch(&self) -> ASCOMResult<usize> {
        Ok(MAX_SWITCH)
    }

    async fn can_write(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);
        let switch_id = switch_id_or_invalid!(id);
        self.dew_row_writable(switch_id).await
    }

    async fn get_switch(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);
        let value = self.get_switch_value_internal(id).await?;
        let switch_id = switch_id_or_invalid!(id);
        Ok(value > switch_id.info().min_value)
    }

    async fn set_switch(&self, id: usize, state: bool) -> ASCOMResult<()> {
        ensure_connected!(self);
        let switch_id = switch_id_or_invalid!(id);
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
        let switch_id = switch_id_or_invalid!(id);
        Ok(switch_id.info().description.to_string())
    }

    /// The operator's label for this switch's connector where the config
    /// carries one, otherwise the built-in name. `GetSwitchDescription` is
    /// left alone: it names the physical connector.
    async fn get_switch_name(&self, id: usize) -> ASCOMResult<String> {
        ensure_connected!(self);
        let switch_id = switch_id_or_invalid!(id);
        Ok(switch_id.effective_name(&self.config.labels))
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
        let switch_id = switch_id_or_invalid!(id);
        Ok(switch_id.info().min_value)
    }

    async fn max_switch_value(&self, id: usize) -> ASCOMResult<f64> {
        ensure_connected!(self);
        let switch_id = switch_id_or_invalid!(id);
        Ok(switch_id.info().max_value)
    }

    async fn switch_step(&self, id: usize) -> ASCOMResult<f64> {
        ensure_connected!(self);
        let switch_id = switch_id_or_invalid!(id);
        Ok(switch_id.info().step)
    }

    // ISwitchV3's async surface. Every switch is synchronous (`CanAsync`
    // false), and the spec ties the other members to that: `SetAsync`,
    // `SetAsyncValue` and `StateChangeComplete` must raise
    // `MethodNotImplemented`, while `CancelAsync` is mandatory and must not.
    // Each checks the connection and the id first, so a bad id still answers
    // `INVALID_VALUE`, which ConformU requires of every member.

    async fn can_async(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);
        switch_id_or_invalid!(id);
        Ok(false)
    }

    async fn state_change_complete(&self, id: usize) -> ASCOMResult<bool> {
        ensure_connected!(self);
        switch_id_or_invalid!(id);
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn cancel_async(&self, id: usize) -> ASCOMResult<()> {
        ensure_connected!(self);
        switch_id_or_invalid!(id);
        Ok(())
    }

    async fn set_async(&self, id: usize, _state: bool) -> ASCOMResult<()> {
        ensure_connected!(self);
        switch_id_or_invalid!(id);
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn set_async_value(&self, id: usize, _value: f64) -> ASCOMResult<()> {
        ensure_connected!(self);
        switch_id_or_invalid!(id);
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    //! Unit tests cover the read mapping, the sentinels, the dew-mode gates,
    //! write validation and confirmation, and connect/read/write/disconnect.
    //! Race / refcount / rollback invariants are tested once in
    //! `rusty-photon-shared-transport`; they are not duplicated per service.

    use super::*;
    use crate::config::Config;
    use crate::mock::MockGeminiTransportFactory;
    use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};

    // Ids, named once.
    const DC1: usize = 0;
    const DC2: usize = 1;
    const DC3: usize = 2;
    const USB_A: usize = 5;
    const USB_F: usize = 10;
    const DEW6_DUTY: usize = 11;
    const DEW7_DUTY: usize = 12;
    const DEW6_ENABLED: usize = 13;
    const DEW7_ENABLED: usize = 14;
    const DEW6_MODE: usize = 15;
    const DEW7_MODE: usize = 16;
    const INPUT_VOLTAGE: usize = 17;
    const TEMPERATURE: usize = 20;
    const HUMIDITY: usize = 21;
    const DEWPOINT: usize = 22;
    const LENS_TEMPERATURE: usize = 23;
    const AMBIENT_ATTACHED: usize = 24;
    const PROBE_ATTACHED: usize = 25;

    /// Factory whose `open()` always fails, to exercise the
    /// `set_connected(true)` acquire-failure mapping into ASCOM errors.
    struct FailingFactory;

    #[async_trait]
    impl TransportFactory for FailingFactory {
        async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
            Err(TransportError::Open(std::io::Error::other(
                "mock factory error",
            )))
        }
    }

    fn make_device_with(factory: Arc<dyn TransportFactory>) -> GeminiSwitchDevice {
        let config = Config::default();
        let manager = GeminiManager::new(&config, factory);
        GeminiSwitchDevice::new(config.switch, manager)
    }

    fn make_device() -> GeminiSwitchDevice {
        make_device_with(Arc::new(MockGeminiTransportFactory::default()))
    }

    /// The mock as the trait object the manager takes.
    fn dyn_factory(factory: &Arc<MockGeminiTransportFactory>) -> Arc<dyn TransportFactory> {
        let concrete: Arc<MockGeminiTransportFactory> = Arc::clone(factory);
        concrete
    }

    async fn connected_device() -> GeminiSwitchDevice {
        let device = make_device();
        device.set_connected(true).await.unwrap();
        device
    }

    async fn connected_device_on(
        factory: MockGeminiTransportFactory,
    ) -> (GeminiSwitchDevice, Arc<MockGeminiTransportFactory>) {
        let factory = Arc::new(factory);
        let device = make_device_with(dyn_factory(&factory));
        device.set_connected(true).await.unwrap();
        (device, factory)
    }

    #[track_caller]
    fn assert_value(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    // ------------------------------------------------------------------
    // Connection lifecycle
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn starts_disconnected_and_round_trips_idempotently() {
        let device = make_device();
        assert!(!device.connected().await.unwrap());
        device.set_connected(true).await.unwrap();
        device.set_connected(true).await.unwrap();
        assert!(device.connected().await.unwrap());
        device.set_connected(false).await.unwrap();
        device.set_connected(false).await.unwrap();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn connect_sends_only_queries_and_leaves_every_output_as_found() {
        // Tenet 3: the handshake is read-only, so the outputs the hub was
        // found with are the outputs after connect, and the wire log shows
        // nothing but the three queries.
        let (device, factory) = connected_device_on(MockGeminiTransportFactory::default()).await;
        for id in DC2..=USB_F {
            assert_value(device.get_switch_value(id).await.unwrap(), 1.0);
        }
        assert_eq!(factory.sent_commands().await, [">H#", ">V#", ">G#"]);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn operations_fail_when_not_connected() {
        let device = make_device();
        assert_eq!(
            device.get_switch(DC2).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.set_switch_value(DC2, 1.0).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.can_write(DEW6_DUTY).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.get_switch_name(DC2).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        // The id is checked after the connection: NOT_CONNECTED first.
        assert_eq!(
            device.can_async(MAX_SWITCH).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
    }

    #[tokio::test]
    async fn set_connected_acquire_failure_maps_to_invalid_operation() {
        let device = make_device_with(Arc::new(FailingFactory));
        let err = device.set_connected(true).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert!(
            err.message.contains("mock factory error"),
            "{}",
            err.message
        );
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn a_refused_handshake_reaches_the_client_with_its_reason() {
        let device = make_device_with(Arc::new(
            MockGeminiTransportFactory::default().with_firmware("307"),
        ));
        let err = device.set_connected(true).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert!(err.message.contains("3.0.7"), "{}", err.message);
        assert!(
            err.message.contains("FirmwareUpgradeTool"),
            "{}",
            err.message
        );
    }

    // ------------------------------------------------------------------
    // Read paths
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn every_switch_id_reads_a_value_and_has_a_unique_name() {
        let device = connected_device().await;
        let mut names = Vec::with_capacity(MAX_SWITCH);
        for id in 0..MAX_SWITCH {
            device.get_switch_value(id).await.unwrap();
            let name = device.get_switch_name(id).await.unwrap();
            assert!(!name.is_empty(), "switch {id} has no name");
            assert!(
                !device.get_switch_description(id).await.unwrap().is_empty(),
                "switch {id} has no description"
            );
            names.push(name);
        }
        names.sort_unstable();
        let unique = names.len();
        names.dedup();
        assert_eq!(names.len(), unique, "switch names must be unique");
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn reads_map_to_the_captured_frames_fields() {
        let device = connected_device().await;
        assert_value(device.get_switch_value(DC1).await.unwrap(), 1.0);
        assert_value(device.get_switch_value(DEW6_DUTY).await.unwrap(), 100.0);
        assert_value(device.get_switch_value(DEW7_DUTY).await.unwrap(), 100.0);
        assert_value(device.get_switch_value(DEW6_ENABLED).await.unwrap(), 1.0);
        assert_value(device.get_switch_value(DEW6_MODE).await.unwrap(), 1.0);
        assert_value(device.get_switch_value(DEW7_MODE).await.unwrap(), 1.0);
        assert_value(device.get_switch_value(INPUT_VOLTAGE).await.unwrap(), 12.2);
        assert_value(device.get_switch_value(18).await.unwrap(), 0.02);
        assert_value(device.get_switch_value(19).await.unwrap(), 0.36);
        assert_value(device.get_switch_value(TEMPERATURE).await.unwrap(), 24.47);
        assert_value(device.get_switch_value(HUMIDITY).await.unwrap(), 67.76);
        assert_value(device.get_switch_value(DEWPOINT).await.unwrap(), 18.12);
        assert_value(
            device.get_switch_value(LENS_TEMPERATURE).await.unwrap(),
            23.25,
        );
        assert_value(
            device.get_switch_value(AMBIENT_ATTACHED).await.unwrap(),
            1.0,
        );
        assert_value(device.get_switch_value(PROBE_ATTACHED).await.unwrap(), 1.0);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn an_absent_ambient_sensor_reads_as_the_sentinels_and_its_flag() {
        let (device, _) =
            connected_device_on(MockGeminiTransportFactory::default().with_sensors(false, true))
                .await;
        assert_value(device.get_switch_value(TEMPERATURE).await.unwrap(), -127.0);
        assert_value(device.get_switch_value(HUMIDITY).await.unwrap(), 0.0);
        assert_value(device.get_switch_value(DEWPOINT).await.unwrap(), -127.0);
        assert_value(
            device.get_switch_value(AMBIENT_ATTACHED).await.unwrap(),
            0.0,
        );
        // The probe's row is untouched by the AHT20's absence.
        assert_value(
            device.get_switch_value(LENS_TEMPERATURE).await.unwrap(),
            23.25,
        );
        assert_value(device.get_switch_value(PROBE_ATTACHED).await.unwrap(), 1.0);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn an_absent_lens_probe_reads_as_the_sentinel_and_its_flag() {
        let (device, _) =
            connected_device_on(MockGeminiTransportFactory::default().with_sensors(true, false))
                .await;
        assert_value(
            device.get_switch_value(LENS_TEMPERATURE).await.unwrap(),
            -127.0,
        );
        assert_value(device.get_switch_value(PROBE_ATTACHED).await.unwrap(), 0.0);
        assert_value(device.get_switch_value(TEMPERATURE).await.unwrap(), 24.47);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn a_reading_outside_its_row_is_published_unclamped() {
        let (device, _) =
            connected_device_on(MockGeminiTransportFactory::default().with_input_voltage(24.0))
                .await;
        assert_value(device.get_switch_value(INPUT_VOLTAGE).await.unwrap(), 24.0);
        assert_value(device.max_switch_value(INPUT_VOLTAGE).await.unwrap(), 19.0);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn boolean_view_thresholds_at_min_value() {
        let device = connected_device().await;
        assert!(device.get_switch(DC2).await.unwrap());
        assert!(device.get_switch(DEW6_DUTY).await.unwrap());
        assert!(
            device.get_switch(DEW6_MODE).await.unwrap(),
            "Manual (1) is above 0"
        );
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn get_switch_value_invalid_id_maps_to_invalid_value() {
        let device = connected_device().await;
        let err = device.get_switch_value(MAX_SWITCH).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        device.set_connected(false).await.unwrap();
    }

    // ------------------------------------------------------------------
    // Write paths
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn output_and_usb_writes_round_trip_through_the_readback_frame() {
        let (device, factory) = connected_device_on(MockGeminiTransportFactory::default()).await;
        device.set_switch_value(DC3, 0.0).await.unwrap();
        assert_value(device.get_switch_value(DC3).await.unwrap(), 0.0);
        assert!(!factory.dc_is_on(3).await);
        device.set_switch(USB_A, false).await.unwrap();
        assert!(!device.get_switch(USB_A).await.unwrap());
        device.set_switch(USB_A, true).await.unwrap();
        assert!(device.get_switch(USB_A).await.unwrap());
        // Each write went out with its status request in one buffer.
        let sent = factory.sent_buffers().await;
        assert!(sent.contains(&">C3#\n>G#".to_string()), "{sent:?}");
        assert!(sent.contains(&">C6#\n>G#".to_string()), "{sent:?}");
        assert!(sent.contains(&">O6#\n>G#".to_string()), "{sent:?}");
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn a_write_is_confirmed_past_stale_streamed_frames() {
        // Two already-streamed, pre-write frames sit ahead of every reply.
        // The discard drops them, so the readback is the post-write frame
        // and the write succeeds on its first look.
        let (device, factory) = connected_device_on(
            MockGeminiTransportFactory::default().with_stale_frames_before_reply(),
        )
        .await;
        device.set_switch_value(DC2, 0.0).await.unwrap();
        assert_value(device.get_switch_value(DC2).await.unwrap(), 0.0);
        let sent = factory.sent_buffers().await;
        assert_eq!(
            sent.iter().filter(|c| c.as_str() == ">G#").count(),
            1,
            "only the handshake's >G# stands alone; no second look was needed: {sent:?}"
        );
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn a_write_the_hub_refuses_is_reported_after_a_second_look() {
        let (device, factory) =
            connected_device_on(MockGeminiTransportFactory::default().with_stuck_output(3)).await;
        let err = device.set_switch_value(DC3, 0.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert!(err.message.contains("DC3"), "{}", err.message);
        assert!(err.message.contains("asked for 0"), "{}", err.message);
        assert!(err.message.contains("reports 1"), "{}", err.message);
        // The set, then exactly one more look.
        assert_eq!(
            factory.sent_buffers().await,
            [">H#", ">V#", ">G#", ">C3#\n>G#", ">G#"]
        );
        // The cache holds what the hub reports, not what was asked.
        assert_value(device.get_switch_value(DC3).await.unwrap(), 1.0);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn the_dew_duty_is_writable_in_manual_and_round_trips() {
        let device = connected_device().await;
        assert!(device.can_write(DEW6_DUTY).await.unwrap());
        assert!(device.can_write(DEW7_DUTY).await.unwrap());
        device.set_switch_value(DEW6_DUTY, 35.0).await.unwrap();
        assert_value(device.get_switch_value(DEW6_DUTY).await.unwrap(), 35.0);
        device.set_switch(DEW7_DUTY, false).await.unwrap();
        assert_value(device.get_switch_value(DEW7_DUTY).await.unwrap(), 0.0);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn the_enabled_flag_is_read_only_in_manual_and_names_the_mode_switch() {
        let (device, factory) = connected_device_on(MockGeminiTransportFactory::default()).await;
        assert!(!device.can_write(DEW6_ENABLED).await.unwrap());
        assert!(!device.can_write(DEW7_ENABLED).await.unwrap());
        let err = device
            .set_switch_value(DEW6_ENABLED, 0.0)
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
        assert!(err.message.contains("DEW6 Mode"), "{}", err.message);
        assert!(err.message.contains("Manual"), "{}", err.message);
        assert_eq!(
            factory.sent_commands().await,
            [">H#", ">V#", ">G#"],
            "nothing reached the hub"
        );
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn in_auto_the_enabled_flag_is_writable_and_the_duty_is_not() {
        let (device, factory) =
            connected_device_on(MockGeminiTransportFactory::default().with_dew_modes(0, 2)).await;
        // DEW6 in Auto, DEW7 in Switch: both have the enabled flag writable.
        assert!(device.can_write(DEW6_ENABLED).await.unwrap());
        assert!(device.can_write(DEW7_ENABLED).await.unwrap());
        assert!(!device.can_write(DEW6_DUTY).await.unwrap());
        assert!(!device.can_write(DEW7_DUTY).await.unwrap());

        device.set_switch(DEW6_ENABLED, false).await.unwrap();
        assert!(!device.get_switch(DEW6_ENABLED).await.unwrap());
        let sent = factory.sent_buffers().await;
        assert!(sent.contains(&">Z10#\n>G#".to_string()), "{sent:?}");

        let err = device.set_switch_value(DEW6_DUTY, 50.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
        assert!(err.message.contains("Auto"), "{}", err.message);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn the_mode_cycles_through_all_three_and_moves_the_gates() {
        let (device, factory) = connected_device_on(MockGeminiTransportFactory::default()).await;
        device.set_switch_value(DEW6_MODE, 2.0).await.unwrap();
        assert_value(device.get_switch_value(DEW6_MODE).await.unwrap(), 2.0);
        assert!(device.can_write(DEW6_ENABLED).await.unwrap());
        assert!(!device.can_write(DEW6_DUTY).await.unwrap());

        device.set_switch_value(DEW6_MODE, 0.0).await.unwrap();
        assert_eq!(factory.dew_mode(0).await, 0);
        assert!(device.can_write(DEW6_ENABLED).await.unwrap());

        device.set_switch_value(DEW6_MODE, 1.0).await.unwrap();
        assert!(device.can_write(DEW6_DUTY).await.unwrap());
        assert!(!device.can_write(DEW6_ENABLED).await.unwrap());
        // DEW7 never moved.
        assert_value(device.get_switch_value(DEW7_MODE).await.unwrap(), 1.0);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn auto_is_refused_while_a_sensor_is_absent_and_nothing_is_sent() {
        let (device, factory) =
            connected_device_on(MockGeminiTransportFactory::default().with_sensors(true, false))
                .await;
        let err = device.set_switch_value(DEW6_MODE, 0.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert!(err.message.contains("DEW6"), "{}", err.message);
        assert!(err.message.contains("DS18B20"), "{}", err.message);
        assert_eq!(factory.sent_commands().await, [">H#", ">V#", ">G#"]);
        // Manual and Switch are still open to it.
        device.set_switch_value(DEW6_MODE, 2.0).await.unwrap();
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn auto_is_refused_naming_the_ambient_sensor_when_that_is_the_one_missing() {
        let (device, _) =
            connected_device_on(MockGeminiTransportFactory::default().with_sensors(false, true))
                .await;
        let err = device.set_switch(DEW7_MODE, false).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert!(err.message.contains("AHT20"), "{}", err.message);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_switch_drives_the_endpoints_of_every_writable_row() {
        // ASCOM's rule: true is MaxSwitchValue, false is MinSwitchValue.
        let device = connected_device().await;
        device.set_switch(DEW6_DUTY, true).await.unwrap();
        assert_value(device.get_switch_value(DEW6_DUTY).await.unwrap(), 100.0);
        device.set_switch(DEW6_MODE, true).await.unwrap();
        assert_value(device.get_switch_value(DEW6_MODE).await.unwrap(), 2.0);
        device.set_switch(DEW6_MODE, false).await.unwrap();
        assert_value(device.get_switch_value(DEW6_MODE).await.unwrap(), 0.0);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn a_write_to_dc1_or_a_telemetry_row_is_not_implemented() {
        let (device, factory) = connected_device_on(MockGeminiTransportFactory::default()).await;
        for id in [
            DC1,
            INPUT_VOLTAGE,
            TEMPERATURE,
            AMBIENT_ATTACHED,
            PROBE_ATTACHED,
        ] {
            let err = device.set_switch_value(id, 1.0).await.unwrap_err();
            assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED, "switch {id}");
            assert!(!device.can_write(id).await.unwrap(), "switch {id}");
        }
        assert_eq!(factory.sent_commands().await, [">H#", ">V#", ">G#"]);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn a_value_between_steps_is_rounded_to_the_nearest_step() {
        // ConformU writes 0.25, 0.5 and 0.75 of every range and expects the
        // write taken; the rows step by 1, so the value is rounded, half up.
        let device = connected_device().await;
        device.set_switch_value(DC2, 0.25).await.unwrap();
        assert_value(device.get_switch_value(DC2).await.unwrap(), 0.0);
        device.set_switch_value(DC2, 0.5).await.unwrap();
        assert_value(device.get_switch_value(DC2).await.unwrap(), 1.0);
        device.set_switch_value(DEW6_DUTY, 50.5).await.unwrap();
        assert_value(device.get_switch_value(DEW6_DUTY).await.unwrap(), 51.0);
        device.set_switch_value(DEW6_MODE, 1.5).await.unwrap();
        assert_value(device.get_switch_value(DEW6_MODE).await.unwrap(), 2.0);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn out_of_range_and_non_finite_values_are_invalid_and_never_sent() {
        let (device, factory) = connected_device_on(MockGeminiTransportFactory::default()).await;
        for (id, value) in [
            (DC2, 2.0),
            (DC2, -1.0),
            (DC2, f64::NAN),
            (DEW6_DUTY, 101.0),
            (DEW6_MODE, 3.0),
            (DEW6_MODE, f64::INFINITY),
        ] {
            let err = device.set_switch_value(id, value).await.unwrap_err();
            assert_eq!(
                err.code,
                ASCOMErrorCode::INVALID_VALUE,
                "switch {id} value {value}: {}",
                err.message
            );
        }
        let err = device.set_switch_value(MAX_SWITCH, 1.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        assert_eq!(factory.sent_commands().await, [">H#", ">V#", ">G#"]);
        device.set_connected(false).await.unwrap();
    }

    // ------------------------------------------------------------------
    // Labels
    // ------------------------------------------------------------------

    async fn connected_device_with_labels() -> GeminiSwitchDevice {
        let mut config = Config::default();
        config.switch.labels =
            serde_json::from_str(r#"{"DC2": "Mount", "DEW6": "Guide scope heater"}"#).unwrap();
        let manager = GeminiManager::new(&config, Arc::new(MockGeminiTransportFactory::default()));
        let device = GeminiSwitchDevice::new(config.switch, manager);
        device.set_connected(true).await.unwrap();
        device
    }

    #[tokio::test]
    async fn a_label_renames_the_connector_and_a_dew_label_its_three_rows() {
        let device = connected_device_with_labels().await;
        assert_eq!(device.get_switch_name(DC2).await.unwrap(), "Mount");
        assert_eq!(device.get_switch_name(DC3).await.unwrap(), "DC3");
        assert_eq!(
            device.get_switch_name(DEW6_DUTY).await.unwrap(),
            "Guide scope heater Duty"
        );
        assert_eq!(
            device.get_switch_name(DEW6_ENABLED).await.unwrap(),
            "Guide scope heater Enabled"
        );
        assert_eq!(
            device.get_switch_name(DEW6_MODE).await.unwrap(),
            "Guide scope heater Mode"
        );
        assert_eq!(
            device.get_switch_name(DEW7_DUTY).await.unwrap(),
            "DEW7 Duty"
        );
        // The description still names the connector.
        assert_eq!(
            device.get_switch_description(DC2).await.unwrap(),
            "Switches the 12 V output DC2"
        );
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn a_gate_and_an_unconfirmed_write_name_the_labelled_switch() {
        let device = connected_device_with_labels().await;
        let err = device
            .set_switch_value(DEW6_ENABLED, 1.0)
            .await
            .unwrap_err();
        assert!(
            err.message.contains("Guide scope heater Enabled"),
            "{}",
            err.message
        );
        assert!(
            err.message.contains("Guide scope heater Mode"),
            "{}",
            err.message
        );
        device.set_connected(false).await.unwrap();
    }

    // ------------------------------------------------------------------
    // Metadata and the async surface
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn max_switch_and_the_published_ranges() {
        let device = connected_device().await;
        assert_eq!(device.max_switch().await.unwrap(), MAX_SWITCH);
        for (id, min, max, step) in [
            (DC1, 0.0, 1.0, 1.0),
            (DEW6_DUTY, 0.0, 100.0, 1.0),
            (DEW6_MODE, 0.0, 2.0, 1.0),
            (INPUT_VOLTAGE, 0.0, 19.0, 0.1),
            (TEMPERATURE, -127.0, 85.0, 0.01),
            (LENS_TEMPERATURE, -127.0, 125.0, 0.01),
        ] {
            assert_value(device.min_switch_value(id).await.unwrap(), min);
            assert_value(device.max_switch_value(id).await.unwrap(), max);
            assert_value(device.switch_step(id).await.unwrap(), step);
        }
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn async_surface_follows_can_async_false() {
        let (device, factory) = connected_device_on(MockGeminiTransportFactory::default()).await;
        assert!(!device.can_async(DC2).await.unwrap());
        for err in [
            device.state_change_complete(DC2).await.unwrap_err(),
            device.set_async(DC2, false).await.unwrap_err(),
            device.set_async_value(DC2, 0.0).await.unwrap_err(),
        ] {
            assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED, "{err}");
        }
        device.cancel_async(DC2).await.unwrap();
        assert_eq!(
            factory.sent_commands().await,
            [">H#", ">V#", ">G#"],
            "neither refused write reached the hub"
        );
        for err in [
            device.can_async(MAX_SWITCH).await.unwrap_err(),
            device.state_change_complete(MAX_SWITCH).await.unwrap_err(),
            device.cancel_async(MAX_SWITCH).await.unwrap_err(),
            device.set_async(MAX_SWITCH, true).await.unwrap_err(),
            device.set_async_value(MAX_SWITCH, 1.0).await.unwrap_err(),
        ] {
            assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        }
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_switch_name_returns_not_implemented() {
        let device = make_device();
        let err = device
            .set_switch_name(DC2, "x".to_string())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn device_metadata_comes_from_config_and_driver_info_names_the_firmware() {
        let device = make_device();
        let config = Config::default();
        assert_eq!(device.static_name(), config.switch.name);
        assert_eq!(device.unique_id(), config.switch.unique_id);
        assert_eq!(
            device.description().await.unwrap(),
            config.switch.description
        );
        assert_eq!(
            device.driver_version().await.unwrap(),
            env!("CARGO_PKG_VERSION")
        );
        let before = device.driver_info().await.unwrap();
        assert!(before.contains("Advanced 3"), "{before}");
        assert!(!before.contains("firmware"), "{before}");
        device.set_connected(true).await.unwrap();
        let after = device.driver_info().await.unwrap();
        assert!(after.contains("firmware 3.0.8"), "{after}");
        device.set_connected(false).await.unwrap();
    }

    // ------------------------------------------------------------------
    // Setup pages
    // ------------------------------------------------------------------

    fn get_request() -> SetupRequest {
        SetupRequest {
            method: axum::http::Method::GET,
            headers: axum::http::HeaderMap::new(),
            query: Vec::new(),
            form: Vec::new(),
        }
    }

    #[tokio::test]
    async fn there_are_no_actions_and_a_device_without_pages_has_no_setup_page() {
        let device = make_device();
        assert_eq!(
            device.supported_actions().await.unwrap(),
            Vec::<String>::new()
        );
        let err = device
            .action("config.get".to_string(), String::new())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::ACTION_NOT_IMPLEMENTED);
        let response = device.setup_request(get_request()).await;
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn bool_value_maps_to_the_ascom_endpoints() {
        assert_value(bool_value(true), 1.0);
        assert_value(bool_value(false), 0.0);
    }
}
