//! Step definitions for `observing_conditions.feature`

use crate::steps::infrastructure::default_test_config;
use crate::world::Upbv2World;
use ascom_alpaca::ASCOMErrorCode;
use cucumber::{given, then, when};

// ============================================================================
// Given steps
// ============================================================================

#[given("a running UPBv2 server with the OC device connected")]
async fn running_server_with_oc_connected(world: &mut Upbv2World) {
    world.config = default_test_config();
    world.start_upbv2().await;

    world.oc_ref().set_connected(true).await.unwrap();
}

#[given(expr = "a running UPBv2 server with OC name {string}")]
async fn running_server_with_oc_name(world: &mut Upbv2World, name: String) {
    world.config = default_test_config();
    world.config["observingconditions"]["name"] = serde_json::json!(name);
    world.start_upbv2().await;
}

#[given(expr = "a running UPBv2 server with OC unique ID {string}")]
async fn running_server_with_oc_unique_id(world: &mut Upbv2World, unique_id: String) {
    world.config = default_test_config();
    world.config["observingconditions"]["unique_id"] = serde_json::json!(unique_id);
    world.start_upbv2().await;
}

// ============================================================================
// When steps
// ============================================================================

#[when("I wait for the OC data to be available")]
async fn wait_for_oc_data(world: &mut Upbv2World) {
    world.wait_for_oc_data().await;
}

#[when(expr = "I set the average period to {float} hours")]
async fn set_average_period(world: &mut Upbv2World, hours: f64) {
    world.oc_ref().set_average_period(hours).await.unwrap();
}

#[when(expr = "I try to set the average period to {float} hours")]
async fn try_set_average_period(world: &mut Upbv2World, hours: f64) {
    let result = world.oc_ref().set_average_period(hours).await;
    world.capture_result(result);
}

#[when("I try to read the temperature")]
async fn try_read_temperature(world: &mut Upbv2World) {
    let result = world.oc_ref().temperature().await;
    world.capture_result(result);
}

#[when("I try to read the humidity")]
async fn try_read_humidity(world: &mut Upbv2World) {
    let result = world.oc_ref().humidity().await;
    world.capture_result(result);
}

#[when("I try to read the dewpoint")]
async fn try_read_dewpoint(world: &mut Upbv2World) {
    let result = world.oc_ref().dew_point().await;
    world.capture_result(result);
}

#[when(expr = "I try to get sensor description for {string}")]
async fn try_get_sensor_description(world: &mut Upbv2World, sensor: String) {
    let result = world.oc_ref().sensor_description(sensor).await;
    world.capture_result(result);
}

#[when(expr = "I try to get time since last update for {string}")]
async fn try_get_time_since_last_update(world: &mut Upbv2World, sensor: String) {
    let result = world.oc_ref().time_since_last_update(sensor).await;
    world.capture_result(result);
}

#[when("I try to refresh the OC device")]
async fn try_refresh_oc_device(world: &mut Upbv2World) {
    let result = world.oc_ref().refresh().await;
    world.capture_result(result);
}

#[when("I try to read cloud cover")]
async fn try_read_cloud_cover(world: &mut Upbv2World) {
    let result = world.oc_ref().cloud_cover().await;
    world.capture_result(result);
}

#[when("I try to read pressure")]
async fn try_read_pressure(world: &mut Upbv2World) {
    let result = world.oc_ref().pressure().await;
    world.capture_result(result);
}

#[when("I try to read rain rate")]
async fn try_read_rain_rate(world: &mut Upbv2World) {
    let result = world.oc_ref().rain_rate().await;
    world.capture_result(result);
}

#[when("I try to read sky brightness")]
async fn try_read_sky_brightness(world: &mut Upbv2World) {
    let result = world.oc_ref().sky_brightness().await;
    world.capture_result(result);
}

#[when("I try to read sky quality")]
async fn try_read_sky_quality(world: &mut Upbv2World) {
    let result = world.oc_ref().sky_quality().await;
    world.capture_result(result);
}

