//! PPBA `ObservingConditions` device implementation.
//!
//! Like the Switch device, this holds an `Option<Session<PpbaCodec>>` —
//! the session existing is the canonical "Connected" state.

use std::sync::Arc;

use ascom_alpaca::api::{Device, ObservingConditions};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult, SetupRequest, SetupResponse};
use async_trait::async_trait;
use rusty_photon_rolling_stats::observing_conditions::{
    average_period_hours, time_since_freshest_update,
};
use rusty_photon_setup::SetupPages;
use rusty_photon_shared_transport::Session;
use tokio::sync::RwLock;
use tracing::debug;

use crate::codec::PpbaCodec;
use crate::config::ObservingConditionsConfig;
use crate::error::PpbaError;
use crate::manager::PpbaManager;
use crate::setup::{PpbaDriver, OBSERVING_CONDITIONS_DEVICE_TYPE};

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
    /// The setup pages behind `/setup/v1/observingconditions/0/setup`: `Some` when
    /// the driver was built with a config source (the normal path through
    /// `ServerBuilder`); `None` for focused unit-test devices that don't
    /// exercise the page.
    #[debug(skip)]
    pages: Option<Arc<SetupPages<PpbaDriver>>>,
}

impl PpbaObservingConditionsDevice {
    #[must_use]
    pub fn new(config: ObservingConditionsConfig, manager: Arc<PpbaManager>) -> Self {
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
    pub fn with_setup_pages(mut self, pages: Arc<SetupPages<PpbaDriver>>) -> Self {
        self.pages = Some(pages);
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

    /// No actions: the configuration lives on the setup pages. An unknown
    /// action is `ActionNotImplemented`, not the generic `NotImplemented`
    /// the crate defaults to.
    async fn action(&self, action: String, _parameters: String) -> ASCOMResult<String> {
        Err(ASCOMError::new(
            ASCOMErrorCode::ACTION_NOT_IMPLEMENTED,
            format!("this device has no action {action:?}"),
        ))
    }

    /// The device's setup page, `/setup/v1/observingconditions/0/setup`, for `GET`
    /// and `POST`, through the shared handler. A device built without the
    /// pages (a focused unit test) has none.
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
impl ObservingConditions for PpbaObservingConditionsDevice {
    async fn average_period(&self) -> ASCOMResult<f64> {
        ensure_connected!(self);
        Ok(self.manager.get_cached_state().await.average_period_hours)
    }

    async fn set_average_period(&self, period: f64) -> ASCOMResult<()> {
        ensure_connected!(self);
        // A range check, not a pair of ordered comparisons: every ordered
        // comparison against NaN is false, so a `period < low || period > high`
        // form accepts NaN and passes it to the manager's seconds-to-`Duration`
        // conversion, which panics on a non-finite value. `contains` is false
        // for NaN, so this rejects it. The range comes from the manager, which
        // enforces the same bound for callers that bypass the device.
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
            // The freshest window; `None` while all three are empty, so the
            // client reads "no data" rather than "updated just now".
            "" => time_since_freshest_update([
                &state.temp_mean,
                &state.humidity_mean,
                &state.dewpoint_mean,
            ]),
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
    use crate::mock::MockPpbaTransportFactory;
    use ascom_alpaca::ASCOMErrorCode;
    use async_trait::async_trait;
    use rusty_photon_rolling_stats::observing_conditions::instantaneous_window;
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
    async fn set_average_period_nan_is_invalid_value() {
        // NaN compares false against both range bounds, so any check built
        // from ordered comparisons accepts it and hands it to the manager's
        // `Duration` conversion, which panics on a non-finite value.
        let device = connected_device().await;
        let err = device.set_average_period(f64::NAN).await.unwrap_err();
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

    // --- Setup pages ------------------------------------------------------

    fn get_request() -> SetupRequest {
        SetupRequest {
            method: axum::http::Method::GET,
            headers: axum::http::HeaderMap::new(),
            query: Vec::new(),
            form: Vec::new(),
        }
    }

    #[tokio::test]
    async fn the_config_actions_are_gone() {
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
    }

    #[tokio::test]
    async fn a_device_without_pages_has_no_setup_page() {
        let device = make_device();
        let response = device.setup_request(get_request()).await;
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }

    // PpbaError → ASCOMError mapping tests moved to error.rs once the
    // canonical mapping landed there (centralised so both devices share
    // the same classification).
}
