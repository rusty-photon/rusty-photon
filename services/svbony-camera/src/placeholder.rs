//! `PlaceholderCamera`: the Camera that holds a listed device number whose
//! camera cannot be served (docs/services/svbony-camera.md U4).
//!
//! It reads `Connected == false`, refuses `Connected = true` with the
//! device-claims error code and the reason, carries the same reason in its
//! `Description`, answers to `placeholder:svbony-camera:<usb_port>`, serves the
//! config actions connected or not, and answers every camera member
//! `NOT_CONNECTED`. It never re-scans: the reason is the one the last start or
//! reload found, and only a reload turns the number back into a camera.

use std::time::{Duration, SystemTime};

use ascom_alpaca::api::camera::{CameraState, GuideDirection, ImageArray, SensorType};
use ascom_alpaca::api::{Camera, Device};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use rusty_photon_doctor_checks::claims;
use rusty_photon_driver::ConfigActionCtx;
use tracing::debug;

use crate::claims::{SDK_NAME, SERVICE};
use crate::config::UsbDeviceEntry;
use crate::config_actions::SvbonyCameraDriver;

/// The error a placeholder's `Connected = true` fails with — the same code in
/// every camera driver with device claims.
const PLACEHOLDER_CODE: ASCOMErrorCode =
    ASCOMErrorCode::new_for_driver(claims::PLACEHOLDER_DRIVER_CODE);

/// A registered Camera that stands in for a listed number's camera.
#[derive(derive_more::Debug)]
pub struct PlaceholderCamera {
    name: String,
    unique_id: String,
    usb_port: String,
    reason: String,
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<SvbonyCameraDriver>>,
}

impl PlaceholderCamera {
    /// The placeholder for `entry`, holding its number for `reason`.
    #[must_use]
    pub fn new(entry: &UsbDeviceEntry, reason: String) -> Self {
        Self {
            name: entry.name.clone().unwrap_or_else(|| {
                format!("{SDK_NAME} camera on {} (placeholder)", entry.usb_port)
            }),
            unique_id: claims::placeholder_unique_id(SERVICE, &entry.usb_port),
            usb_port: entry.usb_port.clone(),
            reason,
            config_ctx: None,
        }
    }

    /// Serve the config actions, as a camera at this number would.
    #[must_use]
    pub fn with_config_actions(mut self, ctx: ConfigActionCtx<SvbonyCameraDriver>) -> Self {
        self.config_ctx = Some(ctx);
        self
    }

    /// The refusal every connect gets.
    fn refusal(&self) -> ASCOMError {
        ASCOMError::new(
            PLACEHOLDER_CODE,
            format!(
                "{} is a placeholder for USB port {}, not a camera: {}",
                self.name, self.usb_port, self.reason
            ),
        )
    }
}

#[async_trait::async_trait]
impl Device for PlaceholderCamera {
    fn static_name(&self) -> &str {
        &self.name
    }

    fn unique_id(&self) -> &str {
        &self.unique_id
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        if !connected {
            return Ok(());
        }
        debug!(port = %self.usb_port, reason = %self.reason, "refusing to connect a placeholder");
        Err(self.refusal())
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok(format!(
            "Placeholder for USB port {}: {}",
            self.usb_port, self.reason
        ))
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("rusty-photon svbony-camera".to_string())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }

    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(rusty_photon_driver::supported_actions(&self.config_ctx))
    }

    async fn action(&self, action: String, parameters: String) -> ASCOMResult<String> {
        rusty_photon_driver::dispatch::<SvbonyCameraDriver>(&self.config_ctx, action, parameters)
            .await
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
        impl Camera for PlaceholderCamera {
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

    fn entry(name: Option<&str>) -> UsbDeviceEntry {
        UsbDeviceEntry {
            device_number: 0,
            usb_port: "pci-0000:00:14.0-usbv3-0:4.2".to_string(),
            name: name.map(str::to_string),
            description: None,
        }
    }

    fn placeholder() -> PlaceholderCamera {
        PlaceholderCamera::new(&entry(None), "the USB scan failed: timeout".to_string())
    }

    #[test]
    fn its_error_code_is_the_device_claims_code() {
        assert_eq!(PLACEHOLDER_CODE.raw(), claims::PLACEHOLDER_ERROR_CODE);
    }

    #[test]
    fn it_answers_to_the_placeholder_unique_id_of_its_port() {
        assert_eq!(
            placeholder().unique_id(),
            "placeholder:svbony-camera:pci-0000:00:14.0-usbv3-0:4.2"
        );
    }

    #[test]
    fn its_name_names_the_port_unless_the_entry_names_it() {
        assert_eq!(
            placeholder().static_name(),
            "SVBony camera on pci-0000:00:14.0-usbv3-0:4.2 (placeholder)"
        );
        let named = PlaceholderCamera::new(&entry(Some("Main")), String::new());
        assert_eq!(named.static_name(), "Main");
    }

    #[tokio::test]
    async fn it_reads_disconnected() {
        assert!(!placeholder().connected().await.unwrap());
    }

    #[tokio::test]
    async fn a_connect_is_refused_with_the_code_and_the_reason() {
        let error = placeholder().set_connected(true).await.unwrap_err();
        assert_eq!(error.code.raw(), claims::PLACEHOLDER_ERROR_CODE);
        assert!(
            error.message.contains("the USB scan failed: timeout"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_disconnect_succeeds() {
        placeholder().set_connected(false).await.unwrap();
    }

    #[tokio::test]
    async fn its_description_carries_the_reason() {
        let description = placeholder().description().await.unwrap();
        assert_eq!(
            description,
            "Placeholder for USB port pci-0000:00:14.0-usbv3-0:4.2: the USB scan failed: timeout"
        );
    }

    #[tokio::test]
    async fn without_a_config_source_it_offers_no_actions() {
        assert_eq!(
            placeholder().supported_actions().await.unwrap(),
            Vec::<String>::new()
        );
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
