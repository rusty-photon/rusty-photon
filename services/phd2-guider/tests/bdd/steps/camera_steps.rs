//! Step definitions for the camera facade: `camera_facade.feature`,
//! `camera_exclusivity.feature` and `camera_config.feature`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ascom_alpaca::api::camera::CameraState;
use ascom_alpaca::api::{Camera, TypedDevice};
use ascom_alpaca::{ASCOMError, Client};
use cucumber::{given, then, when};

use crate::world::GuiderWorld;

/// The `unique_id` the suite's facade carries unless a scenario mints one.
const TEST_UNIQUE_ID: &str = "phd2-guider-facade-test-001";

// ---------------------------------------------------------------------------
// Givens
// ---------------------------------------------------------------------------

#[given(regex = r"^a mock PHD2 (\d+\.\d+\.\d+) with an? (\d+) x (\d+) camera$")]
async fn mock_with_camera(world: &mut GuiderWorld, version: String, width: u32, height: u32) {
    start_camera_mock(world, &version, width, height, "completes", "at_connect").await;
}

#[given(regex = r"^a mock PHD2 (\d+\.\d+\.\d+) with an? (\d+) x (\d+) camera whose frames fail$")]
async fn mock_failing_frames(world: &mut GuiderWorld, version: String, width: u32, height: u32) {
    start_camera_mock(world, &version, width, height, "fails", "at_connect").await;
}

#[given(
    regex = r"^a mock PHD2 (\d+\.\d+\.\d+) with an? (\d+) x (\d+) camera whose frames are never reported$"
)]
async fn mock_unreported_frames(world: &mut GuiderWorld, version: String, width: u32, height: u32) {
    start_camera_mock(
        world,
        &version,
        width,
        height,
        "never_reports",
        "at_connect",
    )
    .await;
}

#[given(
    regex = r"^a mock PHD2 (\d+\.\d+\.\d+) with an? (\d+) x (\d+) camera that reports 0 x 0 until its first frame$"
)]
async fn mock_size_after_first_frame(
    world: &mut GuiderWorld,
    version: String,
    width: u32,
    height: u32,
) {
    start_camera_mock(
        world,
        &version,
        width,
        height,
        "completes",
        "after_first_frame",
    )
    .await;
}

#[given(
    regex = r"^a mock PHD2 (\d+\.\d+\.\d+) with an? (\d+) x (\d+) camera whose frames fail and that reports 0 x 0 until its first frame$"
)]
async fn mock_failing_frames_size_after_first_frame(
    world: &mut GuiderWorld,
    version: String,
    width: u32,
    height: u32,
) {
    start_camera_mock(world, &version, width, height, "fails", "after_first_frame").await;
}

async fn start_camera_mock(
    world: &mut GuiderWorld,
    version: &str,
    width: u32,
    height: u32,
    mode: &str,
    size_known: &str,
) {
    let size = format!("{width}x{height}");
    world
        .start_mock_env(
            "settle_ok",
            "stops",
            &[
                ("MOCK_PHD2_VERSION", version),
                ("MOCK_PHD2_FRAME_SIZE", size.as_str()),
                ("MOCK_PHD2_CAPTURE_MODE", mode),
                ("MOCK_PHD2_FRAME_SIZE_KNOWN", size_known),
            ],
        )
        .await;
}

#[given(regex = r"^a mock PHD2 (\d+\.\d+\.\d+) whose camera is disconnected$")]
async fn mock_with_disconnected_camera(world: &mut GuiderWorld, version: String) {
    world
        .start_mock_env(
            "settle_ok",
            "stops",
            &[
                ("MOCK_PHD2_VERSION", version.as_str()),
                ("MOCK_PHD2_CAMERA", "disconnected"),
            ],
        )
        .await;
}

/// The `camera` block the suite runs the facade with.
async fn camera_block(world: &mut GuiderWorld) -> serde_json::Value {
    let port = bdd_infra::reserved_port_once(&mut world.camera_port).await;
    let image_dir = world.image_dir();
    serde_json::json!({
        "enabled": true,
        "unique_id": TEST_UNIQUE_ID,
        "pixel_size_x_um": 2.9,
        "pixel_size_y_um": 2.9,
        "image_dir": image_dir,
        "server": { "port": port, "bind_address": "127.0.0.1" }
    })
}

