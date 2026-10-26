//! [`PlaceholderCamera`]: the Camera that holds a listed device number whose
//! camera cannot be served (the device-claims plan's D4.5).
//!
//! It reads `Connected == false`, refuses `Connected = true` with the
//! device-claims error code and the reason, carries the same reason in its
//! `Description`, answers to its fixed placeholder `UniqueID`, serves its
//! setup page connected or not, serves no action, as a camera serves none,
//! and answers every camera member `NOT_CONNECTED`. It never
//! re-scans: the reason is the one the last start or reload found, and only a
//! reload turns the number back into a camera.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ascom_alpaca::api::camera::{CameraState, GuideDirection, ImageArray, SensorType};
use ascom_alpaca::api::{Camera, Device};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult, SetupRequest, SetupResponse};
use http::StatusCode;
use rusty_photon_config::driver::ConfigurableDriver;
use rusty_photon_setup::SetupPages;
use tracing::debug;

/// What a placeholder says about itself.
///
/// The driver builds it from its own `usb_devices` entry and from
/// `rusty-photon-doctor-checks`' `claims` module, which owns the reason text,
/// the error code and the `UniqueID` form — this crate does not link that one,
/// whose `mock` twin replaces it in a simulation build's graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placeholder {
    /// The entry's `name`, else one naming the port.
    pub name: String,
    /// `placeholder:<service>:<usb_port>`.
    pub unique_id: String,
    /// The listed port, in the platform's native spelling.
    pub usb_port: String,
    /// Why no camera is served at the port's number, as of the last start or
    /// reload.
    pub reason: String,
    /// The driver's `DriverInfo`, which a placeholder answers as its camera
    /// would.
    pub driver_info: String,
    /// The driver's `DriverVersion`.
    pub driver_version: String,
    /// The driver-range code (`0x500` + this) a connect is refused with.
    pub driver_code: u16,
}

/// A registered Camera that stands in for a listed number's camera.
pub struct PlaceholderCamera<D: ConfigurableDriver> {
    about: Placeholder,
    /// The setup pages, and the device number its page is served at.
    pages: Option<(Arc<SetupPages<D>>, u32)>,
}

impl<D: ConfigurableDriver> PlaceholderCamera<D> {
    /// The placeholder `about` describes, with no setup page until
    /// [`with_setup_pages`](Self::with_setup_pages).
    #[must_use]
    pub const fn new(about: Placeholder) -> Self {
        Self { about, pages: None }
    }

    /// Serve its setup page, `/setup/v1/camera/<number>/setup`, through the
    /// shared pages: the port it stands in for, and why.
    #[must_use]
    pub fn with_setup_pages(mut self, pages: Arc<SetupPages<D>>, number: u32) -> Self {
        self.pages = Some((pages, number));
        self
    }

    /// The refusal every connect gets.
    fn refusal(&self) -> ASCOMError {
        ASCOMError::new(
            ASCOMErrorCode::new_for_driver(self.about.driver_code),
            format!(
                "{} is a placeholder for USB port {}, not a camera: {}",
                self.about.name, self.about.usb_port, self.about.reason
            ),
        )
    }
}

impl<D: ConfigurableDriver> fmt::Debug for PlaceholderCamera<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlaceholderCamera")
            .field("about", &self.about)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl<D> Device for PlaceholderCamera<D>
where
    D: ConfigurableDriver + Send + Sync + 'static,
    D::Config: Send + Sync,
    D::Overrides: Send + Sync,
{
    fn static_name(&self) -> &str {
        &self.about.name
    }

    fn unique_id(&self) -> &str {
        &self.about.unique_id
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        if !connected {
            return Ok(());
        }
        debug!(
            port = %self.about.usb_port,
            reason = %self.about.reason,
            "refusing to connect a placeholder"
        );
        Err(self.refusal())
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok(format!(
            "Placeholder for USB port {}: {}",
            self.about.usb_port, self.about.reason
        ))
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok(self.about.driver_info.clone())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(self.about.driver_version.clone())
    }

    /// No custom action, as a camera has none: the configuration is on the
    /// setup pages.
    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(Vec::new())
    }

    /// The specification's answer to an unknown action name.
    async fn action(&self, action: String, _parameters: String) -> ASCOMResult<String> {
        Err(ASCOMError::new(
            ASCOMErrorCode::ACTION_NOT_IMPLEMENTED,
            format!("this device has no action {action:?}"),
        ))
    }

    fn setup_request(
        &self,
        request: SetupRequest,
    ) -> Pin<Box<dyn Future<Output = SetupResponse> + Send + '_>> {
        match &self.pages {
            Some((pages, number)) => crate::device_setup_request(pages, "camera", *number, request),
            None => Box::pin(async {
                SetupResponse::text(
                    StatusCode::NOT_FOUND,
                    "This placeholder was built without its setup page",
                )
            }),
        }
    }
}