#[when("I try to read sky temperature")]
async fn try_read_sky_temperature(world: &mut Upbv2World) {
    let result = world.oc_ref().sky_temperature().await;
    world.capture_result(result);
}

#[when("I try to read star FWHM")]
async fn try_read_star_fwhm(world: &mut Upbv2World) {
    let result = world.oc_ref().star_fwhm().await;
    world.capture_result(result);
}

#[when("I try to read wind direction")]
async fn try_read_wind_direction(world: &mut Upbv2World) {
    let result = world.oc_ref().wind_direction().await;
    world.capture_result(result);
}

#[when("I try to read wind gust")]
async fn try_read_wind_gust(world: &mut Upbv2World) {
    let result = world.oc_ref().wind_gust().await;
    world.capture_result(result);
}

#[when("I try to read wind speed")]
async fn try_read_wind_speed(world: &mut Upbv2World) {
    let result = world.oc_ref().wind_speed().await;
    world.capture_result(result);
}

#[when("I try to read the average period")]
async fn try_read_average_period(world: &mut Upbv2World) {
    let result = world.oc_ref().average_period().await;
    world.capture_result(result);
}

#[when(expr = "I wait {int} milliseconds")]
async fn wait_milliseconds(_world: &mut Upbv2World, millis: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
}

// ============================================================================
// Then steps
// ============================================================================

#[then(expr = "the OC device static name should be {string}")]
async fn oc_device_static_name_should_be(world: &mut Upbv2World, expected: String) {
    let name = world.oc_ref().name().await.unwrap();
    assert_eq!(name, expected, "OC device name mismatch");
}

#[then(expr = "the OC device unique ID should be {string}")]
async fn oc_device_unique_id_should_be(world: &mut Upbv2World, expected: String) {
    let uid = world.oc_ref().unique_id();
    assert_eq!(uid, expected, "OC unique ID mismatch");
}

#[then(expr = "the OC device description should contain {string}")]
async fn oc_device_description_should_contain(world: &mut Upbv2World, expected: String) {
    let desc = world.oc_ref().description().await.unwrap();
    assert!(
        desc.contains(&expected),
        "expected OC description to contain '{expected}', got: {desc}"
    );
}

#[then(expr = "the OC device driver info should contain {string}")]
async fn oc_device_driver_info_should_contain(world: &mut Upbv2World, expected: String) {
    let info = world.oc_ref().driver_info().await.unwrap();
    assert!(
        info.contains(&expected),
        "expected OC driver info to contain '{expected}', got: {info}"
    );
}

#[then("the OC device driver version should not be empty")]
async fn oc_device_driver_version_not_empty(world: &mut Upbv2World) {
    let version = world.oc_ref().driver_version().await.unwrap();
    assert!(!version.is_empty(), "OC driver version should not be empty");
}

#[then(expr = "the average period should be approximately {float} hours")]
async fn average_period_should_be_approximately(world: &mut Upbv2World, expected: f64) {
    let period = world.oc_ref().average_period().await.unwrap();
    assert!(
        (period - expected).abs() < 0.001,
        "expected average period ~{expected}, got {period}"
    );
}

#[then(expr = "the average period should be {float} hours")]
async fn average_period_should_be(world: &mut Upbv2World, expected: f64) {
    let period = world.oc_ref().average_period().await.unwrap();
    assert!(
        (period - expected).abs() < f64::EPSILON,
        "expected average period {expected}, got {period}"
    );
}

#[then(expr = "the temperature should be approximately {float}")]
async fn temperature_should_be_approximately(world: &mut Upbv2World, expected: f64) {
    let temp = world.oc_ref().temperature().await.unwrap();
    assert!(
        (temp - expected).abs() < 0.1,
        "expected temperature ~{expected}, got {temp}"
    );
}

