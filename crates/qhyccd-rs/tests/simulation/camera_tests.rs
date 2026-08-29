//! Integration tests for simulated cameras
//!
//! These tests verify that simulated cameras work correctly without
//! requiring actual QHYCCD hardware.

use qhyccd_rs::simulation::{ImageGenerator, ImagePattern, SimulatedCameraConfig};
use qhyccd_rs::{
    BayerPattern, CCDChipArea, Camera, ControlType, FilterWheel, FrameInfo, QHYError, SDKVersion,
    Sdk, StreamMode,
};

/// Bytes in a frame with the given [`FrameInfo`] — mirrors the simulated
/// backend's `calculate_buffer_size` (`width * height * channels *
/// bytes_per_pixel`, 1 byte per pixel at ≤8-bit, 2 otherwise). Since the download
/// now writes into a caller-owned buffer and returns only `FrameInfo`, assertions
/// use this to check the frame the download *reports*, not the caller's
/// fixed-length buffer (whose `len()` is always `get_image_size()`).
fn frame_bytes(info: &FrameInfo) -> usize {
    let bytes_per_pixel = if info.bits_per_pixel <= 8 { 1 } else { 2 };
    (info.width * info.height * info.channels) as usize * bytes_per_pixel
}

/// Poll the simulated filter wheel to arrival — the move settles over a few polls
/// (advance-on-poll), so a single read after a command still reports the old slot.
fn cfw_arrive(camera: &Camera, target: u32) {
    for _ in 0..10 {
        if camera.cfw_position().unwrap() == target {
            return;
        }
    }
    panic!("filter wheel did not reach slot {target} within 10 polls");
}

/// Poll a crate [`FilterWheel`] to arrival (the move settles over a few polls).
fn fw_arrive(fw: &FilterWheel, target: u32) {
    for _ in 0..10 {
        if fw.get_fw_position().unwrap() == target {
            return;
        }
    }
    panic!("filter wheel did not reach slot {target} within 10 polls");
}

#[test]
fn test_simulated_camera_creation() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    assert!(camera.is_simulated());
    assert_eq!(camera.id(), "SIM-001");
}

#[test]
fn test_simulated_camera_open_close() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // Initially not open
    assert!(!camera.is_open().unwrap());

    // Open the camera
    camera.open().unwrap();
    assert!(camera.is_open().unwrap());

    // Close the camera
    camera.close().unwrap();
    assert!(!camera.is_open().unwrap());
}

#[test]
fn test_simulated_camera_info() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    let model = camera.get_model().unwrap();
    assert_eq!(model, "QHY-SIMULATED");

    let chip_info = camera.get_ccd_info().unwrap();
    assert_eq!(chip_info.image_width, 3072);
    assert_eq!(chip_info.image_height, 2048);

    let camera_type = camera.get_type().unwrap();
    assert_eq!(camera_type, 4010);

    camera.close().unwrap();
}

#[test]
fn test_simulated_camera_readout_modes() {
    let config = SimulatedCameraConfig::default()
        .with_readout_mode("High Speed", 3072, 2048)
        .with_readout_mode("Low Noise", 1536, 1024);
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    let num_modes = camera.get_number_of_readout_modes().unwrap();
    assert_eq!(num_modes, 3); // default "Standard" + 2 custom

    let name = camera.get_readout_mode_name(0).unwrap();
    assert_eq!(name, "Standard");

    let (width, height) = camera.get_readout_mode_resolution(1).unwrap();
    assert_eq!(width, 3072);
    assert_eq!(height, 2048);

    camera.close().unwrap();
}

#[test]
fn test_simulated_camera_parameters() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // Set and get exposure
    camera.set_parameter(ControlType::Exposure, 1000.0).unwrap();
    let exposure = camera.get_parameter(ControlType::Exposure).unwrap();
    assert!((exposure - 1000.0).abs() < f64::EPSILON);

    // Set and get gain
    camera.set_parameter(ControlType::Gain, 50.0).unwrap();
    let gain = camera.get_parameter(ControlType::Gain).unwrap();
    assert!((gain - 50.0).abs() < f64::EPSILON);

    // Get parameter min/max/step
    let (min, max, step) = camera
        .get_parameter_min_max_step(ControlType::Gain)
        .unwrap();
    assert!((min - 0.0).abs() < f64::EPSILON);
    assert!((max - 100.0).abs() < f64::EPSILON);
    assert!((step - 1.0).abs() < f64::EPSILON);

    camera.close().unwrap();
}

#[test]
fn test_simulated_camera_is_control_available() {
    let config = SimulatedCameraConfig::default().with_cooler();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // Default controls should be available
    assert!(camera.is_control_available(ControlType::Gain).is_some());
    assert!(camera.is_control_available(ControlType::Exposure).is_some());

    // Cooler controls should be available
    assert!(camera.is_control_available(ControlType::Cooler).is_some());
    assert!(camera.is_control_available(ControlType::CurTemp).is_some());

    // CFW controls should NOT be available (no filter wheel)
    assert!(camera.is_control_available(ControlType::CfwPort).is_none());

    camera.close().unwrap();
}

#[test]
fn test_simulated_camera_with_filter_wheel() {
    let config = SimulatedCameraConfig::default().with_filter_wheel(5);
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // CFW controls should be available
    assert!(camera.is_control_available(ControlType::CfwPort).is_some());
    assert!(camera
        .is_control_available(ControlType::CfwSlotsNum)
        .is_some());

    // Check filter wheel plugged in
    assert!(camera.is_cfw_plugged_in().unwrap());

    // Get number of slots
    let slots = camera.get_parameter(ControlType::CfwSlotsNum).unwrap();
    assert!((slots - 5.0).abs() < f64::EPSILON);

    camera.close().unwrap();
}

