//! The simulated devices driven through the Alpaca client, as the guider
//! drives them. See `docs/crates/guide-simulator-alpaca.md` (Testing).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::indexing_slicing
)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    clippy::used_underscore_binding,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::float_cmp,
    clippy::similar_names,
    clippy::struct_excessive_bools
)]

use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::camera::{CameraState, SensorType};
use ascom_alpaca::api::telescope::{GuideDirection, PierSide};
use ascom_alpaca::api::{Camera, Telescope, TypedDevice};
use ascom_alpaca::{ASCOMErrorCode, Client};
use guide_simulator::{
    CameraConfig, GuideDirection as SimDirection, MountConfig, PierSide as SimSide, Seeing,
    SimConfig, SkyConfig, SkyOffset, Star, StarField, SIDEREAL_ARCSEC_PER_S,
};
use guide_simulator_alpaca::{Device, DevicesConfig, FrameFault, Region, SimDevices};

const WIDTH: usize = 320;
const HEIGHT: usize = 240;

/// A 320 × 240 sensor at 2″ a pixel with four bright stars in steady 3″
/// seeing, and a mount at declination 30°.
fn config() -> DevicesConfig {
    let star = |east, north| Star {
        position: SkyOffset::new(east, north),
        e_per_s: 50_000.0,
    };
    DevicesConfig {
        sim: SimConfig {
            seed: 7,
            mount: MountConfig {
                declination_deg: 30.0,
                ..MountConfig::default()
            },
            camera: CameraConfig {
                width: WIDTH,
                height: HEIGHT,
                ..CameraConfig::default()
            },
            sky: SkyConfig {
                stars: StarField::Listed(vec![
                    star(-100.0, -60.0),
                    star(100.0, 60.0),
                    star(0.0, 0.0),
                    star(-100.0, 60.0),
                ]),
                seeing: Seeing {
                    fwhm_arcsec: 3.0,
                    common_motion_arcsec: 0.0,
                    star_motion_arcsec: 0.0,
                },
                clouds: Vec::new(),
            },
        },
        ..DevicesConfig::default()
    }
}

async fn camera(devices: &SimDevices) -> Arc<dyn Camera> {
    let client = Client::new(devices.camera_url()).unwrap();
    let mut listed = client.get_devices().await.unwrap();
    match listed.next() {
        Some(TypedDevice::Camera(camera)) => camera,
        other => panic!("the camera's server lists {other:?}"),
    }
}

async fn mount(devices: &SimDevices) -> Arc<dyn Telescope> {
    let client = Client::new(devices.mount_url()).unwrap();
    let mut listed = client.get_devices().await.unwrap();
    match listed.next() {
        Some(TypedDevice::Telescope(mount)) => mount,
        other => panic!("the mount's server lists {other:?}"),
    }
}

/// Started devices, both connected through the client.
async fn connected() -> (SimDevices, Arc<dyn Camera>, Arc<dyn Telescope>) {
    connected_with(config()).await
}

async fn connected_with(
    config: DevicesConfig,
) -> (SimDevices, Arc<dyn Camera>, Arc<dyn Telescope>) {
    let devices = SimDevices::start(config).await.unwrap();
    let camera = camera(&devices).await;
    let mount = mount(&devices).await;
    camera.set_connected(true).await.unwrap();
    mount.set_connected(true).await.unwrap();
    (devices, camera, mount)
}