#[then(expr = "the humidity should be approximately {float}")]
async fn humidity_should_be_approximately(world: &mut Upbv2World, expected: f64) {
    let humidity = world.oc_ref().humidity().await.unwrap();
    assert!(
        (humidity - expected).abs() < 0.1,
        "expected humidity ~{expected}, got {humidity}"
    );
}

#[then(expr = "the dewpoint should be approximately {float}")]
async fn dewpoint_should_be_approximately(world: &mut Upbv2World, expected: f64) {
    let dewpoint = world.oc_ref().dew_point().await.unwrap();
    assert!(
        (dewpoint - expected).abs() < 0.1,
        "expected dewpoint ~{expected}, got {dewpoint}"
    );
}

#[then(expr = "sensor description for {string} should contain {string}")]
async fn sensor_description_should_contain(
    world: &mut Upbv2World,
    sensor: String,
    expected: String,
) {
    let desc = world
        .oc_ref()
        .sensor_description(sensor.clone())
        .await
        .unwrap();
    assert!(
        desc.to_lowercase().contains(&expected.to_lowercase()),
        "expected sensor description for '{sensor}' to contain '{expected}', got: {desc}"
    );
}

#[then(expr = "sensor description for {string} and {string} should match")]
async fn sensor_description_case_insensitive(
    world: &mut Upbv2World,
    sensor1: String,
    sensor2: String,
) {
    let desc1 = world
        .oc_ref()
        .sensor_description(sensor1.clone())
        .await
        .unwrap();
    let desc2 = world
        .oc_ref()
        .sensor_description(sensor2.clone())
        .await
        .unwrap();
    assert_eq!(
        desc1, desc2,
        "sensor descriptions for '{sensor1}' and '{sensor2}' should match"
    );
}

#[then(expr = "time since last update for {string} should be less than {float} seconds")]
async fn time_since_last_update_less_than(world: &mut Upbv2World, sensor: String, max_time: f64) {
    let time = world
        .oc_ref()
        .time_since_last_update(sensor.clone())
        .await
        .unwrap();
    assert!(
        time < max_time,
        "expected time < {max_time}, got {time} for sensor '{sensor}'"
    );
}

#[then("time since last update should return NOT_IMPLEMENTED for all unimplemented sensors")]
async fn time_since_last_update_not_implemented_for_all(world: &mut Upbv2World) {
    let sensors = [
        "cloudcover",
        "pressure",
        "rainrate",
        "skybrightness",
        "skyquality",
        "starfwhm",
        "skytemperature",
        "winddirection",
        "windgust",
        "windspeed",
    ];
    for sensor in sensors {
        let err = world
            .oc_ref()
            .time_since_last_update(sensor.to_string())
            .await
            .unwrap_err();
        assert_eq!(
            err.code,
            ASCOMErrorCode::NOT_IMPLEMENTED,
            "expected NOT_IMPLEMENTED for timesincelastupdate('{}'), got {:?}",
            sensor,
            err.code
        );
    }
}

#[then("sensor description should return NOT_IMPLEMENTED for all unimplemented sensors")]
async fn sensor_description_not_implemented_for_all(world: &mut Upbv2World) {
    let sensors = [
        "cloudcover",
        "pressure",
        "rainrate",
        "skybrightness",
        "skyquality",
        "starfwhm",
        "skytemperature",
        "winddirection",
        "windgust",
        "windspeed",
    ];
    for sensor in sensors {
        let err = world
            .oc_ref()
            .sensor_description(sensor.to_string())
            .await
            .unwrap_err();
        assert_eq!(
            err.code,
            ASCOMErrorCode::NOT_IMPLEMENTED,
            "expected NOT_IMPLEMENTED for sensordescription('{}'), got {:?}",
            sensor,
            err.code
        );
    }
}

#[then("refreshing the OC device should succeed")]
async fn refreshing_oc_device_should_succeed(world: &mut Upbv2World) {
    world.oc_ref().refresh().await.unwrap();
}