#[test]
fn test_simulated_camera_single_frame_mode() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    camera.set_stream_mode(StreamMode::SingleFrameMode).unwrap();
    camera.init().unwrap();
    camera.set_parameter(ControlType::Exposure, 1000.0).unwrap(); // 1ms

    let buffer_size = camera.get_image_size().unwrap();
    assert!(buffer_size > 0);

    camera.start_single_frame_exposure().unwrap();
    let mut buf = vec![0u8; buffer_size];
    let image = camera.get_single_frame(&mut buf).unwrap();

    assert_eq!(image.width, 3072);
    assert_eq!(image.height, 2048);
    assert_eq!(image.bits_per_pixel, 16);
    // the reported frame exactly fills the buffer we sized from get_image_size
    assert_eq!(frame_bytes(&image), buffer_size);

    camera.close().unwrap();
}

#[test]
fn test_simulated_camera_live_mode() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    camera.set_stream_mode(StreamMode::LiveMode).unwrap();
    camera.init().unwrap();
    camera.begin_live().unwrap();

    let buffer_size = camera.get_image_size().unwrap();
    let mut buf = vec![0u8; buffer_size];
    let image = camera.get_live_frame(&mut buf).unwrap();

    assert_eq!(image.width, 3072);
    assert_eq!(image.height, 2048);
    assert_eq!(frame_bytes(&image), buffer_size);

    camera.end_live().unwrap();
    camera.close().unwrap();
}

#[test]
fn test_simulated_camera_binning() {
    // This test verifies that setting binning alone does NOT automatically
    // change image dimensions. This matches real QHYCCD SDK behavior and
    // ensures we don't have the double-binning bug where ROI dimensions
    // (which may already be in binned coordinates from ASCOM Alpaca)
    // get binned again by the simulation.
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();
    camera.set_stream_mode(StreamMode::LiveMode).unwrap();
    camera.init().unwrap();

    // Get baseline dimensions with 1x1 binning
    camera.begin_live().unwrap();
    let buffer_size = camera.get_image_size().unwrap();
    let mut buf = vec![0u8; buffer_size];
    let image = camera.get_live_frame(&mut buf).unwrap();
    assert_eq!(image.width, 3072);
    assert_eq!(image.height, 2048);
    camera.end_live().unwrap();

    // Set 2x2 binning WITHOUT changing ROI
    camera.set_bin_mode(2, 2).unwrap();

    camera.begin_live().unwrap();
    let buffer_size = camera.get_image_size().unwrap();
    let mut buf = vec![0u8; buffer_size];
    let image = camera.get_live_frame(&mut buf).unwrap();

    // Dimensions should NOT change - binning alone doesn't affect ROI.
    // This proves we fixed the double-binning bug.
    assert_eq!(image.width, 3072);
    assert_eq!(image.height, 2048);

    camera.end_live().unwrap();
    camera.close().unwrap();
}

#[test]
fn test_simulated_camera_bit_mode() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();
    camera.set_stream_mode(StreamMode::LiveMode).unwrap();
    camera.init().unwrap();

    // Set 8-bit mode
    camera.set_bit_mode(8).unwrap();
    camera.begin_live().unwrap();

    let buffer_size = camera.get_image_size().unwrap();
    let mut buf = vec![0u8; buffer_size];
    let image = camera.get_live_frame(&mut buf).unwrap();

    assert_eq!(image.bits_per_pixel, 8);
    // 8-bit mode uses 1 byte per pixel: the reported frame is exactly W*H bytes
    assert_eq!(frame_bytes(&image), (3072 * 2048) as usize);

    camera.end_live().unwrap();
    camera.close().unwrap();
}

#[test]
fn test_simulated_sdk_new_simulated() {
    let sdk = Sdk::new_simulated();
    assert_eq!(sdk.cameras().count(), 0);
    assert_eq!(sdk.filter_wheels().count(), 0);
}

#[test]
fn test_simulated_sdk_add_camera() {
    let mut sdk = Sdk::new_simulated();

    let config = SimulatedCameraConfig::default().with_id("TEST-001");
    sdk.add_simulated_camera(config);

    assert_eq!(sdk.cameras().count(), 1);

    let camera = sdk.cameras().next().unwrap();
    assert_eq!(camera.id(), "TEST-001");
    assert!(camera.is_simulated());
}

#[test]
fn test_sdk_new_with_default_simulated_camera() {
    // Test that Sdk::new() automatically creates a simulated SDK with a default camera
    let sdk = Sdk::new().expect("Failed to create SDK");

    // Should have exactly 1 camera
    assert_eq!(sdk.cameras().count(), 1);

    let camera = sdk.cameras().next().unwrap();
    assert_eq!(camera.id(), "SIM-QHY178M");
    assert!(camera.is_simulated());

    // Should have exactly 1 filter wheel (7 positions)
    assert_eq!(sdk.filter_wheels().count(), 1);

    let filter_wheel = sdk.filter_wheels().next().unwrap();

    // Open the camera to verify it has cooler support
    camera.open().expect("Failed to open camera");

    // Verify camera has cooler support
    assert!(
        camera.is_control_available(ControlType::Cooler).is_some(),
        "Camera should have cooler support"
    );

    camera.close().expect("Failed to close camera");

    // Open and verify filter wheel has 7 positions
    filter_wheel.open().expect("Failed to open filter wheel");
    let num_filters = filter_wheel
        .get_number_of_filters()
        .expect("Failed to get number of filters");
    assert_eq!(num_filters, 7, "Filter wheel should have 7 positions");
    filter_wheel.close().expect("Failed to close filter wheel");
}

#[test]
fn test_simulated_sdk_add_camera_with_filter_wheel() {
    let mut sdk = Sdk::new_simulated();

    let config = SimulatedCameraConfig::default()
        .with_id("CAM-WITH-FW")
        .with_filter_wheel(7);
    sdk.add_simulated_camera(config);

    assert_eq!(sdk.cameras().count(), 1);
    assert_eq!(sdk.filter_wheels().count(), 1);

    let fw = sdk.filter_wheels().next().unwrap();
    assert_eq!(fw.id(), "CAM-WITH-FW");
}

