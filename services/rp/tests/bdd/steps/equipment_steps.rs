//! BDD step definitions for equipment connectivity feature

use cucumber::{given, then, when};

use bdd_infra::rp_harness::{
    CameraConfig, DomeConfig, FilterWheelConfig, ObservingConditionsConfig, OmniSimHandle,
    RotatorConfig, SwitchConfig,
};
use bdd_infra::ServiceHandle;

use crate::world::RpWorld;

// --- Given steps ---

#[given("a running Alpaca simulator")]
async fn running_alpaca_simulator(world: &mut RpWorld) {
    if world.omnisim.is_none() {
        world.omnisim = Some(OmniSimHandle::start().await);
    }
}

#[given("rp is configured with a camera on the simulator")]
fn configured_with_camera(world: &mut RpWorld) {
    let url = world.omnisim_url();
    world.cameras.push(CameraConfig {
        id: "main-cam".to_string(),
        alpaca_url: url,
        device_number: 0,
        cooler_targets_c: Vec::new(),
    });
}

#[given("rp is configured with a filter wheel on the simulator")]
fn configured_with_filter_wheel(world: &mut RpWorld) {
    let url = world.omnisim_url();
    world.filter_wheels.push(FilterWheelConfig {
        wavelengths_nm: vec![],
        id: "main-fw".to_string(),
        alpaca_url: url,
        device_number: 0,
        filters: vec![
            "Luminance".to_string(),
            "Red".to_string(),
            "Green".to_string(),
            "Blue".to_string(),
        ],
    });
}

#[given(expr = "rp is configured with a camera at {string} device {int}")]
fn configured_with_camera_at(world: &mut RpWorld, url: String, device_number: i32) {
    world.cameras.push(CameraConfig {
        id: "main-cam".to_string(),
        alpaca_url: url,
        device_number: device_number.cast_unsigned(),
        cooler_targets_c: Vec::new(),
    });
}

#[given(expr = "rp is configured with a filter wheel at {string} device {int}")]
fn configured_with_filter_wheel_at(world: &mut RpWorld, url: String, device_number: i32) {
    world.filter_wheels.push(FilterWheelConfig {
        wavelengths_nm: vec![],
        id: "main-fw".to_string(),
        alpaca_url: url,
        device_number: device_number.cast_unsigned(),
        filters: vec![
            "Luminance".to_string(),
            "Red".to_string(),
            "Green".to_string(),
            "Blue".to_string(),
        ],
    });
}

#[given(expr = "rp is configured with a camera at the simulator device {int}")]
fn configured_with_camera_at_simulator_device(world: &mut RpWorld, device_number: i32) {
    let url = world.omnisim_url();
    world.cameras.push(CameraConfig {
        id: "main-cam".to_string(),
        alpaca_url: url,
        device_number: device_number.cast_unsigned(),
        cooler_targets_c: Vec::new(),
    });
}

#[given(expr = "rp is configured with a filter wheel at the simulator device {int}")]
fn configured_with_filter_wheel_at_simulator_device(world: &mut RpWorld, device_number: i32) {
    let url = world.omnisim_url();
    world.filter_wheels.push(FilterWheelConfig {
        wavelengths_nm: vec![],
        id: "main-fw".to_string(),
        alpaca_url: url,
        device_number: device_number.cast_unsigned(),
        filters: vec![
            "Luminance".to_string(),
            "Red".to_string(),
            "Green".to_string(),
            "Blue".to_string(),
        ],
    });
}

#[given("rp is configured with a switch on the simulator")]
fn configured_with_switch(world: &mut RpWorld) {
    let url = world.omnisim_url();
    world.switches.push(SwitchConfig {
        id: "main-switch".to_string(),
        alpaca_url: url,
        device_number: 0,
    });
}

#[given(expr = "rp is configured with a switch at {string} device {int}")]
fn configured_with_switch_at(world: &mut RpWorld, url: String, device_number: i32) {
    world.switches.push(SwitchConfig {
        id: "main-switch".to_string(),
        alpaca_url: url,
        device_number: device_number.cast_unsigned(),
    });
}

#[given("rp is configured with a rotator on the simulator")]
fn configured_with_rotator(world: &mut RpWorld) {
    let url = world.omnisim_url();
    world.rotators.push(RotatorConfig {
        id: "main-rotator".to_string(),
        alpaca_url: url,
        device_number: 0,
    });
}

