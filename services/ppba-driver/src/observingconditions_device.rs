//! PPBA `ObservingConditions` device implementation.
//!
//! Like the Switch device, this holds an `Option<Session<PpbaCodec>>` —
//! the session existing is the canonical "Connected" state.

use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::{Device, ObservingConditions};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use async_trait::async_trait;
use rusty_photon_shared_transport::Session;
use tokio::sync::RwLock;
use tracing::debug;

use crate::codec::PpbaCodec;
use crate::config::ObservingConditionsConfig;
use crate::config_actions::PpbaDriver;
use crate::error::PpbaError;
use crate::manager::PpbaManager;
use rusty_photon_driver::ConfigActionCtx;

macro_rules! ensure_connected {
    ($self:ident) => {
        if !$self.connected().await.is_ok_and(|c| c) {
            debug!("ObservingConditions device not connected");
            return Err(ASCOMError::NOT_CONNECTED);
        }
    };
}

#[derive(derive_more::Debug)]
pub struct PpbaObservingConditionsDevice {
    config: ObservingConditionsConfig,
    #[debug(skip)]
    session: Arc<RwLock<Option<Session<PpbaCodec>>>>,
    #[debug(skip)]
    manager: Arc<PpbaManager>,
    /// Shared (cloned) config-action context; `Some` on the normal path through
    /// `ServerBuilder`, `None` for focused unit-test devices.
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<PpbaDriver>>,
}

impl PpbaObservingConditionsDevice {
    #[must_use]
    pub fn new(config: ObservingConditionsConfig, manager: Arc<PpbaManager>) -> Self {
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
}

#[async_trait]
impl Device for PpbaObservingConditionsDevice {
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
                debug!("ObservingConditions device connected");
            }
            (false, true) => {
                if let Some(session) = slot.take() {
                    // `Session::close` returns Result<_, TransportError>;
                    // `From<TransportError> for PpbaError` handles the
                    // conversion, and the existing `From<PpbaError> for
                    // ASCOMError` does the second hop on `?`.
                    session.close().await.map_err(PpbaError::from)?;
                }
                debug!("ObservingConditions device disconnected");
            }
            _ => {}
        }
        Ok(())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("PPBA Driver - ObservingConditions interface for Pegasus Astro PPBA Gen2 environmental sensors".to_string())
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
impl ObservingConditions for PpbaObservingConditionsDevice {
    async fn average_period(&self) -> ASCOMResult<f64> {
        ensure_connected!(self);
        Ok(self.manager.get_cached_state().await.average_period_hours)
    }