#[test]
fn test_simulated_filter_wheel() {
    let config = SimulatedCameraConfig::default()
        .with_id("FW-TEST")
        .with_filter_wheel(5);
    let camera = Camera::new_simulated(config);
    let fw = FilterWheel::new(camera);

    fw.open().unwrap();
    assert!(fw.is_open().unwrap());
    assert!(fw.is_cfw_plugged_in().unwrap());

    let num_filters = fw.get_number_of_filters().unwrap();
    assert_eq!(num_filters, 5);

    // Get initial position
    let pos = fw.get_fw_position().unwrap();
    assert_eq!(pos, 0);

    // Set position; the move settles over a few polls (poll-to-arrival).
    fw.set_fw_position(3).unwrap();
    fw_arrive(&fw, 3);

    fw.close().unwrap();
}

#[test]
fn test_sdk_shares_one_handle_between_camera_and_filter_wheel() {
    // Phase 1(B): the SDK builds the filter wheel from a CLONE of the camera, so
    // both share ONE backend state (a real QHY CFW is driven through the camera
    // handle). Opening the camera therefore also opens the wheel, and the wheel
    // position is one shared value visible through the camera's CFW control.
    let mut sdk = Sdk::new_simulated();
    sdk.add_simulated_camera(
        SimulatedCameraConfig::default()
            .with_id("SHARED-CAM")
            .with_filter_wheel(7),
    );

    let camera = sdk.cameras().next().unwrap();
    let wheel = sdk.filter_wheels().next().unwrap();
    // The wheel delegates its id to the shared camera.
    assert_eq!(wheel.id(), "SHARED-CAM");

    // Opening the camera opens the shared handle the wheel reads through.
    camera.open().unwrap();
    assert!(wheel.is_open().unwrap());
    assert_eq!(wheel.get_number_of_filters().unwrap(), 7);

    // A position set on the wheel is visible through the camera's CFW control
    // (ASCII-offset by 48), proving the single shared state.
    wheel.set_fw_position(3).unwrap();
    fw_arrive(wheel, 3);
    assert_eq!(
        camera.get_parameter(ControlType::CfwPort).unwrap() as u32,
        3 + 48
    );

    // Closing the camera closes the wheel (shared open state).
    camera.close().unwrap();
    assert!(!wheel.is_open().unwrap());
}

#[test]
fn test_simulated_color_camera() {
    let config = SimulatedCameraConfig::default().with_color(BayerPattern::RGGB);
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // Check color mode is available
    let bayer = camera.is_control_available(ControlType::CamColor);
    assert!(bayer.is_some());
    assert_eq!(bayer.unwrap(), BayerPattern::RGGB as u32);

    camera.close().unwrap();
}

#[test]
fn test_image_generator_gradient() {
    let gen = ImageGenerator::default();
    let data = gen.generate_16bit(100, 100, 1);
    assert_eq!(data.len(), 20000); // 100 * 100 * 2 bytes

    // Verify gradient - right side should be brighter than left
    let left_pixel = u16::from_le_bytes([data[0], data[1]]);
    let right_pixel = u16::from_le_bytes([data[198], data[199]]);
    assert!(right_pixel > left_pixel);
}

#[test]
fn test_image_generator_starfield() {
    let gen = ImageGenerator::new(ImagePattern::StarField);
    let data = gen.generate_16bit(200, 200, 1);
    assert_eq!(data.len(), 80000);

    // Starfield should have some variation (stars)
    let mut min_val = u16::MAX;
    let mut max_val = u16::MIN;
    for i in (0..data.len()).step_by(2) {
        let val = u16::from_le_bytes([data[i], data[i + 1]]);
        min_val = min_val.min(val);
        max_val = max_val.max(val);
    }
    // Should have significant contrast between background and stars
    assert!(max_val - min_val > 10000);
}

#[test]
fn test_set_readout_mode() {
    let config = SimulatedCameraConfig::default()
        .with_readout_mode("High Speed", 3072, 2048)
        .with_readout_mode("Low Noise", 1536, 1024);
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // Set to mode 1 (High Speed)
    camera.set_readout_mode(1).unwrap();

    // Verify we can get mode resolution for mode 1
    let (width, height) = camera.get_readout_mode_resolution(1).unwrap();
    assert_eq!(width, 3072);
    assert_eq!(height, 2048);

    // Set to mode 2 (Low Noise)
    camera.set_readout_mode(2).unwrap();

    // Test invalid mode - should fail
    let result = camera.set_readout_mode(99);
    assert!(result.is_err());

    camera.close().unwrap();
}

#[test]
fn test_get_firmware_version() {
    let config =
        SimulatedCameraConfig::default().with_firmware_version("Firmware version: 2025_3_15");
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    let version = camera.get_firmware_version().unwrap();
    assert_eq!(version, "Firmware version: 2025_3_15");

    camera.close().unwrap();
}

#[test]
fn test_set_roi() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();
    camera.set_stream_mode(StreamMode::LiveMode).unwrap();
    camera.init().unwrap();

    // Set a custom ROI
    let roi = CCDChipArea {
        start_x: 100,
        start_y: 100,
        width: 1000,
        height: 800,
    };
    camera.set_roi(roi).unwrap();

    // Begin live mode and capture a frame
    camera.begin_live().unwrap();

    let buffer_size = camera.get_image_size().unwrap();
    let mut buf = vec![0u8; buffer_size];
    let image = camera.get_live_frame(&mut buf).unwrap();

    // Image dimensions should match ROI
    assert_eq!(image.width, 1000);
    assert_eq!(image.height, 800);

    camera.end_live().unwrap();
    camera.close().unwrap();
}

#[test]
fn test_get_effective_area() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    let effective_area = camera.get_effective_area().unwrap();

    // Default config effective area
    assert_eq!(effective_area.start_x, 0);
    assert_eq!(effective_area.start_y, 0);
    assert_eq!(effective_area.width, 3072);
    assert_eq!(effective_area.height, 2048);

    camera.close().unwrap();
}

#[test]
fn test_get_overscan_area() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    let overscan_area = camera.get_overscan_area().unwrap();

    // Default config overscan is a distinct strip (not the full effective area).
    assert_eq!(overscan_area.start_x, 0);
    assert_eq!(overscan_area.start_y, 0);
    assert_eq!(overscan_area.width, 24);
    assert_eq!(overscan_area.height, 2048);

    camera.close().unwrap();
}