async fn start_with_camera(world: &mut GuiderWorld, camera: serde_json::Value) {
    let phd2_port = world.mock.as_ref().expect("mock PHD2 not started").port;
    let mut config = GuiderWorld::service_config(phd2_port, "10s");
    config["camera"] = camera;
    world.start_service_with_config(config, true).await;
}

#[given("the guider service is running with the camera facade enabled")]
async fn service_with_camera(world: &mut GuiderWorld) {
    let camera = camera_block(world).await;
    start_with_camera(world, camera).await;
}

#[given(
    expr = "the guider service is running with the camera facade enabled and a capture grace of {string}"
)]
async fn service_with_camera_grace(world: &mut GuiderWorld, grace: String) {
    let mut camera = camera_block(world).await;
    camera["capture_grace"] = grace.into();
    start_with_camera(world, camera).await;
}

#[given("the guider service is running with the camera facade enabled and no unique_id")]
async fn service_with_camera_no_id(world: &mut GuiderWorld) {
    let mut camera = camera_block(world).await;
    camera
        .as_object_mut()
        .expect("camera block is an object")
        .remove("unique_id");
    start_with_camera(world, camera).await;
}

#[given("the facade camera is connected")]
async fn camera_connected(world: &mut GuiderWorld) {
    let camera = facade(world).await;
    camera
        .set_connected(true)
        .await
        .unwrap_or_else(|e| panic!("connecting the facade camera failed: {e}"));
}

#[given(expr = "image_dir holds a file named {string}")]
fn image_dir_holds(world: &mut GuiderWorld, name: String) {
    let path = world.image_dir().join(name);
    std::fs::write(&path, b"left behind").expect("stage file in image_dir");
}

/// Stage the restart marker a crashed facade leaves, its deadline at
/// `deadline`.
fn stage_marker(world: &mut GuiderWorld, deadline: SystemTime) {
    let dir = world.image_dir();
    let frame = dir.join("phd2-guider-00000000000000000000000000000001.fits");
    let marker = serde_json::json!({
        "path": frame,
        "deadline_unix_ms": u64::try_from(
            deadline.duration_since(UNIX_EPOCH).unwrap().as_millis()
        ).unwrap(),
    });
    std::fs::write(dir.join("phd2-guider-capture.json"), marker.to_string())
        .expect("stage capture marker");
    world.marker_frame = Some(frame);
}

#[given(expr = "image_dir holds a capture marker whose deadline is {int} s away")]
fn image_dir_holds_marker(world: &mut GuiderWorld, seconds: u64) {
    stage_marker(world, SystemTime::now() + Duration::from_secs(seconds));
}

#[given(expr = "image_dir holds a capture marker whose deadline passed {int} s ago")]
fn image_dir_holds_stale_marker(world: &mut GuiderWorld, seconds: u64) {
    stage_marker(world, SystemTime::now() - Duration::from_secs(seconds));
}

#[given(regex = r"^this service's config with the camera block (\{.*\}) staged for doctor$")]
fn doctor_config_with_camera(world: &mut GuiderWorld, block: String) {
    let camera: serde_json::Value = serde_json::from_str(&block).expect("camera block is JSON");
    let mut config =
        <GuiderWorld as bdd_infra::doctor_smoke::DoctorSmokeWorld>::valid_config(world);
    config["camera"] = camera;
    bdd_infra::doctor_smoke::stage_config(&mut world.doctor_smoke, &config);
}

// ---------------------------------------------------------------------------
// Whens
// ---------------------------------------------------------------------------

