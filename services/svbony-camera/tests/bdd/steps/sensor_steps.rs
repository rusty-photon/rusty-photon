//! Sensor geometry, type, and signal steps (`@wip` — Phase E,
//! docs/plans/archive/svbony-camera.md).

use ascom_alpaca::api::camera::SensorType;
use cucumber::gherkin::Step;
use cucumber::then;

use crate::world::CameraWorld;

#[then(regex = r"^camera device (\d+) reports the sensor geometry:$")]
async fn reports_sensor_geometry(world: &mut CameraWorld, step: &Step, _device: u32) {
    let camera = world.camera();
    let table = step
        .table()
        .expect("sensor geometry step needs a data table");
    for row in table.rows.iter().skip(1) {
        let property = &row[0];
        let expected: u32 = row[1].parse().expect("geometry value is a u32");
        let actual = match property.as_str() {
            "CameraXSize" => camera.camera_x_size().await.unwrap(),
            "CameraYSize" => camera.camera_y_size().await.unwrap(),
            other => panic!("unknown geometry property: {other}"),
        };
        assert_eq!(actual, expected, "{property}");
    }
}

#[then(regex = r"^camera device (\d+) reports a positive PixelSizeX$")]
async fn positive_pixel_size_x(world: &mut CameraWorld, _device: u32) {
    assert!(world.camera().pixel_size_x().await.unwrap() > 0.0);
}

#[then(regex = r"^camera device (\d+) reports PixelSizeX equal to PixelSizeY$")]
async fn pixel_size_x_eq_y(world: &mut CameraWorld, _device: u32) {
    let camera = world.camera();
    let x = camera.pixel_size_x().await.unwrap();
    let y = camera.pixel_size_y().await.unwrap();
    assert!(
        (x - y).abs() < f64::EPSILON,
        "PixelSizeX {x} != PixelSizeY {y}"
    );
}

#[then(regex = r"^camera device (\d+) reports SensorType as (\w+)$")]
async fn reports_sensor_type(world: &mut CameraWorld, _device: u32, expected: String) {
    let actual = world.camera().sensor_type().await.unwrap();
    let expected = match expected.as_str() {
        "Monochrome" => SensorType::Monochrome,
        "RGGB" => SensorType::RGGB,
        other => panic!("unknown SensorType: {other}"),
    };
    assert_eq!(actual, expected);
}

#[then(regex = r"^camera device (\d+) reports MaxADU as (\d+)$")]
async fn reports_max_adu(world: &mut CameraWorld, _device: u32, expected: u32) {
    assert_eq!(world.camera().max_adu().await.unwrap(), expected);
}

#[then(regex = r"^camera device (\d+) reports a non-empty SensorName$")]
async fn non_empty_sensor_name(world: &mut CameraWorld, _device: u32) {
    assert_ne!(world.camera().sensor_name().await.unwrap(), "");
}

#[then(regex = r"^camera device (\d+) reports ElectronsPerADU as NOT_IMPLEMENTED$")]
async fn electrons_per_adu_not_implemented(world: &mut CameraWorld, _device: u32) {
    let err = world.camera().electrons_per_adu().await.unwrap_err();
    assert_eq!(err.code, ascom_alpaca::ASCOMErrorCode::NOT_IMPLEMENTED);
}