#[test]
fn test_stop_exposure() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();
    camera.set_stream_mode(StreamMode::SingleFrameMode).unwrap();
    camera.init().unwrap();

    // Set a long exposure
    camera
        .set_parameter(ControlType::Exposure, 10_000_000.0)
        .unwrap(); // 10 seconds

    camera.start_single_frame_exposure().unwrap();

    // Exposure should be in progress
    let remaining = camera.get_remaining_exposure_us().unwrap();
    assert!(remaining > 0);

    // Stop the exposure
    camera.stop_exposure().unwrap();

    // Remaining time should be 0 after stopping
    let remaining_after = camera.get_remaining_exposure_us().unwrap();
    assert_eq!(remaining_after, 0);

    camera.close().unwrap();
}

#[test]
fn test_abort_exposure_and_readout() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();
    camera.set_stream_mode(StreamMode::SingleFrameMode).unwrap();
    camera.init().unwrap();

    // Set a long exposure
    camera
        .set_parameter(ControlType::Exposure, 10_000_000.0)
        .unwrap(); // 10 seconds

    camera.start_single_frame_exposure().unwrap();

    // Exposure should be in progress
    let remaining = camera.get_remaining_exposure_us().unwrap();
    assert!(remaining > 0);

    // Abort the exposure and readout
    camera.abort_exposure_and_readout().unwrap();

    // Remaining time should be 0 after aborting
    let remaining_after = camera.get_remaining_exposure_us().unwrap();
    assert_eq!(remaining_after, 0);

    camera.close().unwrap();
}

#[test]
fn test_set_debayer() {
    let config = SimulatedCameraConfig::default().with_color(BayerPattern::RGGB);
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();
    camera.set_stream_mode(StreamMode::LiveMode).unwrap();
    camera.init().unwrap();

    // Enable debayer
    camera.set_debayer(true).unwrap();
    camera.begin_live().unwrap();

    let buffer_size = camera.get_image_size().unwrap();
    let mut buf = vec![0u8; buffer_size];
    let info = camera.get_live_frame(&mut buf).unwrap();

    // With debayer enabled, the reported frame is 3-channel RGB, 3x the mono size
    let mono_pixels = 3072 * 2048;
    let expected_rgb_size = mono_pixels * 2 * 3; // 16-bit * 3 channels
    assert_eq!(info.channels, 3);
    assert_eq!(frame_bytes(&info), expected_rgb_size as usize);

    camera.end_live().unwrap();

    // Disable debayer
    camera.set_debayer(false).unwrap();
    camera.begin_live().unwrap();

    let buffer_size = camera.get_image_size().unwrap();
    let mut buf = vec![0u8; buffer_size];
    let info = camera.get_live_frame(&mut buf).unwrap();

    // With debayer disabled, the reported frame is 1-channel
    let expected_mono_size = mono_pixels * 2; // 16-bit * 1 channel
    assert_eq!(info.channels, 1);
    assert_eq!(frame_bytes(&info), expected_mono_size as usize);

    camera.end_live().unwrap();
    camera.close().unwrap();
}

#[test]
fn test_camera_not_open_errors() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // Camera is not open - these should all fail
    assert!(camera.get_model().is_err());
    assert!(camera.get_ccd_info().is_err());
    assert!(camera.get_firmware_version().is_err());
    assert!(camera.get_effective_area().is_err());
    assert!(camera.get_overscan_area().is_err());
    assert!(camera.set_parameter(ControlType::Gain, 50.0).is_err());
    assert!(camera.get_parameter(ControlType::Gain).is_err());
    assert!(camera.stop_exposure().is_err());
    assert!(camera.abort_exposure_and_readout().is_err());
}

#[test]
fn test_parameter_not_available_error() {
    let config = SimulatedCameraConfig::default(); // No cooler
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // Cooler control should not be available
    assert!(camera.is_control_available(ControlType::Cooler).is_none());

    // Getting an unavailable control should fail
    assert!(camera.get_parameter(ControlType::Cooler).is_err());

    // get_parameter_min_max_step should fail for unavailable control
    assert!(camera
        .get_parameter_min_max_step(ControlType::Cooler)
        .is_err());

    // set_if_available should fail for unavailable control
    assert!(camera.set_if_available(ControlType::Cooler, -10.0).is_err());

    camera.close().unwrap();
}

#[test]
fn test_set_if_available() {
    let config = SimulatedCameraConfig::default().with_cooler();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // set_if_available should work for available control
    camera.set_if_available(ControlType::Gain, 50.0).unwrap();
    let gain = camera.get_parameter(ControlType::Gain).unwrap();
    assert!((gain - 50.0).abs() < f64::EPSILON);

    // set_if_available should work for cooler (available due to with_cooler)
    camera.set_if_available(ControlType::Cooler, -10.0).unwrap();

    camera.close().unwrap();
}

#[test]
fn test_double_open() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // First open should succeed
    camera.open().unwrap();
    assert!(camera.is_open().unwrap());

    // Second open should also succeed (idempotent)
    camera.open().unwrap();
    assert!(camera.is_open().unwrap());

    camera.close().unwrap();
}

#[test]
fn test_double_close() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    camera.open().unwrap();
    assert!(camera.is_open().unwrap());

    // First close should succeed
    camera.close().unwrap();
    assert!(!camera.is_open().unwrap());

    // Second close should also succeed (idempotent)
    camera.close().unwrap();
    assert!(!camera.is_open().unwrap());
}

#[test]
fn test_get_ccd_info_not_open() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // Camera is not open, should fail
    assert!(camera.get_ccd_info().is_err());

    // After opening, should succeed
    camera.open().unwrap();
    let chip_info = camera.get_ccd_info().unwrap();
    assert_eq!(chip_info.image_width, 3072);

    camera.close().unwrap();
}

#[test]
fn test_no_filter_wheel() {
    let config = SimulatedCameraConfig::default(); // No filter wheel
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // Should report no filter wheel plugged in
    assert!(!camera.is_cfw_plugged_in().unwrap());

    // CFW control should not be available
    assert!(camera.is_control_available(ControlType::CfwPort).is_none());

    camera.close().unwrap();
}

// ============================================================================
// Trait Implementation Tests
// ============================================================================

