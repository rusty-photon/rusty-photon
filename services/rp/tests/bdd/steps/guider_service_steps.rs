//! Steps of `guider_service.feature`: rp against the guider service
//! itself, the `guider` binary guiding `guide-simulator-alpaca`'s camera
//! and mount (docs/services/guider.md § Testing). The other guiding
//! features run against `bdd_infra::rp_harness::GuiderStub`.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bdd_infra::rp_harness::{CameraConfig, GuiderConfig, MountConfig, OpticalTrainConfig};
use bdd_infra::ServiceHandle;
use cucumber::{given, then, when};
use guide_simulator::{
    CameraConfig as SimCameraConfig, GuideDirection, PierSide, Seeing, SimConfig, SkyConfig,
    SkyOffset, Star, StarField,
};
use guide_simulator_alpaca::{DevicesConfig, SimDevices};
use serde_json::{json, Map, Value};
use tempfile::TempDir;

use crate::steps::guider_steps::{call_guider_tool, last_guider_result};
use crate::steps::tool_steps::{add_camera, start_rp};
use crate::world::RpWorld;

/// How long a step waits for the guider or the simulated devices to show
/// what the scenario expects. It bounds a hang and asserts no timing
/// (docs/skills/testing.md §5.9).
const WAIT_BUDGET: Duration = Duration::from_secs(60);

/// The deadline of each read a step sends, well inside [`WAIT_BUDGET`] so
/// that a wait gets several attempts.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The simulated sky's seeing, as a full width at half maximum: three
/// pixels at the simulated 2″ a pixel.
const SEEING_ARCSEC: f64 = 6.0;

/// The stars of the simulated sky, in arcseconds east and north of the
/// frame's centre: at 2″ a pixel they lie at least 60 pixels apart and
/// at least 45 pixels inside every edge of the 320 × 240 frame, beyond
/// the margin the guider's selection keeps. Each stands far above the
/// noise in a 200 ms exposure and far below the clip.
const STARS: [(f64, f64); 3] = [(0.0, 0.0), (-120.0, -60.0), (120.0, 60.0)];
const STAR_E_PER_S: f64 = 50_000.0;

/// The focal length the guider's configuration gives: with the simulated
/// camera's 2 µm pixels, 2″ a pixel.
const FOCAL_LENGTH_MM: f64 = 206.265;

/// The guider binary a scenario runs, the simulated devices it guides,
/// and the directory its configuration and state directory live in.
pub struct GuiderService {
    sim: SimDevices,
    guider: ServiceHandle,
    _scratch: TempDir,
}

impl GuiderService {
    /// Start the simulated devices, neither connected, and the guider on a
    /// configuration naming both.
    async fn start() -> Self {
        let sim = SimDevices::start(devices_config())
            .await
            .expect("start the simulated devices");
        let scratch = bdd_infra::scratch::new_dir("rp-guider-service-")
            .expect("create the guider's scratch dir");
        let config = json!({
            "server": { "port": 0, "bind_address": "127.0.0.1" },
            "state_dir": scratch.path().join("state"),
            "devices": {
                "camera": { "alpaca_url": sim.camera_url() },
                "mount": { "alpaca_url": sim.mount_url() },
                "poll_interval": "100ms"
            },
            "camera": { "exposure": "200ms" },
            // The call deadline's ten seconds rather than the default two:
            // the simulated devices share the test process, so on a loaded
            // machine a `PulseGuide` call can take seconds to come back.
            "mount": { "pulse_timeout": "10s" },
            "optics": { "focal_length_mm": FOCAL_LENGTH_MM },
            "calibration": {
                "distance_px": 6,
                "steps": 4,
                "max_steps": 8,
                "backlash_clear_px": 1
            },
            "settling": { "pixels": 1.5, "time": "1s", "timeout": "30s" }
        });
        let path = scratch.path().join("guider.json");
        std::fs::write(&path, config.to_string()).expect("write the guider's config");
        let guider = ServiceHandle::start("guider", &path.to_string_lossy()).await;
        Self {
            sim,
            guider,
            _scratch: scratch,
        }
    }

    /// The guider's base URL.
    fn url(&self) -> String {
        self.guider.base_url()
    }

    /// Stop the guider, then the simulated devices.
    pub async fn stop(mut self) {
        self.guider.stop().await;
        self.sim.stop().await;
    }
}