#[given(expr = "rp is configured with a rotator at {string} device {int}")]
fn configured_with_rotator_at(world: &mut RpWorld, url: String, device_number: i32) {
    world.rotators.push(RotatorConfig {
        id: "main-rotator".to_string(),
        alpaca_url: url,
        device_number: device_number.cast_unsigned(),
    });
}

#[given("rp is configured with an observing conditions device on the simulator")]
fn configured_with_observing_conditions(world: &mut RpWorld) {
    let url = world.omnisim_url();
    world.observing_conditions.push(ObservingConditionsConfig {
        id: "main-oc".to_string(),
        alpaca_url: url,
        device_number: 0,
    });
}

#[given(expr = "rp is configured with an observing conditions device at {string} device {int}")]
fn configured_with_observing_conditions_at(world: &mut RpWorld, url: String, device_number: i32) {
    world.observing_conditions.push(ObservingConditionsConfig {
        id: "main-oc".to_string(),
        alpaca_url: url,
        device_number: device_number.cast_unsigned(),
    });
}

#[given("rp is configured with a dome on the simulator")]
fn configured_with_dome(world: &mut RpWorld) {
    let url = world.omnisim_url();
    world.domes.push(DomeConfig {
        id: "main-dome".to_string(),
        alpaca_url: url,
        device_number: 0,
    });
}

#[given(expr = "rp is configured with a dome at {string} device {int}")]
fn configured_with_dome_at(world: &mut RpWorld, url: String, device_number: i32) {
    world.domes.push(DomeConfig {
        id: "main-dome".to_string(),
        alpaca_url: url,
        device_number: device_number.cast_unsigned(),
    });
}

// --- When steps ---