async fn wait_ready(camera: &dyn Camera) {
    for _ in 0..500 {
        if camera.image_ready().await.unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the image was not ready within 5 s");
}

async fn wait_pulse_end(mount: &dyn Telescope) {
    for _ in 0..500 {
        if !mount.is_pulse_guiding().await.unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the pulse did not end within 5 s");
}

/// Exposes for `duration` and fetches the image.
async fn frame(camera: &dyn Camera, duration: Duration) -> ascom_alpaca::api::camera::ImageArray {
    camera.start_exposure(duration, true).await.unwrap();
    wait_ready(camera).await;
    camera.image_array().await.unwrap()
}

fn code<T: std::fmt::Debug>(result: ascom_alpaca::ASCOMResult<T>) -> ASCOMErrorCode {
    result.unwrap_err().code
}

#[tokio::test]
async fn each_server_lists_its_device_with_its_unique_id() {
    let devices = SimDevices::start(config()).await.unwrap();
    for (url, kind, id) in [
        (devices.camera_url(), "Camera", "guide-simulator-camera"),
        (devices.mount_url(), "Telescope", "guide-simulator-mount"),
    ] {
        let listed: serde_json::Value =
            reqwest::get(format!("{url}/management/v1/configureddevices"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        let value = &listed["Value"];
        assert_eq!(value.as_array().unwrap().len(), 1, "{listed}");
        assert_eq!(value[0]["DeviceType"], kind);
        assert_eq!(value[0]["DeviceNumber"], 0);
        assert_eq!(value[0]["UniqueID"], id);
    }
}

#[tokio::test]
async fn devices_start_not_connected_and_refuse_their_members() {
    let devices = SimDevices::start(config()).await.unwrap();
    let camera = camera(&devices).await;
    let mount = mount(&devices).await;
    assert!(!camera.connected().await.unwrap());
    assert!(!mount.connected().await.unwrap());
    assert_eq!(
        code(camera.camera_x_size().await),
        ASCOMErrorCode::NOT_CONNECTED
    );
    assert_eq!(code(mount.tracking().await), ASCOMErrorCode::NOT_CONNECTED);
    assert_eq!(devices.exposures(), []);
}

#[tokio::test]
async fn the_camera_reports_its_sensor() {
    let (_devices, camera, _mount) = connected().await;
    assert_eq!(camera.camera_x_size().await.unwrap(), 320);
    assert_eq!(camera.camera_y_size().await.unwrap(), 240);
    assert_eq!(camera.pixel_size_x().await.unwrap(), 2.0);
    assert_eq!(camera.sensor_type().await.unwrap(), SensorType::Monochrome);
    assert_eq!(camera.max_adu().await.unwrap(), 65_535);
    assert_eq!(camera.max_bin_x().await.unwrap(), 2);
    assert!(camera.can_abort_exposure().await.unwrap());
}

#[tokio::test]
async fn an_exposure_is_in_progress_for_its_length() {
    let (_devices, camera, _mount) = connected().await;
    camera
        .start_exposure(Duration::from_secs(10), true)
        .await
        .unwrap();
    assert!(!camera.image_ready().await.unwrap());
    assert_eq!(camera.camera_state().await.unwrap(), CameraState::Exposing);
    assert_eq!(camera.percent_completed().await.unwrap(), 0);
}

#[tokio::test]
async fn an_exposure_runs_to_its_image() {
    let (devices, camera, _mount) = connected().await;
    camera.set_gain(30).await.unwrap();
    camera
        .start_exposure(Duration::from_millis(300), true)
        .await
        .unwrap();
    wait_ready(&*camera).await;
    assert_eq!(camera.camera_state().await.unwrap(), CameraState::Idle);
    let image = camera.image_array().await.unwrap();
    assert_eq!(image.dim(), (WIDTH, HEIGHT, 1));
    // The stars rise well above the bias of 500 ADU.
    let brightest = image.iter().copied().max().unwrap();
    assert!(brightest > 2_000, "the stars are in the frame: {brightest}");
    let exposures = devices.exposures();
    assert_eq!(exposures.len(), 1);
    assert_eq!(exposures[0].duration, Duration::from_millis(300));
    assert_eq!((exposures[0].bin, exposures[0].gain), (1, 30));
}

#[tokio::test]
async fn a_second_exposure_is_refused_while_one_runs_and_an_abort_drops_it() {
    let (devices, camera, mount) = connected().await;
    camera
        .start_exposure(Duration::from_secs(10), true)
        .await
        .unwrap();
    assert_eq!(
        code(camera.start_exposure(Duration::from_secs(1), true).await),
        ASCOMErrorCode::INVALID_OPERATION
    );
    camera.abort_exposure().await.unwrap();
    assert_eq!(devices.aborts(), 1);
    assert_eq!(camera.camera_state().await.unwrap(), CameraState::Idle);
    assert!(!camera.image_ready().await.unwrap());
    // Nothing of the aborted exposure is left: the next exposure and the
    // next pulse start now, not at the aborted exposure's end 10 s on.
    frame(&*camera, Duration::from_millis(20)).await;
    mount
        .pulse_guide(GuideDirection::East, Duration::from_millis(10))
        .await
        .unwrap();
    let now = devices.now();
    assert!(devices.exposures().last().unwrap().at <= now);
    assert!(devices.pulses().last().unwrap().at <= now);
}

#[tokio::test]
async fn a_start_is_refused_while_the_last_exposure_reads_out() {
    let mut config = config();
    config.camera.readout = Duration::from_secs(10);
    let (_devices, camera, _mount) = connected_with(config).await;
    camera
        .start_exposure(Duration::from_millis(20), true)
        .await
        .unwrap();
    for _ in 0..500 {
        if camera.camera_state().await.unwrap() == CameraState::Reading {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(camera.camera_state().await.unwrap(), CameraState::Reading);
    assert_eq!(
        code(camera.start_exposure(Duration::from_millis(20), true).await),
        ASCOMErrorCode::INVALID_OPERATION
    );
}

#[tokio::test]
async fn an_abort_during_readout_returns_the_camera_to_idle() {
    let mut config = config();
    config.camera.readout = Duration::from_secs(10);
    let (_devices, camera, _mount) = connected_with(config).await;
    camera
        .start_exposure(Duration::from_millis(20), true)
        .await
        .unwrap();
    for _ in 0..500 {
        if camera.camera_state().await.unwrap() == CameraState::Reading {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    camera.abort_exposure().await.unwrap();
    assert_eq!(camera.camera_state().await.unwrap(), CameraState::Idle);
    assert!(!camera.image_ready().await.unwrap());
    camera
        .start_exposure(Duration::from_millis(20), true)
        .await
        .unwrap();
}

#[tokio::test]
async fn an_abort_drops_an_image_that_never_comes_and_keeps_a_ready_one() {
    let (devices, camera, _mount) = connected().await;
    devices.fail_frames(FrameFault::NeverReady, Some(1));
    camera
        .start_exposure(Duration::from_millis(20), true)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(camera.camera_state().await.unwrap(), CameraState::Reading);
    camera.abort_exposure().await.unwrap();
    assert_eq!(camera.camera_state().await.unwrap(), CameraState::Idle);
    frame(&*camera, Duration::from_millis(20)).await;
    camera.abort_exposure().await.unwrap();
    assert!(camera.image_ready().await.unwrap());
}

#[tokio::test]
async fn a_camera_that_cannot_abort_refuses_and_the_attempt_is_counted() {
    let mut config = config();
    config.camera.can_abort_exposure = false;
    let (devices, camera, _mount) = connected_with(config).await;
    assert!(!camera.can_abort_exposure().await.unwrap());
    camera
        .start_exposure(Duration::from_secs(10), true)
        .await
        .unwrap();
    assert_eq!(
        code(camera.abort_exposure().await),
        ASCOMErrorCode::NOT_IMPLEMENTED
    );
    assert_eq!(devices.aborts(), 1);
    assert_eq!(camera.camera_state().await.unwrap(), CameraState::Exposing);
}

#[tokio::test]
async fn a_dark_frame_and_an_exposure_out_of_range_are_refused() {
    let (_devices, camera, _mount) = connected().await;
    assert_eq!(
        code(
            camera
                .start_exposure(Duration::from_millis(20), false)
                .await
        ),
        ASCOMErrorCode::INVALID_VALUE
    );
    assert_eq!(
        code(
            camera
                .start_exposure(Duration::from_secs(3_601), true)
                .await
        ),
        ASCOMErrorCode::INVALID_VALUE
    );
}

#[tokio::test]
async fn binning_sums_cells_and_the_region_must_lie_on_the_binned_sensor() {
    let (devices, camera, _mount) = connected().await;
    let full = frame(&*camera, Duration::from_millis(20)).await;
    camera.set_bin_x(2).await.unwrap();
    camera.set_bin_y(2).await.unwrap();
    assert_eq!(
        code(camera.start_exposure(Duration::from_millis(20), true).await),
        ASCOMErrorCode::INVALID_VALUE,
        "the full frame at bin 1 leaves the sensor at bin 2"
    );
    camera.set_num_x(160).await.unwrap();
    camera.set_num_y(120).await.unwrap();
    let binned = frame(&*camera, Duration::from_millis(20)).await;
    assert_eq!(binned.dim(), (160, 120, 1));
    // A binned pixel holds four bias levels.
    let median = |image: &ascom_alpaca::api::camera::ImageArray| {
        let mut values: Vec<i32> = image.iter().copied().collect();
        values.sort_unstable();
        values[values.len() / 2]
    };
    let ratio = f64::from(median(&binned)) / f64::from(median(&full));
    assert!((ratio - 4.0).abs() < 0.2, "{ratio}");
    camera.set_start_x(100).await.unwrap();
    camera.set_num_x(60).await.unwrap();
    assert_eq!(
        frame(&*camera, Duration::from_millis(20)).await.dim(),
        (60, 120, 1)
    );
    assert_eq!(
        devices.exposures().last().unwrap().region,
        Region {
            x: 100,
            y: 0,
            width: 60,
            height: 120
        }
    );
    camera.set_connected(false).await.unwrap();
    camera.set_connected(true).await.unwrap();
    assert_eq!(camera.bin_x().await.unwrap(), 1);
    assert_eq!(camera.num_x().await.unwrap(), 320);
}

/// The shift of the field a pulse of `direction` brings, in the frame
/// after it.
async fn pulse_shift(
    devices: &SimDevices,
    camera: &dyn Camera,
    mount: &dyn Telescope,
    direction: GuideDirection,
    duration: Duration,
) -> guide_simulator::SensorPoint {
    frame(camera, Duration::from_millis(20)).await;
    let before = devices.last_truth().unwrap().field_shift;
    mount.pulse_guide(direction, duration).await.unwrap();
    wait_pulse_end(mount).await;
    frame(camera, Duration::from_millis(20)).await;
    devices.last_truth().unwrap().field_shift.minus(before)
}

#[tokio::test]
async fn a_pulse_moves_the_stars_its_way_in_the_next_frame() {
    let (devices, camera, mount) = connected().await;
    let half = Duration::from_millis(500);
    // 0.5 s at half the sidereal rate at 2″ a pixel, RA at declination 30°.
    let dec_px = 0.5 * 0.5 * SIDEREAL_ARCSEC_PER_S / 2.0;
    let ra_px = dec_px * 30_f64.to_radians().cos();
    // Unrotated and unmirrored, the sky's East lies along +x and its North
    // along +y. A West pulse moves the stars East, along +x, and a North
    // pulse moves them South, along -y.
    let west = pulse_shift(&devices, &*camera, &*mount, GuideDirection::West, half).await;
    assert!((west.x - ra_px).abs() < 0.05, "{west:?} against {ra_px}");
    assert!(west.y.abs() < 0.05, "{west:?}");
    let north = pulse_shift(&devices, &*camera, &*mount, GuideDirection::North, half).await;
    assert!(
        (north.y + dec_px).abs() < 0.05,
        "{north:?} against {dec_px}"
    );
    assert!(north.x.abs() < 0.05, "{north:?}");
    let east = pulse_shift(&devices, &*camera, &*mount, GuideDirection::East, half).await;
    assert!((east.x + ra_px).abs() < 0.05, "{east:?}");
    let south = pulse_shift(&devices, &*camera, &*mount, GuideDirection::South, half).await;
    assert!((south.y - dec_px).abs() < 0.05, "{south:?}");
    let pulses: Vec<_> = devices
        .pulses()
        .iter()
        .map(|pulse| (pulse.direction, pulse.duration))
        .collect();
    assert_eq!(
        pulses,
        [
            (SimDirection::West, half),
            (SimDirection::North, half),
            (SimDirection::East, half),
            (SimDirection::South, half),
        ]
    );
}

#[tokio::test]
async fn a_pulse_is_in_progress_until_it_ends_and_a_park_stops_it() {
    let (devices, _camera, mount) = connected().await;
    mount
        .pulse_guide(GuideDirection::West, Duration::from_secs(10))
        .await
        .unwrap();
    assert!(mount.is_pulse_guiding().await.unwrap());
    devices.park();
    assert!(!mount.is_pulse_guiding().await.unwrap());
}

#[tokio::test]
async fn a_pulse_lasts_its_length_on_the_wall_clock() {
    let (_devices, _camera, mount) = connected().await;
    let asked = std::time::Instant::now();
    mount
        .pulse_guide(GuideDirection::North, Duration::from_millis(300))
        .await
        .unwrap();
    wait_pulse_end(&*mount).await;
    assert!(asked.elapsed() >= Duration::from_millis(300));
}

#[tokio::test]
async fn the_mount_reports_its_rates_and_where_it_points() {
    let (_devices, _camera, mount) = connected().await;
    assert!(mount.can_pulse_guide().await.unwrap());
    let rate = mount.guide_rate_right_ascension().await.unwrap();
    assert!((rate - 0.5 * SIDEREAL_ARCSEC_PER_S / 3_600.0).abs() < 1e-12);
    assert_eq!(mount.guide_rate_declination().await.unwrap(), rate);
    assert!((mount.declination().await.unwrap() - 30.0).abs() < 1e-6);
    assert!((mount.right_ascension().await.unwrap() - 6.0).abs() < 1e-6);
    assert!((mount.sidereal_time().await.unwrap() - 6.0).abs() < 1e-3);
    assert_eq!(mount.side_of_pier().await.unwrap(), PierSide::East);
    assert_eq!(mount.altitude().await.unwrap(), 60.0);
    assert_eq!(mount.azimuth().await.unwrap(), 180.0);
}

#[tokio::test]
async fn a_park_stops_tracking_and_refuses_pulses() {
    let (devices, _camera, mount) = connected().await;
    assert!(mount.tracking().await.unwrap());
    assert!(!mount.at_park().await.unwrap());
    devices.park();
    assert!(mount.at_park().await.unwrap());
    assert!(!mount.tracking().await.unwrap());
    assert_eq!(
        code(
            mount
                .pulse_guide(GuideDirection::North, Duration::from_millis(100))
                .await
        ),
        ASCOMErrorCode::INVALID_WHILE_PARKED
    );
    assert_eq!(
        code(mount.set_tracking(true).await),
        ASCOMErrorCode::INVALID_WHILE_PARKED
    );
    assert_eq!(devices.pulses(), []);
    mount.unpark().await.unwrap();
    mount.set_tracking(true).await.unwrap();
    mount
        .pulse_guide(GuideDirection::North, Duration::from_millis(10))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_slew_in_progress_reads_as_slewing_and_refuses_pulses() {
    let (devices, _camera, mount) = connected().await;
    devices
        .slew(
            SkyOffset::new(0.0, 3_600.0),
            Duration::from_secs(10),
            SimSide::East,
        )
        .unwrap();
    assert!(mount.slewing().await.unwrap());
    assert_eq!(
        code(
            mount
                .pulse_guide(GuideDirection::North, Duration::from_millis(10))
                .await
        ),
        ASCOMErrorCode::INVALID_OPERATION
    );
}

#[tokio::test]
async fn a_slew_moves_the_pointing_and_can_change_the_pier_side() {
    let (devices, _camera, mount) = connected().await;
    // An hour of right ascension at declination 30°, and a degree north.
    let hour_east = 15.0 * 3_600.0 * 30_f64.to_radians().cos();
    devices
        .slew(
            SkyOffset::new(hour_east, 3_600.0),
            Duration::from_millis(200),
            SimSide::West,
        )
        .unwrap();
    for _ in 0..500 {
        if !mount.slewing().await.unwrap() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!mount.slewing().await.unwrap());
    assert_eq!(mount.side_of_pier().await.unwrap(), PierSide::West);
    assert!((mount.declination().await.unwrap() - 31.0).abs() < 1e-6);
    assert!((mount.right_ascension().await.unwrap() - 7.0).abs() < 1e-6);
}

#[tokio::test]
async fn tracking_written_through_the_client_reads_back() {
    let (devices, _camera, mount) = connected().await;
    mount.set_tracking(false).await.unwrap();
    assert!(!mount.tracking().await.unwrap());
    devices.set_tracking(true).unwrap();
    assert!(mount.tracking().await.unwrap());
}

#[tokio::test]
async fn a_mount_can_withhold_its_guide_rates_and_its_pier_side() {
    let mut config = config();
    config.mount.reports_guide_rates = false;
    config.mount.reports_pier_side = false;
    let (devices, _camera, mount) = connected_with(config).await;
    assert_eq!(
        code(mount.guide_rate_right_ascension().await),
        ASCOMErrorCode::NOT_IMPLEMENTED
    );
    assert_eq!(
        code(mount.guide_rate_declination().await),
        ASCOMErrorCode::NOT_IMPLEMENTED
    );
    assert_eq!(mount.side_of_pier().await.unwrap(), PierSide::Unknown);
    devices.set_can_pulse_guide(false);
    assert!(!mount.can_pulse_guide().await.unwrap());
    assert_eq!(
        code(
            mount
                .pulse_guide(GuideDirection::East, Duration::from_millis(10))
                .await
        ),
        ASCOMErrorCode::NOT_IMPLEMENTED
    );
}

#[tokio::test]
async fn stuck_pulses_keep_is_pulse_guiding_true_until_released() {
    let (devices, _camera, mount) = connected().await;
    devices.set_pulses_stuck(true);
    mount
        .pulse_guide(GuideDirection::East, Duration::from_millis(20))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(mount.is_pulse_guiding().await.unwrap());
    devices.set_pulses_stuck(false);
    assert!(!mount.is_pulse_guiding().await.unwrap());
}

#[tokio::test]
async fn reversed_rate_factors_move_the_stars_the_other_way() {
    let (devices, camera, mount) = connected().await;
    devices.set_rate_factors(-1.0, 1.0).unwrap();
    frame(&*camera, Duration::from_millis(20)).await;
    let before = devices.last_truth().unwrap().field_shift;
    mount
        .pulse_guide(GuideDirection::West, Duration::from_millis(300))
        .await
        .unwrap();
    wait_pulse_end(&*mount).await;
    frame(&*camera, Duration::from_millis(20)).await;
    let reversed = devices.last_truth().unwrap().field_shift.minus(before);
    devices.set_rate_factors(1.0, 1.0).unwrap();
    mount
        .pulse_guide(GuideDirection::West, Duration::from_millis(300))
        .await
        .unwrap();
    wait_pulse_end(&*mount).await;
    let before = devices.last_truth().unwrap().field_shift;
    frame(&*camera, Duration::from_millis(20)).await;
    let normal = devices.last_truth().unwrap().field_shift.minus(before);
    assert!(reversed.x.abs() > 0.5, "{reversed:?}");
    assert!(
        (reversed.x + normal.x).abs() < 0.05,
        "{reversed:?} {normal:?}"
    );
}

#[tokio::test]
async fn a_start_fault_fails_start_exposure_for_its_count() {
    let (devices, camera, _mount) = connected().await;
    devices.fail_frames(FrameFault::StartFails, Some(2));
    for _ in 0..2 {
        assert_eq!(
            code(camera.start_exposure(Duration::from_millis(20), true).await),
            ASCOMErrorCode::INVALID_OPERATION
        );
    }
    frame(&*camera, Duration::from_millis(20)).await;
    assert_eq!(devices.exposures().len(), 1);
}

#[tokio::test]
async fn a_start_fault_drops_the_last_frame() {
    let (devices, camera, _mount) = connected().await;
    frame(&*camera, Duration::from_millis(20)).await;
    devices.fail_frames(FrameFault::StartFails, Some(1));
    camera
        .start_exposure(Duration::from_millis(20), true)
        .await
        .unwrap_err();
    assert!(!camera.image_ready().await.unwrap());
    assert_eq!(
        code(camera.image_array().await),
        ASCOMErrorCode::INVALID_OPERATION
    );
}

#[tokio::test]
async fn a_never_ready_fault_keeps_the_image_from_arriving() {
    let (devices, camera, _mount) = connected().await;
    devices.fail_frames(FrameFault::NeverReady, None);
    camera
        .start_exposure(Duration::from_millis(20), true)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!camera.image_ready().await.unwrap());
    assert_eq!(camera.camera_state().await.unwrap(), CameraState::Reading);
    assert_eq!(
        code(camera.image_array().await),
        ASCOMErrorCode::INVALID_OPERATION
    );
    devices.clear_frame_faults();
    frame(&*camera, Duration::from_millis(20)).await;
}

#[tokio::test]
async fn a_wrong_size_fault_answers_a_narrower_frame() {
    let (devices, camera, _mount) = connected().await;
    devices.fail_frames(FrameFault::WrongSize, Some(1));
    assert_eq!(
        frame(&*camera, Duration::from_millis(20)).await.dim(),
        (WIDTH - 1, HEIGHT, 1)
    );
    assert_eq!(
        frame(&*camera, Duration::from_millis(20)).await.dim(),
        (WIDTH, HEIGHT, 1)
    );
}

#[tokio::test]
async fn an_unreachable_server_answers_503_while_the_other_answers() {
    let (devices, camera, mount) = connected().await;
    devices.set_reachable(Device::Camera, false);
    let status = reqwest::get(format!(
        "{}/management/v1/configureddevices",
        devices.camera_url()
    ))
    .await
    .unwrap()
    .status();
    assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    camera.connected().await.unwrap_err();
    assert!(mount.connected().await.unwrap());
    devices.set_reachable(Device::Camera, true);
    assert!(camera.connected().await.unwrap());
}

#[tokio::test]
async fn connections_set_by_the_scenario_read_back() {
    let (devices, camera, mount) = connected().await;
    devices.set_connected(Device::Camera, false);
    devices.set_connected(Device::Mount, false);
    assert!(!camera.connected().await.unwrap());
    assert!(!mount.connected().await.unwrap());
    devices.set_connected(Device::Camera, true);
    assert!(camera.connected().await.unwrap());
}

#[tokio::test]
async fn a_configuration_the_simulation_cannot_model_is_refused() {
    let mut config = config();
    config.camera.max_bin = 0;
    SimDevices::start(config).await.unwrap_err();
    let mut config = self::config();
    config.sim.camera.pixel_scale_arcsec = 0.0;
    SimDevices::start(config).await.unwrap_err();
}

#[tokio::test]
async fn stop_ends_both_servers() {
    let devices = SimDevices::start(config()).await.unwrap();
    let url = devices.camera_url();
    devices.stop().await;
    reqwest::get(format!("{url}/management/v1/configureddevices"))
        .await
        .unwrap_err();
}
