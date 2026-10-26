//! The Advanced 3's `ObservingConditions` device.
//!
//! Like the Switch device, this holds an `Option<Session<GeminiCodec>>` —
//! the session existing is the canonical "Connected" state.
//!
//! `Temperature`, `Humidity` and `DewPoint` are the AHT20's three values
//! from the frame the manager polls, each through the shared sliding-window
//! mean in [`rusty_photon_rolling_stats`]. The manager feeds a window only
//! while the AHT20's attached flag is `1`, and empties the three the first
//! time it reads `0`, so an absent sensor reads `VALUE_NOT_SET` at once. The
//! lens probe has no property here; it is Switch row 23.

use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::{Device, ObservingConditions};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult, SetupRequest, SetupResponse};
use async_trait::async_trait;
use rusty_photon_setup::SetupPages;
use rusty_photon_shared_transport::Session;
use tokio::sync::RwLock;
use tracing::debug;

use crate::codec::GeminiCodec;
use crate::config::ObservingConditionsConfig;
use crate::error::GeminiError;
use crate::manager::{average_period_hours, GeminiManager};
use crate::setup::{GeminiDriver, OBSERVING_CONDITIONS_DEVICE_TYPE};

macro_rules! ensure_connected {
    ($self:ident) => {
        if !$self.connected().await.is_ok_and(|c| c) {
            debug!("ObservingConditions device not connected");
            return Err(ASCOMError::NOT_CONNECTED);
        }
    };
}

#[derive(derive_more::Debug)]
pub struct GeminiObservingConditionsDevice {
    config: ObservingConditionsConfig,
    #[debug(skip)]
    session: Arc<RwLock<Option<Session<GeminiCodec>>>>,
    #[debug(skip)]
    manager: Arc<GeminiManager>,
    /// The setup pages behind `/setup/v1/observingconditions/0/setup`:
    /// `Some` when the driver was built with a config source (the normal
    /// path through `ServerBuilder`); `None` for focused unit-test devices.
    #[debug(skip)]
    pages: Option<Arc<SetupPages<GeminiDriver>>>,
}

impl GeminiObservingConditionsDevice {
    #[must_use]
    pub fn new(config: ObservingConditionsConfig, manager: Arc<GeminiManager>) -> Self {
        Self {
            config,
            session: Arc::new(RwLock::new(None)),
            manager,
            pages: None,
        }
    }

    /// Attach the shared setup pages, so the device's setup page renders and
    /// saves the `observingconditions` section.
    #[must_use]
    pub fn with_setup_pages(mut self, pages: Arc<SetupPages<GeminiDriver>>) -> Self {
        self.pages = Some(pages);
        self
    }
}