/// The facade's Camera 0, through an Alpaca client.
pub async fn facade(world: &mut GuiderWorld) -> Arc<dyn Camera> {
    if let Some(camera) = &world.camera {
        return Arc::clone(camera);
    }
    let port = world.camera_port.expect("the facade is not configured");
    // The facade's listener is bound before the service prints its
    // `bound_addr=`, so it answers by now.
    let client = Client::new_from_addr(([127, 0, 0, 1], port));
    let mut devices = client
        .get_devices()
        .await
        .expect("list the facade's devices");
    // The facade serves one device, its Camera 0. (Matched through the
    // `Option`: which device kinds `TypedDevice` has depends on the
    // features the workspace build unifies.)
    let Some(TypedDevice::Camera(camera)) = devices.next() else {
        panic!("the facade's first device is not a Camera");
    };
    world.camera = Some(Arc::clone(&camera));
    camera
}

pub fn record_error(world: &mut GuiderWorld, result: Result<(), ASCOMError>) {
    world.last_ascom_error = result.err().map(|e| (e.code.raw(), e.message.into_owned()));
}

#[when("the client connects the facade camera")]
async fn connect(world: &mut GuiderWorld) {
    let camera = facade(world).await;
    camera
        .set_connected(true)
        .await
        .unwrap_or_else(|e| panic!("connecting the facade camera failed: {e}"));
}

#[when("the client tries to connect the facade camera")]
async fn try_connect(world: &mut GuiderWorld) {
    let camera = facade(world).await;
    let result = camera.set_connected(true).await;
    record_error(world, result);
}

#[when(expr = "the client starts a {float} s light exposure")]
async fn start_light(world: &mut GuiderWorld, seconds: f64) {
    let camera = facade(world).await;
    camera
        .start_exposure(Duration::from_secs_f64(seconds), true)
        .await
        .unwrap_or_else(|e| panic!("StartExposure failed: {e}"));
}

#[when(regex = r"^the client tries to start a ([0-9.]+) s (light|dark) exposure$")]
async fn try_start(world: &mut GuiderWorld, seconds: f64, kind: String) {
    let camera = facade(world).await;
    let result = camera
        .start_exposure(Duration::from_secs_f64(seconds), kind == "light")
        .await;
    record_error(world, result);
}

#[when(
    expr = "the client sets the subframe to start at x {int}, y {int} and span {int} x {int} pixels"
)]
async fn set_subframe(world: &mut GuiderWorld, x: u32, y: u32, width: u32, height: u32) {
    let camera = facade(world).await;
    camera.set_start_x(x).await.unwrap();
    camera.set_start_y(y).await.unwrap();
    camera.set_num_x(width).await.unwrap();
    camera.set_num_y(height).await.unwrap();
}

#[when("the client aborts the facade exposure")]
async fn abort(world: &mut GuiderWorld) {
    let camera = facade(world).await;
    camera
        .abort_exposure()
        .await
        .unwrap_or_else(|e| panic!("AbortExposure failed: {e}"));
}

#[when(
    regex = r"^the client sends (a guiding start|a pause|a resume|a dither|a calibration clear|a star reselect)$"
)]
async fn send_guiding_operation(world: &mut GuiderWorld, operation: String) {
    let (path, body) = match operation.as_str() {
        "a guiding start" => ("/api/v1/guiding/start", serde_json::json!({})),
        "a pause" => ("/api/v1/guiding/pause", serde_json::json!({})),
        "a resume" => ("/api/v1/guiding/resume", serde_json::json!({})),
        "a dither" => ("/api/v1/dither", serde_json::json!({ "amount_px": 3.0 })),
        "a calibration clear" => ("/api/v1/calibration/clear", serde_json::json!({})),
        "a star reselect" => ("/api/v1/star/reselect", serde_json::json!({})),
        other => unreachable!("operation {other}"),
    };
    let url = format!("{}{path}", world.service_url());
    let response = GuiderWorld::http_client()
        .post(&url)
        .json(&body)
        .send()
        .await
        .expect("HTTP request failed");
    world.record_response(response).await;
}

#[when(expr = "the facade image becomes ready within {int} s")]
async fn when_image_ready(world: &mut GuiderWorld, seconds: u64) {
    image_becomes_ready(world, seconds).await;
}