/// A 320 × 240 camera of 2 µm pixels under a sky of three stars in 6″
/// seeing, and a mount on the East side at declination 0°, tracking, that
/// guides at the sidereal rate.
fn devices_config() -> DevicesConfig {
    let mut sim = SimConfig {
        camera: SimCameraConfig {
            width: 320,
            height: 240,
            ..SimCameraConfig::default()
        },
        sky: SkyConfig {
            seeing: Seeing {
                fwhm_arcsec: SEEING_ARCSEC,
                ..SkyConfig::default().seeing
            },
            stars: StarField::Listed(
                STARS
                    .iter()
                    .map(|&(east, north)| Star {
                        position: SkyOffset { east, north },
                        e_per_s: STAR_E_PER_S,
                    })
                    .collect(),
            ),
            ..SkyConfig::default()
        },
        ..SimConfig::default()
    };
    sim.mount.guide_rate_ra = 1.0;
    sim.mount.guide_rate_dec = 1.0;
    DevicesConfig {
        sim,
        ..DevicesConfig::default()
    }
}

/// One client for every read the steps send the guider and the simulated
/// devices.
fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("build the steps' HTTP client")
    })
}

const fn service(world: &RpWorld) -> &GuiderService {
    world
        .guider_service
        .as_ref()
        .expect("the scenario runs no guider service")
}

/// Wait until `GET url` answers a JSON body `accept` takes. Panics with
/// the last answer, or the last failure, when [`WAIT_BUDGET`] runs out.
async fn wait_for_json(url: &str, what: &str, accept: impl Fn(&Value) -> bool) {
    let start = Instant::now();
    let mut last = String::from("no answer yet");
    while start.elapsed() < WAIT_BUDGET {
        match read_json(url).await {
            Ok(body) if accept(&body) => return,
            Ok(body) => last = body.to_string(),
            Err(error) => last = error,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("GET {url} did not show {what} within {WAIT_BUDGET:?}; last answer: {last}");
}

async fn read_json(url: &str) -> Result<Value, String> {
    let response = http().get(url).send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("{status}: the body is not JSON: {e}"))?;
    if status.is_success() {
        Ok(body)
    } else {
        Err(format!("{status} {body}"))
    }
}

/// `field` of a guider tool result, as a number.
fn result_number(result: &Value, field: &str) -> f64 {
    result
        .get(field)
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("expected a number at '{field}' in {result}"))
}

// --- Given steps --------------------------------------------------------

#[given("a guider service on the simulated camera and mount")]
async fn guider_service_on_the_simulated_devices(world: &mut RpWorld) {
    let service = GuiderService::start().await;
    world.guider = Some(GuiderConfig::url_only(service.url()));
    world.guider_service = Some(service);
}

#[given("rp is running with the simulated mount and the simulated camera in a guiding train")]
async fn rp_with_the_simulated_rig(world: &mut RpWorld) {
    add_simulated_mount(world);
    add_simulated_guide_camera(world);
    start_rp(world).await;
}

/// The simulator's camera, which the guider does not guide on, is the
/// guiding train's.
#[given("rp is running with the simulated mount and the simulator's camera in a guiding train")]
async fn rp_with_another_guide_camera(world: &mut RpWorld) {
    add_simulated_mount(world);
    add_camera(world);
    push_guiding_train(world, "main-cam");
    start_rp(world).await;
}

#[given(
    "rp is running with the simulated mount, the simulated camera in a guiding train and a camera on the simulator"
)]
async fn rp_with_the_simulated_rig_and_a_camera(world: &mut RpWorld) {
    add_simulated_mount(world);
    add_camera(world);
    add_simulated_guide_camera(world);
    start_rp(world).await;
}

/// `start_guiding` with no arguments has answered the state `guiding`.
#[given("rp has started guiding")]
async fn rp_has_started_guiding(world: &mut RpWorld) {
    call_guider_tool(world, "start_guiding", Map::new()).await;
    let outcome = world.last_tool_result.as_ref().expect("no tool result");
    let result = outcome
        .as_ref()
        .unwrap_or_else(|e| panic!("start_guiding failed: {e}"));
    assert_eq!(
        result.get("state").and_then(Value::as_str),
        Some("guiding"),
        "start_guiding did not answer guiding: {result}"
    );
}

fn add_simulated_mount(world: &mut RpWorld) {
    let url = service(world).sim.mount_url();
    world.mount = Some(MountConfig {
        alpaca_url: url,
        device_number: 0,
        settle_after_slew: None,
    });
}

/// The simulated camera as `guide-cam`, in the guiding train `guide`.
fn add_simulated_guide_camera(world: &mut RpWorld) {
    let url = service(world).sim.camera_url();
    world.cameras.push(CameraConfig {
        id: "guide-cam".to_string(),
        alpaca_url: url,
        device_number: 0,
        cooler_targets_c: Vec::new(),
    });
    push_guiding_train(world, "guide-cam");
}

