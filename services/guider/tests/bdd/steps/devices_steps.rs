//! Steps of `devices.feature` and `starting_idle.feature`: the simulated
//! devices a scenario scripts, and what the guider reports of them.

use cucumber::gherkin::Step;
use cucumber::{given, then, when};
use guide_simulator::{BayerResponse, SensorType};
use guide_simulator_alpaca::Device;
use serde_json::Value;

use crate::steps::config_steps::json_value;
use crate::world::GuiderWorld;

#[given("the simulated camera and mount, both connected")]
fn both_connected(world: &mut GuiderWorld) {
    world.sim_config();
    world.camera_connected = true;
    world.mount_connected = true;
}

#[given("the simulated camera and mount, neither connected")]
fn neither_connected(world: &mut GuiderWorld) {
    world.sim_config();
    world.camera_connected = false;
    world.mount_connected = false;
}

#[given("the simulated camera not connected and the simulated mount connected")]
fn only_the_mount_connected(world: &mut GuiderWorld) {
    world.sim_config();
    world.camera_connected = false;
    world.mount_connected = true;
}

#[given(regex = r"^the simulated camera has a (monochrome|RGGB) sensor$")]
fn camera_sensor(world: &mut GuiderWorld, sensor: String) {
    world.sim_config().sim.camera.sensor = match sensor.as_str() {
        "RGGB" => SensorType::Rggb(BayerResponse {
            red: 1.0,
            green: 1.0,
            blue: 1.0,
        }),
        _ => SensorType::Monochrome,
    };
}

#[given("the simulated camera cannot abort an exposure")]
fn cannot_abort(world: &mut GuiderWorld) {
    world.sim_config().camera.can_abort_exposure = false;
}

#[given("the simulated mount cannot pulse guide")]
fn cannot_pulse_guide(world: &mut GuiderWorld) {
    world.sim_config().mount.can_pulse_guide = false;
}

#[given(expr = "the simulated camera is listed with the UniqueID {string}")]
fn camera_unique_id(world: &mut GuiderWorld, unique_id: String) {
    world.sim_config().camera.unique_id = unique_id;
}

#[when("the simulated camera is connected")]
fn connect_the_camera(world: &mut GuiderWorld) {
    world.sim().set_connected(Device::Camera, true);
}

#[given(regex = r"^the (camera|mount)'s server stops answering$")]
#[when(regex = r"^the (camera|mount)'s server stops answering$")]
fn stops_answering(world: &mut GuiderWorld, device: String) {
    world.sim().set_reachable(device_named(&device), false);
}

#[when(regex = r"^the (camera|mount)'s server answers again$")]
#[then(regex = r"^the (camera|mount)'s server answers again$")]
fn answers_again(world: &mut GuiderWorld, device: String) {
    world.sim().set_reachable(device_named(&device), true);
}

#[given(
    regex = r"^the guider reports the (camera|mount) (connected|reachable and not connected|not reachable)$"
)]
#[when(
    regex = r"^the guider reports the (camera|mount) (connected|reachable and not connected|not reachable)$"
)]
#[then(
    regex = r"^the guider reports the (camera|mount) (connected|reachable and not connected|not reachable)$"
)]
async fn guider_reports(world: &mut GuiderWorld, device: String, state: String) {
    let (reachable, connected) = match state.as_str() {
        "connected" => (true, true),
        "reachable and not connected" => (true, false),
        _ => (false, false),
    };
    world
        .wait_for(
            "/api/v1/devices",
            &format!("the {device} {state}"),
            |status, body| {
                status == 200
                    && body[device.as_str()]["reachable"] == Value::Bool(reachable)
                    && body[device.as_str()]["connected"] == Value::Bool(connected)
            },
        )
        .await;
}

#[then(regex = r"^the devices answer reports for the (camera|mount):$")]
async fn devices_answer_reports(world: &mut GuiderWorld, device: String, step: &Step) {
    let (status, body) = world
        .get("/api/v1/devices")
        .await
        .expect("GET /api/v1/devices");
    assert_eq!(status, 200, "GET /api/v1/devices answered {status}: {body}");
    let table = step.table.as_ref().expect("a table of fields and values");
    for row in table.rows.iter().skip(1) {
        let (field, expected) = (&row[0], json_value(&row[1]));
        assert_eq!(
            body[device.as_str()][field.as_str()],
            expected,
            "the {device}'s {field} in {body}"
        );
    }
}

#[then(regex = r"^the devices answer reports the (camera|mount|rotator) as null$")]
async fn reported_as_null(world: &mut GuiderWorld, device: String) {
    let (status, body) = world
        .get("/api/v1/devices")
        .await
        .expect("GET /api/v1/devices");
    assert_eq!(status, 200, "GET /api/v1/devices answered {status}: {body}");
    let object = body.as_object().expect("the devices answer is an object");
    assert_eq!(
        object.get(device.as_str()),
        Some(&Value::Null),
        "the {device} in {body}"
    );
}

#[then("the simulated camera has started no exposure")]
fn no_exposure(world: &mut GuiderWorld) {
    assert_eq!(world.sim().exposures(), []);
}

#[then("the simulated mount has received no pulse")]
fn no_pulse(world: &mut GuiderWorld) {
    assert_eq!(world.sim().pulses(), []);
}

#[then(regex = r"^the simulated (camera|mount) is not connected$")]
async fn sim_not_connected(world: &mut GuiderWorld, device: String) {
    assert!(
        !world.sim_connected(device_named(&device)).await,
        "the simulated {device} is connected"
    );
}

fn device_named(name: &str) -> Device {
    match name {
        "camera" => Device::Camera,
        _ => Device::Mount,
    }
}