#[test]
fn test_camera_clone() {
    let config = SimulatedCameraConfig::default().with_id("CLONE-TEST");
    let camera1 = Camera::new_simulated(config);

    // Clone the camera
    let camera2 = camera1.clone();

    // Both should have the same ID
    assert_eq!(camera1.id(), camera2.id());
    assert_eq!(camera1.id(), "CLONE-TEST");

    // Both should be simulated
    assert!(camera1.is_simulated());
    assert!(camera2.is_simulated());

    // Opening one should not affect the other's state query
    camera1.open().unwrap();
    assert!(camera1.is_open().unwrap());

    camera1.close().unwrap();
}

#[test]
fn test_camera_partial_eq() {
    let config1 = SimulatedCameraConfig::default().with_id("CAM-A");
    let config2 = SimulatedCameraConfig::default().with_id("CAM-B");
    let config3 = SimulatedCameraConfig::default().with_id("CAM-A");

    let camera_a = Camera::new_simulated(config1);
    let camera_b = Camera::new_simulated(config2);
    let camera_a2 = Camera::new_simulated(config3);

    // Cameras with the same ID should be equal
    assert_eq!(camera_a, camera_a2);

    // Cameras with different IDs should not be equal
    assert_ne!(camera_a, camera_b);
}

#[test]
fn test_camera_debug() {
    let config = SimulatedCameraConfig::default().with_id("DEBUG-TEST");
    let camera = Camera::new_simulated(config);

    let debug_str = format!("{camera:?}");
    assert!(debug_str.contains("Camera"));
    assert!(debug_str.contains("DEBUG-TEST"));
}

// ============================================================================
// Additional Error Case Tests
// ============================================================================

#[test]
fn test_init_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // init() without opening should fail
    assert!(camera.init().is_err());
}

#[test]
fn test_set_stream_mode_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // set_stream_mode() without opening should fail
    assert!(camera.set_stream_mode(StreamMode::LiveMode).is_err());
}

#[test]
fn test_set_bin_mode_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // set_bin_mode() without opening should fail
    assert!(camera.set_bin_mode(2, 2).is_err());
}

#[test]
fn test_set_bit_mode_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // set_bit_mode() without opening should fail
    assert!(camera.set_bit_mode(8).is_err());
}

#[test]
fn test_begin_live_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // begin_live() without opening should fail
    assert!(camera.begin_live().is_err());
}

#[test]
fn test_end_live_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // end_live() without opening should fail
    assert!(camera.end_live().is_err());
}

#[test]
fn test_get_image_size_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // get_image_size() without opening should fail
    assert!(camera.get_image_size().is_err());
}

#[test]
fn test_get_live_frame_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // get_live_frame() without opening should fail
    let mut buf = vec![0u8; 1000];
    assert!(camera.get_live_frame(&mut buf).is_err());
}

#[test]
fn test_get_single_frame_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // get_single_frame() without opening should fail
    let mut buf = vec![0u8; 1000];
    assert!(camera.get_single_frame(&mut buf).is_err());
}

#[test]
fn test_start_single_frame_exposure_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // start_single_frame_exposure() without opening should fail
    assert!(camera.start_single_frame_exposure().is_err());
}

#[test]
fn test_get_remaining_exposure_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // get_remaining_exposure_us() without opening should fail
    assert!(camera.get_remaining_exposure_us().is_err());
}

#[test]
fn test_set_roi_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    let roi = CCDChipArea {
        start_x: 0,
        start_y: 0,
        width: 100,
        height: 100,
    };

    // set_roi() without opening should fail
    assert!(camera.set_roi(roi).is_err());
}

#[test]
fn test_set_debayer_not_open_error() {
    let config = SimulatedCameraConfig::default().with_color(BayerPattern::RGGB);
    let camera = Camera::new_simulated(config);

    // set_debayer() without opening should fail
    assert!(camera.set_debayer(true).is_err());
}

#[test]
fn test_is_cfw_plugged_in_not_open_error() {
    let config = SimulatedCameraConfig::default().with_filter_wheel(5);
    let camera = Camera::new_simulated(config);

    // is_cfw_plugged_in() without opening should fail
    assert!(camera.is_cfw_plugged_in().is_err());
}

#[test]
fn test_get_type_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // get_type() without opening should fail
    assert!(camera.get_type().is_err());
}

#[test]
fn test_get_number_of_readout_modes_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // get_number_of_readout_modes() without opening should fail
    assert!(camera.get_number_of_readout_modes().is_err());
}

#[test]
fn test_get_readout_mode_name_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // get_readout_mode_name() without opening should fail
    assert!(camera.get_readout_mode_name(0).is_err());
}

#[test]
fn test_get_readout_mode_resolution_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // get_readout_mode_resolution() without opening should fail
    assert!(camera.get_readout_mode_resolution(0).is_err());
}

#[test]
fn test_set_readout_mode_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // set_readout_mode() without opening should fail
    assert!(camera.set_readout_mode(0).is_err());
}

#[test]
fn test_is_control_available_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // is_control_available() without opening returns None (not an error, but no result)
    // This is expected behavior - returns None when camera not open
    let result = camera.is_control_available(ControlType::Gain);
    assert!(result.is_none());
}

#[test]
fn test_get_parameter_min_max_step_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // get_parameter_min_max_step() without opening should fail
    assert!(camera
        .get_parameter_min_max_step(ControlType::Gain)
        .is_err());
}

#[test]
fn test_invalid_readout_mode_name() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // Requesting a non-existent readout mode should fail
    let result = camera.get_readout_mode_name(99);
    assert!(result.is_err());

    camera.close().unwrap();
}

#[test]
fn test_invalid_readout_mode_resolution() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // Requesting a non-existent readout mode resolution should fail
    let result = camera.get_readout_mode_resolution(99);
    assert!(result.is_err());

    camera.close().unwrap();
}

