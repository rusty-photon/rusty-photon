//! BDD step definitions for device identity pins (rp.md § Device
//! Identity Pin): every equipment kind binds only the device that
//! reports its pinned `UniqueID`, the equipment status names the bound
//! device, and a pinned camera whose number comes back addressing
//! another camera is refused on reconnect.

use std::time::Duration;

use cucumber::{given, then, when};
use serde_json::Value;

use bdd_infra::rp_harness::{
    AlpacaDeviceStub, CameraConfig, CoverCalibratorConfig, DomeConfig, FilterWheelConfig,
    FocuserConfig, MountConfig, ObservingConditionsConfig, RotatorConfig, SafetyMonitorConfig,
    StubDevice, SwitchConfig,
};

use crate::world::RpWorld;

/// How long a poll-until-observed step waits before failing the
/// scenario. A floor, not an estimate: every loop exits as soon as its
/// condition holds.
const OBSERVATION_BUDGET: Duration = Duration::from_secs(15);

/// The pin the refused rows use: no device on the simulator lists it.
const UNLISTED_UNIQUE_ID: &str = "not-listed-by-the-simulator";

/// One equipment kind as the outline names it, and where rp's config and
/// status and the simulator's listing keep it.
struct Kind {
    /// The equipment key in rp's config and `GET /api/equipment`
    /// (`"mount"` for the singular mount).
    key: &'static str,
    /// The roster id these scenarios give the entry.
    id: &'static str,
    /// The Alpaca `DeviceType` the simulator lists it under.
    device_type: &'static str,
}

fn kind(name: &str) -> Kind {
    let (key, id, device_type) = match name {
        "camera" => ("cameras", "pinned-camera", "Camera"),
        "filter wheel" => ("filter_wheels", "pinned-filter-wheel", "FilterWheel"),
        "focuser" => ("focusers", "pinned-focuser", "Focuser"),
        "cover calibrator" => (
            "cover_calibrators",
            "pinned-cover-calibrator",
            "CoverCalibrator",
        ),
        "safety monitor" => ("safety_monitors", "pinned-safety-monitor", "SafetyMonitor"),
        "switch" => ("switches", "pinned-switch", "Switch"),
        "rotator" => ("rotators", "pinned-rotator", "Rotator"),
        "observing conditions device" => (
            "observing_conditions",
            "pinned-observing-conditions",
            "ObservingConditions",
        ),
        "dome" => ("domes", "pinned-dome", "Dome"),
        "mount" => ("mount", "mount", "Telescope"),
        other => panic!("unknown equipment kind {other:?}"),
    };
    Kind {
        key,
        id,
        device_type,
    }
}

/// Add a device of `kind` at the simulator's device 0 to rp's roster.
fn configure_on_simulator(world: &mut RpWorld, kind: &Kind) {
    let url = world.omnisim_url();
    let id = kind.id.to_string();
    match kind.key {
        "cameras" => world.cameras.push(CameraConfig {
            id,
            alpaca_url: url,
            device_number: 0,
            cooler_targets_c: Vec::new(),
        }),
        "filter_wheels" => world.filter_wheels.push(FilterWheelConfig {
            id,
            alpaca_url: url,
            device_number: 0,
            filters: vec!["Luminance".to_string()],
            wavelengths_nm: Vec::new(),
        }),
        "focusers" => world.focusers.push(FocuserConfig {
            id,
            alpaca_url: url,
            device_number: 0,
            min_position: None,
            max_position: None,
            backlash: None,
            microns_per_step: None,
        }),
        "cover_calibrators" => world.cover_calibrators.push(CoverCalibratorConfig {
            id,
            alpaca_url: url,
            device_number: 0,
            poll_interval: None,
        }),
        "safety_monitors" => world.safety_monitors.push(SafetyMonitorConfig {
            id,
            alpaca_url: url,
            device_number: 0,
        }),
        "switches" => world.switches.push(SwitchConfig {
            id,
            alpaca_url: url,
            device_number: 0,
        }),
        "rotators" => world.rotators.push(RotatorConfig {
            id,
            alpaca_url: url,
            device_number: 0,
        }),
        "observing_conditions" => world.observing_conditions.push(ObservingConditionsConfig {
            id,
            alpaca_url: url,
            device_number: 0,
        }),
        "domes" => world.domes.push(DomeConfig {
            id,
            alpaca_url: url,
            device_number: 0,
        }),
        "mount" => {
            world.mount = Some(MountConfig {
                alpaca_url: url,
                device_number: 0,
                settle_after_slew: None,
            });
        }
        other => panic!("no roster builder for {other:?}"),
    }
}

