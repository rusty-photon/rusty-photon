// Manual hardware-probe binary (requires a physical camera) — excluded from coverage.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![allow(non_snake_case)]
use qhyccd_rs::{ControlType, Sdk, StreamMode};
use tracing::{error, trace};
use tracing_subscriber::FmtSubscriber;

fn main() {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::TRACE)
        .with_test_writer()
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let sdk = Sdk::new().expect("SDK::new failed");
    let sdk_version = sdk.version().expect("get_sdk_version failed");
    trace!(sdk_version = ?sdk_version);
    trace!(cameras = ?sdk.cameras().count());
    trace!(filter_wheels = ?sdk.filter_wheels().count());

    let camera = sdk.cameras().last().expect("no camera found");
    trace!(camera = ?camera);

    camera.open().expect("opening camera failed");

    let fw_version = camera
        .get_firmware_version()
        .expect("get_firmware_version failed");
    trace!(fw_version = ?fw_version);

    if camera
        .is_control_available(ControlType::CamSingleFrameMode)
        .is_none()
    {
        panic!("CameraFeature::CamLiveVideoMode is not supported");
    }
    trace!("CameraFeature::CamSingleFrameMode is supported");

    camera
        .set_stream_mode(StreamMode::SingleFrameMode)
        .expect("set_camera_stream_mode failed");
    trace!(set_camera_stream_mode = ?StreamMode::SingleFrameMode);

    camera
        .set_readout_mode(0)
        .expect("set_camera_read_mode failed");
    trace!(set_camera_read_mode = 0);

    camera.init().expect("init_camera failed");

    let over_scan_area = camera
        .get_overscan_area()
        .expect("get_camera_overscan_area failed");
    trace!(over_scan_area = ?over_scan_area);

    let effective_area = camera
        .get_effective_area()
        .expect("get_camera_effective_area failed");
    trace!(effective_area = ?effective_area);

    let info = camera.get_ccd_info().expect("get_camera_ccd_info failed");
    trace!(ccd_info = ?info);

    let bayer_id = match camera.is_control_available(ControlType::CamIsColor) {
        Some(camera_is_color) => {
            trace!(camera_is_color = ?camera_is_color);
            //camera.set_debayer(true).expect("set debayer true failed"); -- this core-dumps on
            //QHY290C
            camera.is_control_available(ControlType::CamColor)
        }
        None => None,
    };
    trace!(bayer_id = ?bayer_id);

    match camera.set_if_available(ControlType::UsbTraffic, 255.0) {
        Ok(()) => trace!(control_usb_traffic = 255.0),
        Err(_) => {
            error!("ControlUsbTraffic is not supported");
            return;
        }
    }

    match camera.set_if_available(ControlType::Gain, 10.0) {
        Ok(()) => trace!(control_gain = 10),
        Err(_) => {
            error!("ControlGain is not supported");
            return;
        }
    }

    match camera.set_if_available(ControlType::Offset, 140.0) {
        Ok(()) => trace!(control_offset = 140),
        Err(_) => {
            error!("ControlOffset is not supported");
            return;
        }
    }

    camera
        .set_parameter(ControlType::Exposure, 2000.0)
        .expect("setting exposure time failed");
    trace!(exposure_time = 2000.0);

    camera
        .set_roi(effective_area)
        .expect("set_camera_roi failed");
    trace!(roi = ?effective_area);

    camera
        .set_bin_mode(1, 1)
        .expect("set_camera_bin_mode failed");
    trace!(bin_mode = "(1, 1)");

    match camera.set_if_available(ControlType::TransferBit, 16.0) {
        Ok(()) => trace!(cam_transfer_bit = 16.0),
        Err(_) => {
            error!("setting transfer bits is not supported");
            return;
        }
    }

    trace!("beginning single frame capture");
    camera
        .start_single_frame_exposure()
        .expect("start_camera_single_frame_exposure failed");

    let buffer_size = camera
        .get_image_size()
        .expect("get_camera_image_size failed");

    let mut buf = vec![0u8; buffer_size];
    let info = camera
        .get_single_frame(&mut buf)
        .expect("get_camera_single_frame failed");
    trace!(frame = ?info);
}