#[test]
fn test_filter_wheel_not_open_errors() {
    let config = SimulatedCameraConfig::default()
        .with_id("FW-ERROR-TEST")
        .with_filter_wheel(5);
    let camera = Camera::new_simulated(config);
    let fw = FilterWheel::new(camera);

    // Filter wheel operations without opening should fail
    assert!(fw.get_number_of_filters().is_err());
    assert!(fw.get_fw_position().is_err());
    assert!(fw.set_fw_position(1).is_err());
    assert!(fw.is_cfw_plugged_in().is_err());
}

#[test]
fn test_multiple_simulated_cameras() {
    let mut sdk = Sdk::new_simulated();

    // Add multiple cameras
    sdk.add_simulated_camera(SimulatedCameraConfig::default().with_id("CAM-001"));
    sdk.add_simulated_camera(SimulatedCameraConfig::default().with_id("CAM-002"));
    sdk.add_simulated_camera(
        SimulatedCameraConfig::default()
            .with_id("CAM-003")
            .with_filter_wheel(5),
    );

    assert_eq!(sdk.cameras().count(), 3);
    assert_eq!(sdk.filter_wheels().count(), 1); // Only CAM-003 has a filter wheel

    // Verify camera IDs
    let ids: Vec<_> = sdk.cameras().map(|c| c.id().to_string()).collect();
    assert!(ids.contains(&"CAM-001".to_string()));
    assert!(ids.contains(&"CAM-002".to_string()));
    assert!(ids.contains(&"CAM-003".to_string()));
}

#[test]
fn test_get_current_readout_mode() {
    let config = SimulatedCameraConfig::default()
        .with_readout_mode("High Speed", 3072, 2048)
        .with_readout_mode("Low Noise", 1536, 1024);
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // Get initial readout mode (should be 0)
    let mode = camera.get_readout_mode().unwrap();
    assert_eq!(mode, 0);

    // Set to mode 1
    camera.set_readout_mode(1).unwrap();

    // Get current mode (should be 1)
    let mode = camera.get_readout_mode().unwrap();
    assert_eq!(mode, 1);

    // Set to mode 2
    camera.set_readout_mode(2).unwrap();
    let mode = camera.get_readout_mode().unwrap();
    assert_eq!(mode, 2);

    camera.close().unwrap();
}

#[test]
fn test_get_readout_mode_not_open_error() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);

    // get_readout_mode() without opening should fail
    assert!(camera.get_readout_mode().is_err());
}

#[test]
fn test_custom_chip_info() {
    use qhyccd_rs::CCDChipInfo;

    let chip_info = CCDChipInfo {
        chip_width: 14.0,
        chip_height: 10.0,
        image_width: 4096,
        image_height: 3072,
        pixel_width: 3.76,
        pixel_height: 3.76,
        bits_per_pixel: 16,
    };
    let config = SimulatedCameraConfig::default().with_chip_info(chip_info);
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    let info = camera.get_ccd_info().unwrap();
    assert_eq!(info.image_width, 4096);
    assert_eq!(info.image_height, 3072);
    assert!((info.pixel_width - 3.76).abs() < 0.01);
    assert!((info.pixel_height - 3.76).abs() < 0.01);

    camera.close().unwrap();
}

#[test]
fn test_cooler_controls() {
    let config = SimulatedCameraConfig::default().with_cooler();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // Check cooler control is available
    assert!(camera.is_control_available(ControlType::Cooler).is_some());
    assert!(camera.is_control_available(ControlType::CurTemp).is_some());

    // Get current temperature
    let temp = camera.get_parameter(ControlType::CurTemp).unwrap();
    assert!(temp > -50.0 && temp < 50.0); // Reasonable range

    // Set cooler target temperature
    camera.set_parameter(ControlType::Cooler, -10.0).unwrap();
    let cooler_target = camera.get_parameter(ControlType::Cooler).unwrap();
    assert!((cooler_target - (-10.0)).abs() < 0.001);

    camera.close().unwrap();
}

/// The typed accessors are the routine surface over the generic
/// `get_parameter`/`set_parameter` controls, so exercise the full set against
/// the simulation backend (Phase 2 of the convention-alignment plan).
#[test]
fn test_camera_typed_accessors() {
    let config = SimulatedCameraConfig::default()
        .with_cooler()
        .with_filter_wheel(5);
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // gain / offset / exposure round-trip through the typed setters + getters.
    camera.set_gain(50.0).unwrap();
    assert!((camera.gain().unwrap() - 50.0).abs() < f64::EPSILON);
    camera.set_offset(20.0).unwrap();
    assert!((camera.offset().unwrap() - 20.0).abs() < f64::EPSILON);
    camera.set_exposure_us(1000.0).unwrap();
    assert!((camera.exposure_us().unwrap() - 1000.0).abs() < f64::EPSILON);

    // Ranges come from the simulated control table.
    let (gain_min, gain_max, _) = camera.gain_range().unwrap();
    assert!(gain_min <= gain_max);
    assert!(camera.offset_range().unwrap().1 >= camera.offset_range().unwrap().0);
    assert!(camera.exposure_range_us().unwrap().1 > 0.0);

    // Temperature is already whole °C — no decode.
    let temp = camera.current_temperature_celsius().unwrap();
    assert!((-50.0..50.0).contains(&temp));

    // Cooler set-point and manual PWM route to the same controls the generic
    // path reads back.
    camera.set_target_temperature_celsius(-15.0).unwrap();
    assert!((camera.get_parameter(ControlType::Cooler).unwrap() - (-15.0)).abs() < 0.001);
    camera.set_manual_cooler_pwm(80.0).unwrap();
    assert!((camera.cooler_power_raw().unwrap() - 80.0).abs() < f64::EPSILON);

    // CFW accessors encode the slot as a hex-ASCII code; the move settles over a
    // few polls (poll-to-arrival), so read until it reports the target.
    assert_eq!(camera.cfw_slot_count().unwrap(), 5);
    camera.set_cfw_position(3).unwrap();
    cfw_arrive(&camera, 3);

    camera.close().unwrap();
}