fn push_guiding_train(world: &mut RpWorld, camera_id: &str) {
    world.optical_trains.push(OpticalTrainConfig {
        telescope: None,
        aperture_mm: None,
        obstruction_mm: None,
        id: "guide".to_string(),
        purpose: Some("guiding".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices: vec![camera_id.to_string()],
        auto_focus: None,
    });
}

// --- When steps: what happens to the simulated devices -------------------

#[given("the simulated mount is parked")]
fn simulated_mount_parked(world: &mut RpWorld) {
    service(world).sim.park();
}

#[when("the simulated mount starts a slew of an hour")]
fn simulated_mount_slews(world: &mut RpWorld) {
    service(world)
        .sim
        .slew(
            SkyOffset {
                east: 3_600.0,
                north: 0.0,
            },
            Duration::from_secs(3_600),
            PierSide::East,
        )
        .expect("slew the simulated mount");
}

/// The field moves at once, as a slew of no length moves it.
#[when(regex = r"^the simulated field jumps (\d+)″ (east|west)$")]
fn simulated_field_jumps(world: &mut RpWorld, arcsec: f64, direction: String) {
    let east = if direction == "east" { arcsec } else { -arcsec };
    service(world)
        .sim
        .slew(
            SkyOffset { east, north: 0.0 },
            Duration::ZERO,
            PierSide::East,
        )
        .expect("move the simulated field");
}

// --- Then steps -----------------------------------------------------------

#[then(expr = "the guider result's {string} should be at least {int}")]
fn guider_result_at_least(world: &mut RpWorld, field: String, minimum: u64) {
    let result = last_guider_result(world);
    let actual = result_number(result, &field);
    assert!(
        actual >= minimum as f64,
        "'{field}' is {actual}, below {minimum}, in {result}"
    );
}

/// Each RMS in arcseconds is its pixel figure at the guider's pixel scale.
#[then(
    expr = "the guider result's arcsecond figures should be its pixel figures at {float} arcseconds a pixel"
)]
fn guider_result_arcsec_at_scale(world: &mut RpWorld, scale: f64) {
    let result = last_guider_result(world);
    for axis in ["rms_ra", "rms_dec", "total_rms"] {
        let px = result_number(result, &format!("{axis}_px"));
        let arcsec = result_number(result, &format!("{axis}_arcsec"));
        assert!(
            (arcsec - px * scale).abs() < 1e-6,
            "{axis}: {arcsec}″ is not {px} px at {scale}″ a pixel, in {result}"
        );
    }
}

/// The calibration's legs, in the order the guider runs them.
#[then("the simulated mount should have received West pulses, then East, then North, then South")]
fn simulated_mount_calibrated(world: &mut RpWorld) {
    let mut runs: Vec<GuideDirection> = Vec::new();
    for pulse in service(world).sim.pulses() {
        if runs.last() != Some(&pulse.direction) {
            runs.push(pulse.direction);
        }
    }
    assert_eq!(
        runs.get(..4),
        Some(
            &[
                GuideDirection::West,
                GuideDirection::East,
                GuideDirection::North,
                GuideDirection::South
            ][..]
        ),
        "the directions of the pulses, run together: {runs:?}"
    );
}

#[then("the simulated mount should have received no pulse")]
fn simulated_mount_unpulsed(world: &mut RpWorld) {
    let pulses = service(world).sim.pulses();
    assert!(pulses.is_empty(), "the simulated mount received {pulses:?}");
}

#[then(expr = "the guider service's stats should report the state {string}")]
async fn guider_service_state(world: &mut RpWorld, state: String) {
    let url = format!("{}/api/v1/guiding/stats", service(world).url());
    wait_for_json(&url, &format!("the state {state:?}"), |body| {
        body["state"] == state.as_str()
    })
    .await;
}

#[then(expr = "the guider service's last stop should have the reason {string}")]
async fn guider_service_last_stop(world: &mut RpWorld, reason: String) {
    let url = format!("{}/api/v1/guiding/stats", service(world).url());
    wait_for_json(&url, &format!("a last stop of reason {reason:?}"), |body| {
        body["last_stop"]["reason"] == reason.as_str()
    })
    .await;
}

/// `AtPark` as another client of the simulated mount reads it.
#[then(expr = "the simulated mount should report parked within {int} seconds")]
async fn simulated_mount_reports_parked(world: &mut RpWorld, seconds: u64) {
    let url = format!(
        "{}/api/v1/telescope/0/atpark?ClientID=1&ClientTransactionID=1",
        service(world).sim.mount_url()
    );
    let start = Instant::now();
    let budget = Duration::from_secs(seconds);
    let mut last = String::from("no answer yet");
    while start.elapsed() < budget {
        match read_json(&url).await {
            Ok(body) if body["Value"] == true => return,
            Ok(body) => last = body.to_string(),
            Err(error) => last = error,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the simulated mount did not report parked within {budget:?}; last answer: {last}");
}