#[when("PHD2 reports the capture marker's frame")]
async fn phd2_reports_marker_frame(world: &mut GuiderWorld) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let frame = world
        .marker_frame
        .clone()
        .expect("no capture marker staged");
    std::fs::write(&frame, b"PHD2's frame").expect("land the marker's frame");
    // A second client of the mock asks it to report the frame to every
    // client, the guider service included, as PHD2 does.
    let port = world.mock.as_ref().expect("mock PHD2 not started").port;
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to the mock PHD2");
    let (read, mut write) = stream.into_split();
    let request = serde_json::json!({
        "method": "mock_report_frame",
        "params": { "path": frame },
        "id": 1,
    });
    write
        .write_all(format!("{request}\r\n").as_bytes())
        .await
        .expect("send mock_report_frame");
    let mut lines = BufReader::new(read).lines();
    let mut answered = false;
    while let Some(line) = lines.next_line().await.expect("read the mock's reply") {
        if line.contains("\"id\":1") {
            answered = true;
            break;
        }
    }
    assert!(
        answered,
        "the mock PHD2 closed the connection without answering mock_report_frame"
    );
    // The service removes the marker and releases the slot together,
    // under the guiding lock, so a request sent once the marker is gone
    // finds the slot released.
    let marker = world.image_dir().join("phd2-guider-capture.json");
    let give_up = tokio::time::Instant::now() + Duration::from_secs(10);
    while marker.exists() {
        if tokio::time::Instant::now() >= give_up {
            let diagnosis = marker_diagnosis(world);
            panic!("the capture marker was not removed after PHD2 reported its frame; {diagnosis}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// What can be read, without changing anything, once a reported marker
/// frame was not released: the requests the mock logged, and whether the
/// frame's file is gone. The service deletes it when it handles the
/// report, before it removes the marker, so a file still there means the
/// report was never handled.
fn marker_diagnosis(world: &GuiderWorld) -> String {
    let logged: Vec<String> = world
        .logged_rpcs()
        .iter()
        .filter_map(|rpc| rpc["method"].as_str().map(str::to_string))
        .collect();
    let frame = world.marker_frame.as_ref().map_or_else(
        || "no frame staged".to_string(),
        |frame| {
            if frame.exists() {
                "the frame's file is still there, so the service never handled the report"
                    .to_string()
            } else {
                "the frame's file is gone, so the service handled the report".to_string()
            }
        },
    );
    format!("the mock logged {logged:?}; {frame}")
}

#[when("the client fetches the facade image as ImageBytes")]
async fn fetch_image_bytes(world: &mut GuiderWorld) {
    let port = world.camera_port.expect("the facade is not configured");
    let url = format!(
        "http://127.0.0.1:{port}/api/v1/camera/0/imagearray?ClientID=1&ClientTransactionID=1"
    );
    let response = GuiderWorld::http_client()
        .get(&url)
        .header("Accept", "application/imagebytes")
        .send()
        .await
        .expect("ImageArray request failed");
    assert_eq!(response.status().as_u16(), 200, "ImageArray status");
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        content_type, "application/imagebytes",
        "ImageArray content type"
    );
    let body = response.bytes().await.expect("read the ImageBytes body");
    world.image_bytes = Some(body.to_vec());
}

async fn config_action(world: &mut GuiderWorld, action: &str, parameters: String) {
    let camera = facade(world).await;
    let reply = camera
        .action(action.to_string(), parameters)
        .await
        .unwrap_or_else(|e| panic!("{action} failed: {e}"));
    world.config_action_reply =
        Some(serde_json::from_str(&reply).expect("config action reply is JSON"));
}

#[when("the client reads the facade's config schema")]
async fn read_schema(world: &mut GuiderWorld) {
    config_action(world, "config.schema", String::new()).await;
}

#[when(expr = "the client applies the current config with camera.capture_grace set to {string}")]
async fn apply_grace(world: &mut GuiderWorld, grace: String) {
    config_action(world, "config.get", String::new()).await;
    let mut config = world.config_action_reply.take().unwrap()["config"].clone();
    config["camera"]["capture_grace"] = grace.into();
    config_action(world, "config.apply", config.to_string()).await;
}

// ---------------------------------------------------------------------------
// Thens
// ---------------------------------------------------------------------------

#[then("the facade camera should be connected")]
async fn is_connected(world: &mut GuiderWorld) {
    assert!(facade(world).await.connected().await.unwrap());
}

#[then("the facade camera should not be connected")]
async fn is_not_connected(world: &mut GuiderWorld) {
    assert!(!facade(world).await.connected().await.unwrap());
}

#[then(expr = "the facade camera should report a sensor of {int} x {int} pixels named {string}")]
async fn sensor(world: &mut GuiderWorld, width: u32, height: u32, name: String) {
    let camera = facade(world).await;
    assert_eq!(camera.camera_x_size().await.unwrap(), width);
    assert_eq!(camera.camera_y_size().await.unwrap(), height);
    assert_eq!(camera.sensor_name().await.unwrap(), name);
}

#[then("the mock PHD2 should have received no request that changes its state")]
fn only_reads(world: &mut GuiderWorld) {
    let writes: Vec<String> = world
        .logged_rpcs()
        .iter()
        .filter_map(|rpc| rpc["method"].as_str().map(str::to_string))
        .filter(|method| !method.starts_with("get_"))
        .collect();
    assert!(
        writes.is_empty(),
        "PHD2 received state-changing requests: {writes:?}"
    );
}

fn assert_call_failed(world: &GuiderWorld, code: &str) -> String {
    let expected = u16::from_str_radix(code, 16).unwrap();
    let (got, message) = world
        .last_ascom_error
        .clone()
        .expect("the facade call succeeded");
    assert_eq!(got, expected, "ASCOM error {got:#X}: {message}");
    message
}

#[then(regex = r"^the facade call should fail with ASCOM error 0x([0-9A-Fa-f]+)$")]
fn call_failed(world: &mut GuiderWorld, code: String) {
    assert_call_failed(world, &code);
}

#[then(
    regex = r#"^the facade call should fail with ASCOM error 0x([0-9A-Fa-f]+) mentioning "(.*)"$"#
)]
fn call_failed_mentioning(world: &mut GuiderWorld, code: String, needle: String) {
    let message = assert_call_failed(world, &code);
    assert!(
        message.contains(&needle),
        "ASCOM error message does not mention {needle:?}: {message}"
    );
}