/// Audit #2: reading `CurTemp` after a set-point drives the reported sensor
/// temperature toward it, one poll-step at a time (advance-on-poll, like the
/// sibling `svbony-rs` sim), instead of staying frozen at ambient.
#[test]
fn cooling_ramps_toward_target_over_polls() {
    let config = SimulatedCameraConfig::default().with_cooler();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // No set-point yet: the temperature holds at ambient across polls.
    let ambient = camera.current_temperature_celsius().unwrap();
    assert!((camera.current_temperature_celsius().unwrap() - ambient).abs() < f64::EPSILON);

    // Engage cooling; the reported temperature must descend toward the target and
    // eventually settle there.
    camera.set_target_temperature_celsius(-10.0).unwrap();
    let mut previous = camera.current_temperature_celsius().unwrap();
    for _ in 0..200 {
        let next = camera.current_temperature_celsius().unwrap();
        assert!(next <= previous, "temperature must not rise while cooling");
        previous = next;
    }
    assert!(
        (camera.current_temperature_celsius().unwrap() - (-10.0)).abs() < f64::EPSILON,
        "temperature should reach the set-point after enough polls"
    );
    // Auto-cooling now reports a nonzero cooler PWM (was frozen at 0 before).
    assert!(camera.cooler_power_raw().unwrap() > 0.0);

    camera.close().unwrap();
}

/// Audit #11: a `close()` -> `open()` cycle presents a clean device. Real
/// `CloseQHYCCD` destroys the handle, so retained live/exposure/frame state must
/// not survive to make a frame call falsely succeed without re-arming.
#[test]
fn close_then_open_clears_session_state() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // Engage live mode and confirm a frame flows.
    camera.begin_live().unwrap();
    let size = camera.get_image_size().unwrap();
    let mut buf = vec![0u8; size];
    camera.get_live_frame(&mut buf).unwrap();

    // Reconnect. Live mode must NOT still be engaged on the fresh device.
    camera.close().unwrap();
    camera.open().unwrap();
    let err = camera.get_live_frame(&mut buf).unwrap_err();
    assert!(
        matches!(err, QHYError::Sdk { .. }),
        "live frame must fail after reconnect until begin_live() is called again, got {err:?}"
    );

    camera.close().unwrap();
}

/// Audit #3: the CFW position is carried on `CONTROL_CFWPORT` as a HEX ASCII
/// digit, so slot 10 encodes to 'A' (65), not ':' (58). Exercise a >= 10 slot.
#[test]
fn cfw_position_uses_hex_ascii_for_high_slots() {
    let config = SimulatedCameraConfig::default().with_filter_wheel(16);
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    camera.set_cfw_position(10).unwrap();
    cfw_arrive(&camera, 10);
    // On the wire the value is the hex-ASCII code 'A' == 65, not decimal 10 + 48 == 58.
    assert_eq!(
        camera.get_parameter(ControlType::CfwPort).unwrap() as u32,
        65
    );

    camera.close().unwrap();
}

/// Audit #4: the simulated CFW move is NOT instantaneous — the first read after a
/// command still reports the old slot (a consumer must poll to arrival), unlike
/// the former synchronous update that hid the real hardware settle time.
#[test]
fn cfw_move_is_not_instantaneous() {
    let config = SimulatedCameraConfig::default().with_filter_wheel(7);
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // Start at slot 0, command slot 5. The next read must still report a
    // non-target (moving) slot...
    camera.set_cfw_position(5).unwrap();
    assert_ne!(
        camera.cfw_position().unwrap(),
        5,
        "move should not be instantaneous"
    );
    // ...but it converges to the target with a few more polls.
    cfw_arrive(&camera, 5);

    camera.close().unwrap();
}

/// A raw `CONTROL_CFWPORT` write carrying a byte that is not a hex digit decodes
/// through the legacy decimal fallback, which has no upper bound: `'G'` reads as
/// slot 23. Commanding that would park the wheel where the position read — which
/// reports by *encoding* the slot — has no code to answer with, so every later
/// read would fail. The command is rejected instead and the wheel holds its slot.
#[test]
fn cfw_command_rejects_a_slot_the_wheel_could_not_report() {
    let config = SimulatedCameraConfig::default().with_filter_wheel(16);
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    camera.set_cfw_position(4).unwrap();
    cfw_arrive(&camera, 4);

    // 'G' is 0x47, one past 'F', so the hex decode declines it and the fallback
    // carries it to 0x47 - 0x30 = 23 — a slot no hex digit names.
    let err = camera
        .set_parameter(ControlType::CfwPort, f64::from(b'G'))
        .unwrap_err();
    assert!(
        matches!(err, QHYError::InvalidFilterSlot { slot: 23 }),
        "the rejected slot should be named, got {err:?}"
    );

    // The wheel stayed where it was, and reads still answer.
    assert_eq!(camera.cfw_position().unwrap(), 4);

    camera.close().unwrap();
}