    async fn set_average_period(&self, period: f64) -> ASCOMResult<()> {
        ensure_connected!(self);
        if period < 0.0 {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Average period cannot be negative, got {period}"),
            ));
        }
        if period > 24.0 {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Average period cannot exceed 24 hours, got {period}"),
            ));
        }
        // Passed through in hours, not as a window: the manager is where the
        // mapping lives, so a period set over the wire and the same period
        // read from config cannot drift apart, and the requested value is
        // recorded verbatim for read-back.
        self.manager.set_averaging_period(period).await;
        debug!("Average period set to {} hours", period);
        Ok(())
    }

    async fn temperature(&self) -> ASCOMResult<f64> {
        ensure_connected!(self);
        self.manager
            .get_cached_state()
            .await
            .temp_mean
            .get_mean()
            .ok_or_else(|| {
                ASCOMError::new(
                    ASCOMErrorCode::VALUE_NOT_SET,
                    "No temperature data available yet",
                )
            })
    }

    async fn humidity(&self) -> ASCOMResult<f64> {
        ensure_connected!(self);
        self.manager
            .get_cached_state()
            .await
            .humidity_mean
            .get_mean()
            .ok_or_else(|| {
                ASCOMError::new(
                    ASCOMErrorCode::VALUE_NOT_SET,
                    "No humidity data available yet",
                )
            })
    }

    async fn dew_point(&self) -> ASCOMResult<f64> {
        ensure_connected!(self);
        self.manager
            .get_cached_state()
            .await
            .dewpoint_mean
            .get_mean()
            .ok_or_else(|| {
                ASCOMError::new(
                    ASCOMErrorCode::VALUE_NOT_SET,
                    "No dewpoint data available yet",
                )
            })
    }

    async fn time_since_last_update(&self, sensor_name: String) -> ASCOMResult<f64> {
        ensure_connected!(self);
        let state = self.manager.get_cached_state().await;
        let duration = match sensor_name.to_lowercase().as_str() {
            "" => {
                let times = [
                    state.temp_mean.time_since_last_update(),
                    state.humidity_mean.time_since_last_update(),
                    state.dewpoint_mean.time_since_last_update(),
                ];
                times
                    .iter()
                    .filter_map(|&t| t)
                    .min()
                    .or(Some(Duration::ZERO))
            }
            "temperature" => state.temp_mean.time_since_last_update(),
            "humidity" => state.humidity_mean.time_since_last_update(),
            "dewpoint" => state.dewpoint_mean.time_since_last_update(),
            "cloudcover" | "pressure" | "rainrate" | "skybrightness" | "skyquality"
            | "starfwhm" | "skytemperature" | "winddirection" | "windgust" | "windspeed" => {
                return Err(ASCOMError::NOT_IMPLEMENTED);
            }
            _ => {
                return Err(ASCOMError::new(
                    ASCOMErrorCode::INVALID_VALUE,
                    format!("Unknown sensor name: {sensor_name}"),
                ))
            }
        };
        Ok(duration.map_or(f64::MAX, |d| d.as_secs_f64()))
    }

    async fn sensor_description(&self, sensor_name: String) -> ASCOMResult<String> {
        ensure_connected!(self);
        match sensor_name.to_lowercase().as_str() {
            "temperature" => Ok("PPBA internal temperature sensor".to_string()),
            "humidity" => Ok("PPBA internal humidity sensor".to_string()),
            "dewpoint" => Ok("Dewpoint calculated from temperature and humidity".to_string()),
            "" => Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                "Sensor name cannot be empty".to_string(),
            )),
            "cloudcover" | "pressure" | "rainrate" | "skybrightness" | "skyquality"
            | "starfwhm" | "skytemperature" | "winddirection" | "windgust" | "windspeed" => {
                Err(ASCOMError::NOT_IMPLEMENTED)
            }
            _ => Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Unknown sensor name: {sensor_name}"),
            )),
        }
    }

    async fn refresh(&self) -> ASCOMResult<()> {
        ensure_connected!(self);
        let guard = self.session.read().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| ASCOMError::new(ASCOMErrorCode::NOT_CONNECTED, "not connected"))?;
        self.manager.refresh_status(session).await?;
        drop(guard);
        debug!("ObservingConditions sensors refreshed");
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::manager::instantaneous_window;
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

    fn make_device() -> PpbaObservingConditionsDevice {
        let factory = Arc::new(MockPpbaTransportFactory::default());
        let config = Config::default();
        let manager = PpbaManager::new(&config, factory);
        PpbaObservingConditionsDevice::new(config.observingconditions, manager)
    }

    fn make_device_with_failing_factory() -> PpbaObservingConditionsDevice {
        let factory = Arc::new(FailingPpbaTransportFactory);
        let config = Config::default();
        let manager = PpbaManager::new(&config, factory);
        PpbaObservingConditionsDevice::new(config.observingconditions, manager)
    }

    async fn connected_device() -> PpbaObservingConditionsDevice {
        let device = make_device();
        device.set_connected(true).await.unwrap();
        device
    }

    /// The averaging window for the staleness test: long enough that the
    /// handshake's samples are comfortably inside it on any runner, short
    /// enough to age out during a test.
    const STALE_WINDOW: Duration = Duration::from_secs(1);

    /// A connected device whose poll loop will not fire for the length of a
    /// test and whose window is [`STALE_WINDOW`], so the samples the handshake
    /// seeded age out with nothing arriving to replace them — the state a
    /// stalled poll loop leaves behind.
    ///
    /// The window comes from config rather than a later `SetAveragePeriod`
    /// deliberately. Resizing evicts, so a test that shrank the window would
    /// race its own setup: slow enough setup empties the buffer, and an empty
    /// buffer reads `VALUE_NOT_SET` whether or not the window is applied on
    /// read. The manager comes back so a test can assert occupancy directly.
    async fn connected_device_with_stale_window(
    ) -> (PpbaObservingConditionsDevice, Arc<PpbaManager>) {
        let factory = Arc::new(MockPpbaTransportFactory::default());
        let mut config = Config::default();
        config.serial.polling_interval = Duration::from_mins(5);
        config.observingconditions.averaging_period = STALE_WINDOW;
        let manager = PpbaManager::new(&config, factory);
        let device =
            PpbaObservingConditionsDevice::new(config.observingconditions, Arc::clone(&manager));
        device.set_connected(true).await.unwrap();
        (device, manager)
    }

    #[tokio::test]
    async fn starts_disconnected() {
        let device = make_device();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn connect_disconnect_round_trip() {
        let device = make_device();
        device.set_connected(true).await.unwrap();
        assert!(device.connected().await.unwrap());
        device.set_connected(false).await.unwrap();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn operations_fail_when_not_connected() {
        let device = make_device();
        assert_eq!(
            device.temperature().await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.humidity().await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.dew_point().await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.average_period().await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.set_average_period(1.0).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            device.refresh().await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
    }

    #[tokio::test]
    async fn set_average_period_negative_is_invalid_value() {
        let device = connected_device().await;
        let err = device.set_average_period(-1.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_average_period_too_large_is_invalid_value() {
        let device = connected_device().await;
        let err = device.set_average_period(25.0).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_average_period_zero_is_instantaneous_mode() {
        let device = connected_device().await;
        device.set_average_period(0.0).await.unwrap();
        let period = device.average_period().await.unwrap();
        assert!((period - 0.0).abs() < f64::EPSILON);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn average_period_reads_back_what_was_set_even_at_the_instantaneous_window() {
        // The collision the verbatim store exists to survive: a genuine
        // average whose window is exactly the one that stands in for "not
        // averaging". Derived from the configured cadence rather than written
        // as a constant, because the instantaneous window scales with it — a
        // hard-coded value silently stops being the collision case the moment
        // that mapping changes.
        let device = connected_device().await;
        let collision = instantaneous_window(Config::default().serial.polling_interval);
        let collision_in_hours = collision.as_secs_f64() / 3600.0;

        device.set_average_period(collision_in_hours).await.unwrap();

        let period = device.average_period().await.unwrap();
        assert!(
            (period - collision_in_hours).abs() < f64::EPSILON,
            "expected {collision_in_hours} hours ({collision:?}) back, got {period}"
        );
        assert!(
            period > 0.0,
            "a real average whose window equals the instantaneous one must not read back as 0"
        );
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn sensor_reads_report_value_not_set_once_every_sample_has_aged_out() {
        // Eviction runs on insert, so a session that stops polling leaves the
        // buffer full of readings older than the averaging period. Averaging
        // them and calling the answer current is what a client would set dew
        // heaters from; VALUE_NOT_SET is the honest answer instead, and it is
        // the code the device already returns before the first poll.
        let (device, manager) = connected_device_with_stale_window().await;

        // All three answer first. This assertion is what keeps the rest of the
        // test honest — an empty buffer produces the same VALUE_NOT_SET below,
        // so without proving the samples are there and readable, the test
        // could pass with the read-side window doing nothing at all.
        device.temperature().await.expect("temperature reads fresh");
        device.humidity().await.expect("humidity reads fresh");
        device.dew_point().await.expect("dewpoint reads fresh");

        // Only the floor matters: load can push the samples further outside
        // the window, never back inside it.
        tokio::time::sleep(STALE_WINDOW + Duration::from_millis(500)).await;

        // Still held. The only eviction paths are an insert — the poll loop is
        // idle — and a window change, of which there is none, so a None below
        // can come from nothing but the read-side filter.
        assert!(
            manager.get_cached_state().await.temp_mean.sample_count() > 0,
            "the buffer must still hold the aged-out samples, or this proves nothing"
        );

        for (sensor, result) in [
            ("temperature", device.temperature().await),
            ("humidity", device.humidity().await),
            ("dewpoint", device.dew_point().await),
        ] {
            let err = result.expect_err(sensor);
            assert_eq!(err.code, ASCOMErrorCode::VALUE_NOT_SET, "{sensor}");
        }

        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn sensor_descriptions() {
        let device = connected_device().await;
        let t = device
            .sensor_description("temperature".to_string())
            .await
            .unwrap();
        assert!(t.contains("temperature"));
        let err = device
            .sensor_description("pressure".to_string())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
        let err = device
            .sensor_description("foobar".to_string())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn read_sensor_values_after_handshake() {
        let device = connected_device().await;
        let t = device.temperature().await.unwrap();
        assert!((t - 25.0).abs() < 0.01);
        let h = device.humidity().await.unwrap();
        assert!((h - 60.0).abs() < 0.01);
        let d = device.dew_point().await.unwrap();
        assert!((d - 15.5).abs() < 0.01);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn refresh_succeeds_when_connected() {
        let device = connected_device().await;
        device.refresh().await.unwrap();
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_connected_acquire_failure_maps_to_invalid_operation() {
        let device = make_device_with_failing_factory();
        let err = device.set_connected(true).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert!(
            err.message.contains("mock factory error"),
            "expected message to carry the underlying io error, got: {}",
            err.message
        );
        assert!(!device.connected().await.unwrap());
    }

    // PpbaError → ASCOMError mapping tests moved to error.rs once the
    // canonical mapping landed there (centralised so both devices share
    // the same classification).
}