async fn image_becomes_ready(world: &mut GuiderWorld, seconds: u64) {
    let camera = facade(world).await;
    let give_up = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        if camera.image_ready().await.unwrap() {
            return;
        }
        let state = camera.camera_state().await.unwrap();
        assert!(
            state != CameraState::Error,
            "the exposure failed: {:?}",
            camera.image_array().await.err()
        );
        assert!(
            tokio::time::Instant::now() < give_up,
            "the facade image was not ready within {seconds} s (state {state:?})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[then(expr = "the facade image should become ready within {int} s")]
async fn then_image_ready(world: &mut GuiderWorld, seconds: u64) {
    image_becomes_ready(world, seconds).await;
}

#[then(expr = "the facade image should be {int} x {int} pixels")]
async fn image_size(world: &mut GuiderWorld, width: usize, height: usize) {
    let image = facade(world).await.image_array().await.unwrap();
    let shape = image.shape();
    assert_eq!(
        (shape[0], shape[1]),
        (width, height),
        "ImageArray shape {shape:?}"
    );
}

#[then(expr = "the facade image pixel at x {int}, y {int} should be {int}")]
async fn image_pixel(world: &mut GuiderWorld, x: usize, y: usize, value: i32) {
    let image = facade(world).await.image_array().await.unwrap();
    assert_eq!(image[[x, y, 0]], value);
}

/// The little-endian 32-bit integer at `offset` of the `ImageBytes` body.
fn image_bytes_i32(world: &GuiderWorld, offset: usize) -> i32 {
    let body = world.image_bytes.as_ref().expect("no ImageBytes fetched");
    let bytes: [u8; 4] = body
        .get(offset..offset + 4)
        .unwrap_or_else(|| {
            panic!(
                "ImageBytes body of {} bytes ends before offset {offset}",
                body.len()
            )
        })
        .try_into()
        .expect("four bytes");
    i32::from_le_bytes(bytes)
}

#[then("the ImageBytes header should match these constants (i32 little-endian):")]
fn image_bytes_header(world: &mut GuiderWorld, step: &cucumber::gherkin::Step) {
    let table = step.table.as_ref().expect("a table of header constants");
    for row in table.rows.iter().skip(1) {
        let [field, offset, value] = row.as_slice() else {
            panic!("a header row is field, offset, value: {row:?}");
        };
        let offset: usize = offset.parse().expect("offset is a number");
        let value: i32 = value.parse().expect("value is a number");
        assert_eq!(
            image_bytes_i32(world, offset),
            value,
            "{field} at offset {offset}"
        );
    }
}

#[then(expr = "the ImageBytes pixel data should be {int} bytes")]
fn image_bytes_data_len(world: &mut GuiderWorld, len: usize) {
    let data_start = usize::try_from(image_bytes_i32(world, 16)).expect("data_start");
    let body = world.image_bytes.as_ref().expect("no ImageBytes fetched");
    let data = body.len().checked_sub(data_start).unwrap_or_else(|| {
        panic!(
            "the ImageBytes body is {} bytes, shorter than its data_start {data_start}",
            body.len()
        )
    });
    assert_eq!(data, len, "ImageBytes pixel data length");
}

#[then(expr = "the facade camera's last exposure should have lasted {float} s")]
async fn last_duration(world: &mut GuiderWorld, seconds: f64) {
    let lasted = facade(world).await.last_exposure_duration().await.unwrap();
    assert_eq!(lasted, Duration::from_secs_f64(seconds));
}

#[then(
    regex = r"^the mock PHD2 should have received a capture of (\d+) ms saved under image_dir as a phd2-guider FITS file$"
)]
fn received_capture(world: &mut GuiderWorld, exposure_ms: u64) {
    let image_dir = world.image_dir();
    let captures = world.logged_rpcs_named("capture_single_frame");
    let [capture] = captures.as_slice() else {
        panic!("expected one capture_single_frame, got {captures:?}");
    };
    let params = &capture["params"];
    assert_eq!(params["exposure"], exposure_ms, "{params}");
    assert_eq!(params["save"], true, "{params}");
    let path = std::path::PathBuf::from(params["path"].as_str().expect("path is a string"));
    assert_eq!(path.parent(), Some(image_dir.as_path()), "{params}");
    let name = path.file_name().unwrap().to_str().unwrap();
    let hex = name
        .strip_prefix("phd2-guider-")
        .and_then(|rest| rest.strip_suffix(".fits"))
        .unwrap_or_else(|| panic!("not a phd2-guider FITS name: {name}"));
    assert!(
        hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        "not 32 hex digits: {name}"
    );
}