/// The `UniqueID` the simulator lists for its first device of
/// `device_type` — device 0, the one rp binds.
async fn simulator_unique_id(world: &RpWorld, device_type: &str) -> String {
    let url = format!("{}/management/v1/configureddevices", world.omnisim_url());
    let body: Value = reqwest::get(&url)
        .await
        .expect("failed to GET the simulator's configureddevices")
        .json()
        .await
        .expect("configureddevices is not JSON");
    body["Value"]
        .as_array()
        .expect("configureddevices has no Value array")
        .iter()
        .find(|d| d["DeviceType"] == device_type)
        .and_then(|d| d["UniqueID"].as_str())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| panic!("the simulator lists no {device_type} with a UniqueID: {body}"))
        .to_string()
}

/// One read of an entry's `GET /api/equipment` status object. `None`
/// when the request failed or the entry is absent.
async fn entry_status(world: &RpWorld, key: &str, id: &str) -> Option<Value> {
    let url = format!("{}/api/equipment", world.rp_url());
    let body: Value = reqwest::get(&url).await.ok()?.json().await.ok()?;
    if key == "mount" {
        return body.get("mount").filter(|m| !m.is_null()).cloned();
    }
    body.get(key)?
        .as_array()?
        .iter()
        .find(|d| d.get("id").and_then(Value::as_str) == Some(id))
        .cloned()
}

/// Poll the entry's status until `accept` holds, failing the scenario
/// with the last status seen after [`OBSERVATION_BUDGET`].
async fn poll_entry_status(
    world: &RpWorld,
    key: &str,
    id: &str,
    expectation: &str,
    accept: impl Fn(&Value) -> bool,
) {
    let deadline = std::time::Instant::now() + OBSERVATION_BUDGET;
    loop {
        let status = entry_status(world, key, id).await;
        if status.as_ref().is_some_and(&accept) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{key} {id} never showed {expectation} within {OBSERVATION_BUDGET:?}; last status: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

const fn stub(world: &RpWorld) -> &AlpacaDeviceStub {
    world
        .alpaca_stub
        .as_ref()
        .expect("no stub Alpaca service — add a 'Given a stub Alpaca service ...' step")
}

// --- Given steps ---

#[given(
    regex = r"^rp is configured with the simulator's (.+) pinned to (the UniqueID the simulator lists for it|a UniqueID the simulator does not list)$"
)]
async fn configured_with_pinned_simulator_device(world: &mut RpWorld, name: String, pin: String) {
    let kind = kind(&name);
    configure_on_simulator(world, &kind);
    let unique_id = if pin.starts_with("the UniqueID") {
        simulator_unique_id(world, kind.device_type).await
    } else {
        UNLISTED_UNIQUE_ID.to_string()
    };
    world
        .unique_id_pins
        .push((kind.key.to_string(), kind.id.to_string(), unique_id.clone()));
    world.pinned_unique_id = Some(unique_id);
}

#[given(expr = "a stub Alpaca service hosting a camera with UniqueID {string}")]
async fn stub_hosting_camera_as(world: &mut RpWorld, unique_id: String) {
    world.alpaca_stub = Some(AlpacaDeviceStub::start_as(StubDevice::Camera, &unique_id));
}