/// `Camera` with every member but `InterfaceVersion` answering
/// `NOT_CONNECTED`: a placeholder is a camera that is not connected and
/// cannot be, so even the members a disconnected camera answers from its own
/// constants have nothing to describe. Generated, because the list is the
/// whole `Camera` surface and every arm is the same.
macro_rules! not_connected_camera {
    ($($member:ident($($arg:ident: $ty:ty),*) -> $out:ty;)*) => {
        #[async_trait::async_trait]
        impl<D> Camera for PlaceholderCamera<D>
        where
            D: ConfigurableDriver + Send + Sync + 'static,
            D::Config: Send + Sync,
            D::Overrides: Send + Sync,
        {
            $(
                async fn $member(&self, $($arg: $ty),*) -> ASCOMResult<$out> {
                    Err(ASCOMError::NOT_CONNECTED)
                }
            )*
        }
    };
}

not_connected_camera! {
    bayer_offset_x() -> u8;
    bayer_offset_y() -> u8;
    bin_x() -> u8;
    set_bin_x(_bin_x: u8) -> ();
    bin_y() -> u8;
    set_bin_y(_bin_y: u8) -> ();
    camera_state() -> CameraState;
    camera_x_size() -> u32;
    camera_y_size() -> u32;
    can_abort_exposure() -> bool;
    can_asymmetric_bin() -> bool;
    can_fast_readout() -> bool;
    can_get_cooler_power() -> bool;
    can_pulse_guide() -> bool;
    can_set_ccd_temperature() -> bool;
    can_stop_exposure() -> bool;
    ccd_temperature() -> f64;
    cooler_on() -> bool;
    set_cooler_on(_cooler_on: bool) -> ();
    cooler_power() -> f64;
    electrons_per_adu() -> f64;
    exposure_max() -> Duration;
    exposure_min() -> Duration;
    exposure_resolution() -> Duration;
    fast_readout() -> bool;
    set_fast_readout(_fast_readout: bool) -> ();
    full_well_capacity() -> f64;
    gain() -> i32;
    set_gain(_gain: i32) -> ();
    gain_max() -> i32;
    gain_min() -> i32;
    gains() -> Vec<String>;
    has_shutter() -> bool;
    heat_sink_temperature() -> f64;
    image_array() -> ImageArray;
    image_ready() -> bool;
    is_pulse_guiding() -> bool;
    last_exposure_duration() -> Duration;
    last_exposure_start_time() -> SystemTime;
    max_adu() -> u32;
    max_bin_x() -> u8;
    max_bin_y() -> u8;
    num_x() -> u32;
    set_num_x(_num_x: u32) -> ();
    num_y() -> u32;
    set_num_y(_num_y: u32) -> ();
    offset() -> i32;
    set_offset(_offset: i32) -> ();
    offset_max() -> i32;
    offset_min() -> i32;
    offsets() -> Vec<String>;
    percent_completed() -> u8;
    pixel_size_x() -> f64;
    pixel_size_y() -> f64;
    readout_mode() -> usize;
    set_readout_mode(_readout_mode: usize) -> ();
    readout_modes() -> Vec<String>;
    sensor_name() -> String;
    sensor_type() -> SensorType;
    set_ccd_temperature() -> f64;
    set_set_ccd_temperature(_set_ccd_temperature: f64) -> ();
    start_x() -> u32;
    set_start_x(_start_x: u32) -> ();
    start_y() -> u32;
    set_start_y(_start_y: u32) -> ();
    sub_exposure_duration() -> f64;
    set_sub_exposure_duration(_sub_exposure_duration: f64) -> ();
    abort_exposure() -> ();
    pulse_guide(_direction: GuideDirection, _duration: Duration) -> ();
    start_exposure(_duration: Duration, _light: bool) -> ();
    stop_exposure() -> ();
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use rusty_photon_config::driver::FieldError;

    /// A driver with nothing to configure, to stand the placeholder up.
    struct NoConfig;

    #[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
    struct Empty {}

    impl ConfigurableDriver for NoConfig {
        type Config = Empty;
        type Overrides = ();

        fn normalize(_config: &mut Empty) {}

        fn validate(_config: &Empty) -> Vec<FieldError> {
            Vec::new()
        }

        fn secret_pointers() -> &'static [&'static str] {
            &[]
        }

        fn override_paths(_overrides: &()) -> Vec<String> {
            Vec::new()
        }

        fn apply_overrides(_config: &mut Empty, _overrides: &()) {}
    }

    fn placeholder() -> PlaceholderCamera<NoConfig> {
        PlaceholderCamera::new(Placeholder {
            name: "ZWO camera on pci-0000:00:14.0-usbv3-0:4.2 (placeholder)".to_string(),
            unique_id: "placeholder:zwo-camera:pci-0000:00:14.0-usbv3-0:4.2".to_string(),
            usb_port: "pci-0000:00:14.0-usbv3-0:4.2".to_string(),
            reason: "the USB scan failed: timeout".to_string(),
            driver_info: "rusty-photon zwo-camera".to_string(),
            driver_version: "0.1.0".to_string(),
            driver_code: 0x40,
        })
    }

    #[test]
    fn it_answers_to_its_name_and_unique_id() {
        let camera = placeholder();
        assert_eq!(
            camera.static_name(),
            "ZWO camera on pci-0000:00:14.0-usbv3-0:4.2 (placeholder)"
        );
        assert_eq!(
            camera.unique_id(),
            "placeholder:zwo-camera:pci-0000:00:14.0-usbv3-0:4.2"
        );
    }

    #[tokio::test]
    async fn it_reads_disconnected() {
        assert!(!placeholder().connected().await.unwrap());
    }

    #[tokio::test]
    async fn a_connect_is_refused_with_the_driver_code_and_the_reason() {
        let error = placeholder().set_connected(true).await.unwrap_err();
        assert_eq!(error.code.raw(), 0x540);
        assert_eq!(
            error.message,
            "ZWO camera on pci-0000:00:14.0-usbv3-0:4.2 (placeholder) is a placeholder for USB \
             port pci-0000:00:14.0-usbv3-0:4.2, not a camera: the USB scan failed: timeout"
        );
    }

    #[tokio::test]
    async fn a_disconnect_succeeds() {
        placeholder().set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn its_description_carries_the_reason() {
        assert_eq!(
            placeholder().description().await.unwrap(),
            "Placeholder for USB port pci-0000:00:14.0-usbv3-0:4.2: the USB scan failed: timeout"
        );
    }

    #[tokio::test]
    async fn it_answers_driver_info_and_version_as_its_camera_would() {
        let camera = placeholder();
        assert_eq!(
            camera.driver_info().await.unwrap(),
            "rusty-photon zwo-camera"
        );
        assert_eq!(camera.driver_version().await.unwrap(), "0.1.0");
    }

    #[tokio::test]
    async fn it_offers_no_action_and_answers_any_as_not_implemented() {
        let camera = placeholder();
        assert_eq!(
            camera.supported_actions().await.unwrap(),
            Vec::<String>::new()
        );
        let error = camera
            .action("config.get".to_string(), String::new())
            .await
            .unwrap_err();
        assert_eq!(error.code, ASCOMErrorCode::ACTION_NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn camera_members_answer_not_connected() {
        let camera = placeholder();
        let codes = [
            camera.camera_x_size().await.unwrap_err().code,
            camera.can_abort_exposure().await.unwrap_err().code,
            camera.has_shutter().await.unwrap_err().code,
            camera.ccd_temperature().await.unwrap_err().code,
            camera
                .start_exposure(Duration::from_secs(1), true)
                .await
                .unwrap_err()
                .code,
        ];
        assert!(
            codes.iter().all(|c| *c == ASCOMErrorCode::NOT_CONNECTED),
            "{codes:?}"
        );
    }

    #[tokio::test]
    async fn it_reports_interface_version_4_like_every_camera() {
        assert_eq!(placeholder().interface_version().await.unwrap(), 4);
    }
}
