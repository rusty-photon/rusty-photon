//! BDD step definitions for the camera-cooling feature
//! (`camera_cooling.feature`). Session/orchestrator/webhook steps are
//! shared with `session_steps.rs` and `event_steps.rs`; this file adds the
//! ladder-configured camera Givens, the numeric event/document
//! assertions, and the direct simulator cooler-state checks.

use cucumber::{given, then, when};

use bdd_infra::rp_harness::{CameraConfig, CoolingOverrides};

use crate::world::RpWorld;

// --- Given steps ---

#[given("cooling is tuned for test speed")]
const fn cooling_tuned_for_test_speed(world: &mut RpWorld) {
    world.cooling_overrides = Some(CoolingOverrides::fast());
}

#[given(
    expr = "rp is running with a camera with cooler targets {string} on the simulator and the test orchestrator"
)]
async fn rp_running_with_cooled_camera(world: &mut RpWorld, targets: String) {
    crate::steps::tool_steps::ensure_omnisim(world).await;
    let ladder: Vec<i32> = targets
        .split(',')
        .map(|s| {
            s.trim()
                .parse()
                .unwrap_or_else(|e| panic!("bad cooler target {s:?}: {e}"))
        })
        .collect();
    add_camera_with_targets(world, ladder);
    crate::steps::tool_steps::start_rp(world).await;
}

#[given(
    "rp is running with a camera with no cooler targets on the simulator and the test orchestrator"
)]
async fn rp_running_with_uncooled_camera(world: &mut RpWorld) {
    crate::steps::tool_steps::ensure_omnisim(world).await;
    add_camera_with_targets(world, Vec::new());
    crate::steps::tool_steps::start_rp(world).await;
}

fn add_camera_with_targets(world: &mut RpWorld, ladder: Vec<i32>) {
    let url = world.omnisim_url();
    world.cameras.push(CameraConfig {
        id: "main-cam".to_string(),
        alpaca_url: url,
        device_number: 0,
        cooler_targets_c: ladder,
    });
}

// --- When steps ---

/// Wait until the cooldown task has actually switched the simulator
/// camera's cooler on. The warm-up scenario stops the session right
/// after starting it; without this barrier the stop can race the
/// cooldown's first device command and the scenario would assert on an
/// arbitrary interleaving instead of the ramp contract.
#[when("the camera cooler becomes on")]
async fn camera_cooler_becomes_on(world: &mut RpWorld) {
    for _ in 0..40 {
        if omnisim_cooler_on(world).await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("the simulator camera cooler did not turn on within timeout");
}

// --- Then steps ---

#[then(expr = "the {string} event payload field {string} should be the number {float}")]
async fn event_payload_field_number(
    world: &mut RpWorld,
    event_type: String,
    field: String,
    expected: f64,
) {
    let events = world.received_events.read().await;
    let event = events
        .iter()
        .find(|e| e.event_type == event_type)
        .unwrap_or_else(|| panic!("no '{event_type}' event received"));
    let actual = event
        .payload
        .get(&field)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| {
            panic!(
                "'{event_type}' payload field '{field}' missing or not a number: {}",
                event.payload
            )
        });
    assert!(
        (actual - expected).abs() < 1e-6,
        "'{event_type}' payload field '{field}' is {actual}, expected {expected}"
    );
}

#[then("the camera cooler should be on")]
async fn camera_cooler_should_be_on(world: &mut RpWorld) {
    assert!(
        omnisim_cooler_on(world).await,
        "expected the simulator camera cooler to be on"
    );
}

#[then("the camera cooler should be off")]
async fn camera_cooler_should_be_off(world: &mut RpWorld) {
    assert!(
        !omnisim_cooler_on(world).await,
        "expected the simulator camera cooler to be off"
    );
}

#[then(expr = "the document field {string} should be the number {float}")]
fn document_field_number(world: &mut RpWorld, field: String, expected: f64) {
    let doc = world
        .last_document_response_body
        .as_ref()
        .expect("no document fetched — add an 'I fetch the document ...' step first");
    let actual = doc
        .get(&field)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("document field '{field}' missing or not a number: {doc}"));
    assert!(
        (actual - expected).abs() < 1e-6,
        "document field '{field}' is {actual}, expected {expected}"
    );
}

#[then(expr = "the document should carry a numeric {string}")]
fn document_has_numeric_field(world: &mut RpWorld, field: String) {
    let doc = world
        .last_document_response_body
        .as_ref()
        .expect("no document fetched — add an 'I fetch the document ...' step first");
    assert!(
        doc.get(&field)
            .and_then(serde_json::Value::as_f64)
            .is_some(),
        "document field '{field}' missing or not a number: {doc}"
    );
}

// --- Helpers ---

/// Read `CoolerOn` straight from the simulator camera — the
/// device-level ground truth the scenarios assert against (connecting
/// first: `OmniSim` rejects cooler reads on a disconnected device, and
/// rp's own connection does not make the state readable for other
/// clients).
async fn omnisim_cooler_on(world: &RpWorld) -> bool {
    let base = world.omnisim_url();
    let client = reqwest::Client::new();
    client
        .put(format!("{base}/api/v1/camera/0/connected"))
        .form(&[("Connected", "true"), ("ClientID", "9090")])
        .send()
        .await
        .expect("failed to connect to the simulator camera");
    let body: serde_json::Value = client
        .get(format!("{base}/api/v1/camera/0/cooleron?ClientID=9090"))
        .send()
        .await
        .expect("failed to read CoolerOn from the simulator")
        .json()
        .await
        .expect("CoolerOn response is not JSON");
    assert_eq!(
        body.get("ErrorNumber").and_then(serde_json::Value::as_i64),
        Some(0),
        "CoolerOn read failed: {body}"
    );
    body.get("Value")
        .and_then(serde_json::Value::as_bool)
        .expect("CoolerOn response carries no boolean Value")
}