#[given(expr = "the camera {string} is pinned to UniqueID {string}")]
fn camera_pinned(world: &mut RpWorld, id: String, unique_id: String) {
    world
        .unique_id_pins
        .push(("cameras".to_string(), id, unique_id));
}

// --- When steps ---

#[when(expr = "the stub Alpaca service comes back hosting a camera with UniqueID {string}")]
async fn stub_comes_back_as(world: &mut RpWorld, unique_id: String) {
    world
        .alpaca_stub
        .as_mut()
        .expect("no stub Alpaca service — add a 'Given a stub Alpaca service ...' step")
        .restart_as(&unique_id)
        .await;
}

// --- Then steps ---

#[then(regex = r"^the equipment status should show the pinned (.+) bound to that UniqueID$")]
async fn pinned_device_bound(world: &mut RpWorld, name: String) {
    let kind = kind(&name);
    let expected = world
        .pinned_unique_id
        .clone()
        .expect("no pin recorded — add the 'rp is configured with the simulator's ...' step");
    let status = entry_status(world, kind.key, kind.id)
        .await
        .unwrap_or_else(|| panic!("{} {} missing from GET /api/equipment", kind.key, kind.id));
    assert_eq!(
        status["connected"], true,
        "expected the pinned {name} to bind: {status}"
    );
    assert_eq!(
        status["unique_id"], expected,
        "the pinned {name} must report the identity it was bound to: {status}"
    );
}

#[then(regex = r"^the equipment status should show the pinned (.+) refused and unbound$")]
async fn pinned_device_refused(world: &mut RpWorld, name: String) {
    let kind = kind(&name);
    let status = entry_status(world, kind.key, kind.id)
        .await
        .unwrap_or_else(|| panic!("{} {} missing from GET /api/equipment", kind.key, kind.id));
    let mut expected = serde_json::json!({
        "connected": false,
        "device_name": null,
        "unique_id": null,
    });
    if kind.key != "mount" {
        expected["id"] = Value::String(kind.id.to_string());
    }
    assert_eq!(
        status, expected,
        "expected the {name} pinned to an unlisted UniqueID to be refused"
    );
}

#[then(
    expr = "the equipment status should show camera {string} bound to {string} with UniqueID {string}"
)]
async fn camera_bound_to_named_device(
    world: &mut RpWorld,
    id: String,
    device_name: String,
    unique_id: String,
) {
    poll_entry_status(
        world,
        "cameras",
        &id,
        &format!("a binding to {device_name:?} ({unique_id:?})"),
        |status| {
            status["connected"] == true
                && status["device_name"] == device_name.as_str()
                && status["unique_id"] == unique_id.as_str()
        },
    )
    .await;
}

#[then(expr = "the equipment status should show camera {string} bound to UniqueID {string}")]
async fn camera_bound_to(world: &mut RpWorld, id: String, unique_id: String) {
    poll_entry_status(
        world,
        "cameras",
        &id,
        &format!("a binding to {unique_id:?}"),
        |status| status["connected"] == true && status["unique_id"] == unique_id.as_str(),
    )
    .await;
}

/// The negative half of a refused reconnect, made observable: the stub
/// counts every `configureddevices` read, so once rp has looked the
/// camera up `reads` times since the restart, that many re-establish
/// attempts have run. None of them may have switched the camera on —
/// rp never sends `Connected = false`, so a camera it had switched on
/// would still read connected here.
#[then(
    expr = "rp has looked the stub's camera up {int} times since it came back without switching it on"
)]
async fn looked_up_without_switching_on(world: &mut RpWorld, reads: u32) {
    let deadline = std::time::Instant::now() + OBSERVATION_BUDGET;
    while stub(world).device_list_reads() < reads {
        assert!(
            std::time::Instant::now() < deadline,
            "rp read the stub's device list only {} times within {OBSERVATION_BUDGET:?}",
            stub(world).device_list_reads()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !stub(world).is_connected(),
        "rp switched on a camera whose UniqueID its pin refuses"
    );
}