/// Audit #6: with a configured not-ready probability, live-mode reads
/// intermittently return the retryable error (as real `GetQHYCCDLiveFrame` does
/// between frames), so a consumer must poll/retry. Over 200 reads at p=0.5 both
/// outcomes are effectively certain (P(all one outcome) = 0.5^200).
#[test]
fn live_frame_reports_not_ready_with_configured_probability() {
    let config = SimulatedCameraConfig::default().with_live_not_ready_probability(0.5);
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();
    camera.set_stream_mode(StreamMode::LiveMode).unwrap();
    camera.init().unwrap();
    // A small ROI keeps each generated frame cheap over many reads.
    camera
        .set_roi(CCDChipArea {
            start_x: 0,
            start_y: 0,
            width: 8,
            height: 8,
        })
        .unwrap();
    camera.begin_live().unwrap();

    let size = camera.get_image_size().unwrap();
    let mut buf = vec![0u8; size];

    let mut not_ready = 0;
    let mut frames = 0;
    for _ in 0..200 {
        match camera.get_live_frame(&mut buf) {
            Ok(info) => {
                assert_eq!(frame_bytes(&info), size);
                frames += 1;
            }
            Err(QHYError::Sdk { .. }) => not_ready += 1,
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert!(frames > 0, "should deliver frames");
    assert!(not_ready > 0, "should report some not-ready reads at p=0.5");

    camera.end_live().unwrap();
    camera.close().unwrap();
}

#[test]
fn test_usb_traffic_control() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // Check USB traffic control is available
    assert!(camera
        .is_control_available(ControlType::UsbTraffic)
        .is_some());

    // Get and set USB traffic
    let (min, max, step) = camera
        .get_parameter_min_max_step(ControlType::UsbTraffic)
        .unwrap();
    assert!(min <= max);
    assert!(step > 0.0);

    camera.set_parameter(ControlType::UsbTraffic, 50.0).unwrap();
    let traffic = camera.get_parameter(ControlType::UsbTraffic).unwrap();
    assert!((traffic - 50.0).abs() < f64::EPSILON);

    camera.close().unwrap();
}

#[test]
fn test_offset_control() {
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();

    // Check offset control is available
    assert!(camera.is_control_available(ControlType::Offset).is_some());

    // Get and set offset
    camera.set_parameter(ControlType::Offset, 10.0).unwrap();
    let offset = camera.get_parameter(ControlType::Offset).unwrap();
    assert!((offset - 10.0).abs() < f64::EPSILON);

    camera.close().unwrap();
}

#[test]
fn test_image_with_custom_generator() {
    let gen = ImageGenerator::new(ImagePattern::TestPattern)
        .with_noise_level(5.0)
        .with_base_level(1000);

    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();
    camera.set_stream_mode(StreamMode::LiveMode).unwrap();
    camera.init().unwrap();
    camera.begin_live().unwrap();

    let buffer_size = camera.get_image_size().unwrap();
    let mut buf = vec![0u8; buffer_size];
    let image = camera.get_live_frame(&mut buf).unwrap();

    // Image should have expected dimensions
    assert_eq!(image.width, 3072);
    assert_eq!(image.height, 2048);
    assert_eq!(frame_bytes(&image), buffer_size);

    camera.end_live().unwrap();
    camera.close().unwrap();

    // Also test the generator directly produces test pattern
    let data = gen.generate_16bit(100, 100, 1);
    assert_eq!(data.len(), 20000); // 100 * 100 * 2 bytes
}

#[test]
fn test_filter_wheel_close_then_reopen() {
    let config = SimulatedCameraConfig::default()
        .with_id("FW-REOPEN")
        .with_filter_wheel(5);
    let camera = Camera::new_simulated(config);
    let fw = FilterWheel::new(camera);

    // Open
    fw.open().unwrap();
    assert!(fw.is_open().unwrap());

    // Set position; the move settles over a few polls (poll-to-arrival).
    fw.set_fw_position(2).unwrap();
    fw_arrive(&fw, 2);

    // Close
    fw.close().unwrap();
    assert!(!fw.is_open().unwrap());

    // Reopen
    fw.open().unwrap();
    assert!(fw.is_open().unwrap());

    // Position should still be accessible
    let pos = fw.get_fw_position().unwrap();
    assert!(pos < 10); // Valid position range

    fw.close().unwrap();
}

// The tests below preserve coverage of behaviour formerly reached only through
// the deleted FFI-mock unit tests (src/tests/), re-expressed against the
// simulated backend (the mock layer is gone — see the convention plan, Phase 4).

#[test]
fn test_simulated_sdk_version_is_the_targeted_release() {
    // The simulated `Sdk::version()` returns the synthetic SDK release this crate
    // targets — previously only asserted (on the FFI byte-decode) by the deleted
    // `sdk_tests::version_success`.
    let sdk = Sdk::new_simulated();
    assert_eq!(
        sdk.version().unwrap(),
        SDKVersion {
            year: 26,
            month: 6,
            day: 4,
            subday: 0,
        }
    );
}

#[test]
fn test_get_live_frame_without_begin_live_errors() {
    // Open + init in live mode but never call `begin_live`: `get_live_frame`
    // must fail (formerly `camera_tests::get_live_frame_fail`).
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();
    camera.set_stream_mode(StreamMode::LiveMode).unwrap();
    camera.init().unwrap();

    let buffer_size = camera.get_image_size().unwrap();
    let mut buf = vec![0u8; buffer_size];
    assert!(camera.get_live_frame(&mut buf).is_err());

    camera.close().unwrap();
}

#[test]
fn test_get_single_frame_without_exposure_errors() {
    // Open + init in single-frame mode but never start an exposure: no image is
    // captured, so `get_single_frame` must fail (formerly
    // `camera_tests::get_single_frame_fail`).
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();
    camera.set_stream_mode(StreamMode::SingleFrameMode).unwrap();
    camera.init().unwrap();

    let buffer_size = camera.get_image_size().unwrap();
    let mut buf = vec![0u8; buffer_size];
    assert!(camera.get_single_frame(&mut buf).is_err());

    camera.close().unwrap();
}

#[test]
fn test_get_live_frame_rejects_undersized_buffer() {
    // A buffer smaller than the frame is rejected with `BufferTooSmall` before any
    // pixels are written — the caller-owned-buffer bounds check (Phase 5).
    let config = SimulatedCameraConfig::default();
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();
    camera.set_stream_mode(StreamMode::LiveMode).unwrap();
    camera.init().unwrap();
    camera.begin_live().unwrap();

    let need = camera.get_image_size().unwrap();
    let mut buf = vec![0u8; need - 1];
    let error = camera.get_live_frame(&mut buf).unwrap_err();
    assert_eq!(
        error,
        QHYError::BufferTooSmall {
            needed: need,
            got: need - 1,
        }
    );

    camera.end_live().unwrap();
    camera.close().unwrap();
}

#[test]
fn test_filter_wheel_methods_error_when_camera_has_no_cfw_control() {
    // A `FilterWheel` wrapping an OPEN camera whose config has no CFW control:
    // every wheel method must fail on the control-unavailable branch — distinct
    // from the not-open branch. Preserves the three deleted
    // `filter_wheel_tests::*_fail_no_filter_wheel` cases.
    let config = SimulatedCameraConfig::default(); // no `.with_filter_wheel(..)`
    let camera = Camera::new_simulated(config);
    camera.open().unwrap();
    let fw = FilterWheel::new(camera);

    assert!(fw.get_number_of_filters().is_err());
    assert!(fw.get_fw_position().is_err());
    assert!(fw.set_fw_position(1).is_err());
}