#[async_trait]
impl Device for GeminiObservingConditionsDevice {
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
                let session = self
                    .manager
                    .transport()
                    .acquire()
                    .await
                    .map_err(GeminiError::from)?;
                *slot = Some(session);
                debug!("ObservingConditions device connected");
            }
            (false, true) => {
                if let Some(session) = slot.take() {
                    session.close().await.map_err(GeminiError::from)?;
                }
                debug!("ObservingConditions device disconnected");
            }
            _ => {}
        }
        Ok(())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok(
            "Gemini Power & Data Hubs Advanced 3 Driver - ObservingConditions interface for \
             the hub's AHT20 ambient sensor"
                .to_string(),
        )
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

    /// The device's setup page, `/setup/v1/observingconditions/0/setup`,
    /// for `GET` and `POST`, through the shared handler.
    fn setup_request(
        &self,
        request: SetupRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SetupResponse> + Send + '_>> {
        match &self.pages {
            Some(pages) => rusty_photon_driver::device_setup_request(
                pages,
                OBSERVING_CONDITIONS_DEVICE_TYPE,
                0,
                request,
            ),
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
impl ObservingConditions for GeminiObservingConditionsDevice {
    async fn average_period(&self) -> ASCOMResult<f64> {
        ensure_connected!(self);
        Ok(self.manager.get_cached_state().await.average_period_hours)
    }

    async fn set_average_period(&self, period: f64) -> ASCOMResult<()> {
        ensure_connected!(self);
        // A range check, not a pair of ordered comparisons: every ordered
        // comparison against NaN is false, so a `period < low || period > high`
        // form accepts NaN. `contains` is false for NaN, so this rejects it.
        let allowed = average_period_hours();
        if !allowed.contains(&period) {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!(
                    "Average period must be a finite value in [{}, {}] hours, got {period}",
                    allowed.start(),
                    allowed.end()
                ),
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
                    "No temperature reading: the window is empty, or the AHT20 is not attached",
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
                    "No humidity reading: the window is empty, or the AHT20 is not attached",
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
                    "No dewpoint reading: the window is empty, or the AHT20 is not attached",
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
            "temperature" => {
                Ok("Ambient temperature from the AHT20 sensor on the hub's Temp port".to_string())
            }
            "humidity" => {
                Ok("Relative humidity from the AHT20 sensor on the hub's Temp port".to_string())
            }
            "dewpoint" => Ok(
                "Dewpoint the hub computes from the AHT20's temperature and \
                             humidity"
                    .to_string(),
            ),
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

    /// One fresh frame now, outside the poll cadence.
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
    use crate::mock::MockGeminiTransportFactory;
    use ascom_alpaca::ASCOMErrorCode;
    use async_trait::async_trait;
    use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};

    /// Factory whose `open()` always fails.
    struct FailingFactory;

    #[async_trait]
    impl TransportFactory for FailingFactory {
        async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
            Err(TransportError::Open(std::io::Error::other(
                "mock factory error",
            )))
        }
    }

    fn make_device_on(
        factory: Arc<dyn TransportFactory>,
    ) -> (GeminiObservingConditionsDevice, Arc<GeminiManager>) {
        let config = Config::default();
        let manager = GeminiManager::new(&config, factory);
        let device =
            GeminiObservingConditionsDevice::new(config.observingconditions, Arc::clone(&manager));
        (device, manager)
    }

    fn make_device() -> GeminiObservingConditionsDevice {
        make_device_on(Arc::new(MockGeminiTransportFactory::default())).0
    }

    /// The mock as the trait object the manager takes.
    fn dyn_factory(factory: &Arc<MockGeminiTransportFactory>) -> Arc<dyn TransportFactory> {
        let concrete: Arc<MockGeminiTransportFactory> = Arc::clone(factory);
        concrete
    }

    async fn connected_device() -> GeminiObservingConditionsDevice {
        let device = make_device();
        device.set_connected(true).await.unwrap();
        device
    }

    #[tokio::test]
    async fn starts_disconnected_and_round_trips() {
        let device = make_device();
        assert!(!device.connected().await.unwrap());
        device.set_connected(true).await.unwrap();
        assert!(device.connected().await.unwrap());
        device.set_connected(false).await.unwrap();
        assert!(!device.connected().await.unwrap());
    }

    #[tokio::test]
    async fn operations_fail_when_not_connected() {
        let device = make_device();
        for code in [
            device.temperature().await.unwrap_err().code,
            device.humidity().await.unwrap_err().code,
            device.dew_point().await.unwrap_err().code,
            device.average_period().await.unwrap_err().code,
            device.set_average_period(1.0).await.unwrap_err().code,
            device.refresh().await.unwrap_err().code,
        ] {
            assert_eq!(code, ASCOMErrorCode::NOT_CONNECTED);
        }
    }

    #[tokio::test]
    async fn set_average_period_rejects_negative_too_large_and_nan() {
        let device = connected_device().await;
        for bad in [-1.0, 25.0, f64::NAN] {
            let err = device.set_average_period(bad).await.unwrap_err();
            assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE, "{bad}");
        }
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn average_period_reads_back_as_set_zero_included() {
        let device = connected_device().await;
        device.set_average_period(0.0).await.unwrap();
        assert!((device.average_period().await.unwrap() - 0.0).abs() < f64::EPSILON);
        let ten_seconds_in_hours = 10.0 / 3600.0;
        device
            .set_average_period(ten_seconds_in_hours)
            .await
            .unwrap();
        let period = device.average_period().await.unwrap();
        assert!(
            (period - ten_seconds_in_hours).abs() < f64::EPSILON,
            "{period}"
        );
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn the_three_readings_are_the_aht20s_fields() {
        let (device, manager) = make_device_on(Arc::new(MockGeminiTransportFactory::default()));
        device.set_connected(true).await.unwrap();
        let frame = manager.get_cached_state().await.status.unwrap();
        let t = device.temperature().await.unwrap();
        assert!(
            (t - frame.ambient_temperature).abs() < 0.01,
            "temperature: {t}"
        );
        let h = device.humidity().await.unwrap();
        assert!((h - frame.humidity).abs() < 0.01, "humidity: {h}");
        let d = device.dew_point().await.unwrap();
        assert!((d - frame.dew_point).abs() < 0.01, "dewpoint: {d}");
        // The lens probe is not an ObservingConditions property.
        assert_ne!(t, frame.lens_temperature);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn an_absent_ambient_sensor_reads_value_not_set() {
        let (device, _) = make_device_on(Arc::new(
            MockGeminiTransportFactory::default().with_sensors(false, true),
        ));
        device.set_connected(true).await.unwrap();
        for err in [
            device.temperature().await.unwrap_err(),
            device.humidity().await.unwrap_err(),
            device.dew_point().await.unwrap_err(),
        ] {
            assert_eq!(err.code, ASCOMErrorCode::VALUE_NOT_SET, "{err}");
            assert!(err.message.contains("AHT20"), "{}", err.message);
        }
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn sensor_descriptions_name_the_aht20_and_refuse_what_the_hub_lacks() {
        let device = connected_device().await;
        let t = device
            .sensor_description("temperature".to_string())
            .await
            .unwrap();
        assert!(t.contains("AHT20"), "{t}");
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
        let err = device.sensor_description(String::new()).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn refresh_asks_the_hub_for_a_fresh_frame() {
        let factory = Arc::new(MockGeminiTransportFactory::default());
        let (device, _) = make_device_on(dyn_factory(&factory));
        device.set_connected(true).await.unwrap();
        device.refresh().await.unwrap();
        assert_eq!(
            factory.sent_commands().await,
            [">H#", ">V#", ">G#", ">G#"],
            "one more status request, nothing else"
        );
        device.set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn set_connected_acquire_failure_maps_to_invalid_operation() {
        let (device, _) = make_device_on(Arc::new(FailingFactory));
        let err = device.set_connected(true).await.unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
        assert!(
            err.message.contains("mock factory error"),
            "{}",
            err.message
        );
        assert!(!device.connected().await.unwrap());
    }

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
}
