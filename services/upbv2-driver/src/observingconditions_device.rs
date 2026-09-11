//! UPBv2 `ObservingConditions` device implementation.
//!
//! Like the Switch device, this holds an `Option<Session<Upbv2Codec>>` —
//! the session existing is the canonical "Connected" state.
//!
//! The three sensors (temperature, humidity, dewpoint) all come from the one
//! `PA` frame the manager polls, and are served through the shared
//! sliding-window mean in [`rusty_photon_rolling_stats`].

use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::{Device, ObservingConditions};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use async_trait::async_trait;
use rusty_photon_shared_transport::Session;
use tokio::sync::RwLock;
use tracing::debug;

use crate::codec::Upbv2Codec;
use crate::config::ObservingConditionsConfig;
use crate::config_actions::Upbv2Driver;
use crate::error::Upbv2Error;
use crate::manager::Upbv2Manager;
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
pub struct Upbv2ObservingConditionsDevice {
    config: ObservingConditionsConfig,
    #[debug(skip)]
    session: Arc<RwLock<Option<Session<Upbv2Codec>>>>,
    #[debug(skip)]
    manager: Arc<Upbv2Manager>,
    /// Shared (cloned) config-action context; `Some` on the normal path through
    /// `ServerBuilder`, `None` for focused unit-test devices.
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<Upbv2Driver>>,
}

impl Upbv2ObservingConditionsDevice {
    #[must_use]
    pub fn new(config: ObservingConditionsConfig, manager: Arc<Upbv2Manager>) -> Self {
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
    pub fn with_config_actions(mut self, ctx: ConfigActionCtx<Upbv2Driver>) -> Self {
        self.config_ctx = Some(ctx);
        self
    }
}

#[async_trait]
impl Device for Upbv2ObservingConditionsDevice {
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
                // `?` does SessionError → Upbv2Error via the manual
                // .map_err, then Upbv2Error → ASCOMError via the From
                // impl in error.rs.
                let session = self
                    .manager
                    .transport()
                    .acquire()
                    .await
                    .map_err(Upbv2Error::from)?;
                *slot = Some(session);
                debug!("ObservingConditions device connected");
            }
            (false, true) => {
                if let Some(session) = slot.take() {
                    // `Session::close` returns Result<_, TransportError>;
                    // `From<TransportError> for Upbv2Error` handles the
                    // conversion, and the existing `From<Upbv2Error> for
                    // ASCOMError` does the second hop on `?`.
                    session.close().await.map_err(Upbv2Error::from)?;
                }
                debug!("ObservingConditions device disconnected");
            }
            _ => {}
        }
        Ok(())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok(
            "UPBv2 Driver - ObservingConditions interface for Pegasus Astro \
            Ultimate Powerbox v2 environmental sensors"
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
        rusty_photon_driver::dispatch::<Upbv2Driver>(&self.config_ctx, action, parameters).await
    }
}

#[async_trait]
impl ObservingConditions for Upbv2ObservingConditionsDevice {
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
            "temperature" => Ok("UPBv2 internal temperature sensor".to_string()),
            "humidity" => Ok("UPBv2 internal humidity sensor".to_string()),
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
    use crate::mock::MockUpbv2TransportFactory;
    use ascom_alpaca::ASCOMErrorCode;
    use async_trait::async_trait;
    use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};

    /// Factory whose `open()` always fails. Used to exercise the
    /// `set_connected(true)` acquire-failure mapping into ASCOM errors —
    /// the BDD suite can't reach this path because its mock always
    /// succeeds.
    struct FailingUpbv2TransportFactory;

    #[async_trait]
    impl TransportFactory for FailingUpbv2TransportFactory {
        async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
            Err(TransportError::Open(std::io::Error::other(
                "mock factory error",
            )))
        }
    }

    fn make_device_with_manager() -> (Upbv2ObservingConditionsDevice, Arc<Upbv2Manager>) {
        let factory = Arc::new(MockUpbv2TransportFactory::default());
        let config = Config::default();
        let manager = Upbv2Manager::new(&config, factory);
        let device =
            Upbv2ObservingConditionsDevice::new(config.observingconditions, Arc::clone(&manager));
        (device, manager)
    }

    fn make_device() -> Upbv2ObservingConditionsDevice {
        make_device_with_manager().0
    }

    fn make_device_with_failing_factory() -> Upbv2ObservingConditionsDevice {
        let factory = Arc::new(FailingUpbv2TransportFactory);
        let config = Config::default();
        let manager = Upbv2Manager::new(&config, factory);
        Upbv2ObservingConditionsDevice::new(config.observingconditions, manager)
    }

    async fn connected_device() -> Upbv2ObservingConditionsDevice {
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
    async fn a_real_ten_second_average_does_not_read_back_as_zero() {
        // Ten seconds was once the window that stood in for "no averaging",
        // and the read-back inferred the period from the window — so asking
        // for a genuine ten-second average got 0 hours back, which means the
        // opposite. The period is now recorded as set.
        let device = connected_device().await;
        let ten_seconds_in_hours = 10.0 / 3600.0;
        device
            .set_average_period(ten_seconds_in_hours)
            .await
            .unwrap();
        let period = device.average_period().await.unwrap();
        assert!(
            (period - ten_seconds_in_hours).abs() < f64::EPSILON,
            "expected {ten_seconds_in_hours} hours back, got {period}"
        );
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
        let (device, manager) = make_device_with_manager();
        device.set_connected(true).await.unwrap();

        // The handshake seeds the cache from one `PA` frame, so every rolling
        // mean holds exactly that sample. Asserting against the cache pins
        // each ASCOM property to its own wire field without hard-coding the
        // mock's numbers here as well as in the mock.
        let status = manager.get_cached_state().await.status.unwrap();
        let t = device.temperature().await.unwrap();
        assert!((t - status.temperature).abs() < 0.01, "temperature: {t}");
        let h = device.humidity().await.unwrap();
        assert!((h - status.humidity).abs() < 0.01, "humidity: {h}");
        let d = device.dew_point().await.unwrap();
        assert!((d - status.dewpoint).abs() < 0.01, "dewpoint: {d}");

        // Distinct readings, so a swapped pair of fields cannot pass above.
        assert_ne!(status.temperature, status.humidity);
        assert_ne!(status.temperature, status.dewpoint);
        assert_ne!(status.humidity, status.dewpoint);

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

    // Upbv2Error → ASCOMError mapping tests moved to error.rs once the
    // canonical mapping landed there (centralised so both devices share
    // the same classification).
}