#[then("image_dir should hold no phd2-guider file")]
fn no_frame_files(world: &mut GuiderWorld) {
    let left: Vec<String> = std::fs::read_dir(world.image_dir())
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| {
            name.starts_with("phd2-guider-")
                && std::path::Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("fits"))
        })
        .collect();
    assert!(left.is_empty(), "frame files left in image_dir: {left:?}");
}

#[then(expr = "image_dir should still hold {string}")]
fn still_holds(world: &mut GuiderWorld, name: String) {
    assert!(world.image_dir().join(&name).exists(), "{name} was removed");
}

#[then("image_dir should hold no capture marker")]
fn no_marker(world: &mut GuiderWorld) {
    assert!(!world.image_dir().join("phd2-guider-capture.json").exists());
}

#[then(expr = "the facade camera should report a failed exposure within {int} s")]
async fn failed_within(world: &mut GuiderWorld, seconds: u64) {
    let camera = facade(world).await;
    let give_up = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let state = camera.camera_state().await.unwrap();
        if state == CameraState::Error {
            return;
        }
        assert!(
            tokio::time::Instant::now() < give_up,
            "no failed exposure within {seconds} s (state {state:?})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[then(
    regex = r#"^reading the facade image should fail with ASCOM error 0x([0-9A-Fa-f]+) mentioning "(.*)"$"#
)]
async fn image_fails(world: &mut GuiderWorld, code: String, needle: String) {
    let expected = u16::from_str_radix(&code, 16).unwrap();
    let err = facade(world)
        .await
        .image_array()
        .await
        .expect_err("ImageArray returned an image");
    assert_eq!(err.code.raw(), expected, "{err}");
    assert!(err.message.contains(&needle), "{err}");
}