#[when("rp starts")]
async fn rp_starts(world: &mut RpWorld) {
    let config = world.build_config();
    let config_path = std::env::temp_dir()
        .join(format!(
            "rp-test-config-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
        .to_string_lossy()
        .to_string();
    tokio::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap())
        .await
        .expect("failed to write config");

    world.rp = Some(ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await);

    assert!(
        world.wait_for_rp_healthy().await,
        "rp did not become healthy within timeout"
    );
}

// --- Then steps ---

#[then("the equipment status should show the camera as connected")]
async fn camera_should_be_connected(world: &mut RpWorld) {
    let client = reqwest::Client::new();
    let url = format!("{}/api/equipment", world.rp_url());
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /api/equipment");

    let body: serde_json::Value = resp
        .json()
        .await
        .expect("failed to parse equipment response");

    let cameras = body
        .get("cameras")
        .and_then(|v| v.as_array())
        .expect("no cameras array in equipment response");

    let cam = cameras
        .iter()
        .find(|c| c.get("id").and_then(|v| v.as_str()) == Some("main-cam"))
        .expect("main-cam not found in equipment response");

    assert_eq!(
        cam.get("connected").and_then(serde_json::Value::as_bool),
        Some(true),
        "expected main-cam to be connected, got: {cam:?}"
    );
}

#[then("the equipment status should show the filter wheel as connected")]
async fn filter_wheel_should_be_connected(world: &mut RpWorld) {
    let client = reqwest::Client::new();
    let url = format!("{}/api/equipment", world.rp_url());
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /api/equipment");

    let body: serde_json::Value = resp
        .json()
        .await
        .expect("failed to parse equipment response");

    let filter_wheels = body
        .get("filter_wheels")
        .and_then(|v| v.as_array())
        .expect("no filter_wheels array in equipment response");

    let fw = filter_wheels
        .iter()
        .find(|f| f.get("id").and_then(|v| v.as_str()) == Some("main-fw"))
        .expect("main-fw not found in equipment response");

    assert_eq!(
        fw.get("connected").and_then(serde_json::Value::as_bool),
        Some(true),
        "expected main-fw to be connected, got: {fw:?}"
    );
}

#[then("rp should be healthy")]
async fn rp_should_be_healthy(world: &mut RpWorld) {
    let client = reqwest::Client::new();
    let url = format!("{}/health", world.rp_url());
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /health");
    assert_eq!(resp.status().as_u16(), 200, "expected rp to be healthy");
}

#[then("the equipment status should show the filter wheel as disconnected")]
async fn filter_wheel_should_be_disconnected(world: &mut RpWorld) {
    let client = reqwest::Client::new();
    let url = format!("{}/api/equipment", world.rp_url());
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /api/equipment");

    let body: serde_json::Value = resp
        .json()
        .await
        .expect("failed to parse equipment response");

    let filter_wheels = body
        .get("filter_wheels")
        .and_then(|v| v.as_array())
        .expect("no filter_wheels array in equipment response");

    let fw = filter_wheels
        .iter()
        .find(|f| f.get("id").and_then(|v| v.as_str()) == Some("main-fw"))
        .expect("main-fw not found in equipment response");

    assert_eq!(
        fw.get("connected").and_then(serde_json::Value::as_bool),
        Some(false),
        "expected main-fw to be disconnected, got: {fw:?}"
    );
}

#[then("the equipment status should show the camera as disconnected")]
async fn camera_should_be_disconnected(world: &mut RpWorld) {
    let client = reqwest::Client::new();
    let url = format!("{}/api/equipment", world.rp_url());
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /api/equipment");

    let body: serde_json::Value = resp
        .json()
        .await
        .expect("failed to parse equipment response");

    let cameras = body
        .get("cameras")
        .and_then(|v| v.as_array())
        .expect("no cameras array in equipment response");

    let cam = cameras
        .iter()
        .find(|c| c.get("id").and_then(|v| v.as_str()) == Some("main-cam"))
        .expect("main-cam not found in equipment response");

    assert_eq!(
        cam.get("connected").and_then(serde_json::Value::as_bool),
        Some(false),
        "expected main-cam to be disconnected, got: {cam:?}"
    );
}

/// Fetch `GET /api/equipment`, find the entry with `id` in the array at
/// `array_key`, and assert its `connected` flag equals `expected`. Shared by
/// the switch/rotator/observing-conditions/dome connected/disconnected Then
/// steps below — the `camera/filter_wheel` steps above predate this helper and
/// keep their own inline bodies.
async fn assert_device_connected(world: &RpWorld, array_key: &str, id: &str, expected: bool) {
    let client = reqwest::Client::new();
    let url = format!("{}/api/equipment", world.rp_url());
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /api/equipment")
        .error_for_status()
        .expect("GET /api/equipment returned a non-success status");

    let body: serde_json::Value = resp
        .json()
        .await
        .expect("failed to parse equipment response");

    let devices = body
        .get(array_key)
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("no {array_key} array in equipment response"));

    let device = devices
        .iter()
        .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(id))
        .unwrap_or_else(|| panic!("{id} not found in {array_key} in equipment response"));

    assert_eq!(
        device.get("connected").and_then(serde_json::Value::as_bool),
        Some(expected),
        "expected {id} connected to be {expected}, got: {device:?}"
    );
}

#[then("the equipment status should show the switch as connected")]
async fn switch_should_be_connected(world: &mut RpWorld) {
    assert_device_connected(world, "switches", "main-switch", true).await;
}

#[then("the equipment status should show the switch as disconnected")]
async fn switch_should_be_disconnected(world: &mut RpWorld) {
    assert_device_connected(world, "switches", "main-switch", false).await;
}

#[then("the equipment status should show the rotator as connected")]
async fn rotator_should_be_connected(world: &mut RpWorld) {
    assert_device_connected(world, "rotators", "main-rotator", true).await;
}

#[then("the equipment status should show the rotator as disconnected")]
async fn rotator_should_be_disconnected(world: &mut RpWorld) {
    assert_device_connected(world, "rotators", "main-rotator", false).await;
}

#[then("the equipment status should show the observing conditions device as connected")]
async fn observing_conditions_should_be_connected(world: &mut RpWorld) {
    assert_device_connected(world, "observing_conditions", "main-oc", true).await;
}

#[then("the equipment status should show the observing conditions device as disconnected")]
async fn observing_conditions_should_be_disconnected(world: &mut RpWorld) {
    assert_device_connected(world, "observing_conditions", "main-oc", false).await;
}

#[then("the equipment status should show the dome as connected")]
async fn dome_should_be_connected(world: &mut RpWorld) {
    assert_device_connected(world, "domes", "main-dome", true).await;
}

#[then("the equipment status should show the dome as disconnected")]
async fn dome_should_be_disconnected(world: &mut RpWorld) {
    assert_device_connected(world, "domes", "main-dome", false).await;
}