#[then(expr = "the facade camera should be idle within {int} s")]
async fn idle_within(world: &mut GuiderWorld, seconds: u64) {
    let camera = facade(world).await;
    let give_up = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let state = camera.camera_state().await.unwrap();
        if state == CameraState::Idle {
            return;
        }
        assert!(
            tokio::time::Instant::now() < give_up,
            "not idle within {seconds} s (state {state:?})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[then("the facade image should not be ready")]
async fn not_ready(world: &mut GuiderWorld) {
    assert!(!facade(world).await.image_ready().await.unwrap());
}

fn config_file(world: &GuiderWorld) -> serde_json::Value {
    let path = world.config_path.as_ref().expect("service not started");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[then("the config file should hold a camera unique_id")]
fn file_holds_id(world: &mut GuiderWorld) {
    let config = config_file(world);
    let id = config["camera"]["unique_id"].as_str().unwrap_or_default();
    assert!(!id.is_empty(), "no camera.unique_id in {config}");
}

#[then("the facade camera's UniqueID should be the config file's camera unique_id")]
async fn unique_id_matches(world: &mut GuiderWorld) {
    let config = config_file(world);
    let camera = facade(world).await;
    assert_eq!(
        camera.unique_id(),
        config["camera"]["unique_id"].as_str().unwrap()
    );
}

fn reply_list(world: &GuiderWorld, field: &str) -> Vec<String> {
    world
        .config_action_reply
        .as_ref()
        .expect("no config action reply")[field]
        .as_array()
        .unwrap_or_else(|| panic!("no {field} in the reply"))
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

#[then(expr = "the read-only fields should include {string}")]
fn read_only_includes(world: &mut GuiderWorld, path: String) {
    let fields = reply_list(world, "read_only_fields");
    assert!(fields.contains(&path), "{path} not in {fields:?}");
}

#[then(expr = "the locked fields should include {string}")]
fn locked_includes(world: &mut GuiderWorld, path: String) {
    let fields = reply_list(world, "locked_fields");
    assert!(fields.contains(&path), "{path} not in {fields:?}");
}

#[then(expr = "the apply status should be {string}")]
fn apply_status(world: &mut GuiderWorld, status: String) {
    let reply = world.config_action_reply.as_ref().expect("no apply reply");
    assert_eq!(reply["status"], status, "{reply}");
}

#[then(expr = "the apply should list {string} as needing a restart")]
fn apply_restart(world: &mut GuiderWorld, path: String) {
    let fields = reply_list(world, "restart_required");
    assert!(fields.contains(&path), "{path} not in {fields:?}");
}

#[then(expr = "the config file should hold camera.capture_grace {string}")]
fn file_holds_grace(world: &mut GuiderWorld, grace: String) {
    let config = config_file(world);
    assert_eq!(config["camera"]["capture_grace"], grace, "{config}");
}

#[then(expr = "the doctor report should fail mentioning {string}")]
fn doctor_fails_mentioning(world: &mut GuiderWorld, needle: String) {
    let output = world
        .doctor_smoke
        .output
        .as_ref()
        .expect("doctor was not run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "doctor exit code\n{stdout}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("JSON report");
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "config.full-shape")
        .expect("config.full-shape check");
    assert_eq!(check["status"], "fail", "{report}");
    let detail = check["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains(&needle),
        "detail does not mention {needle:?}: {detail}"
    );
}
