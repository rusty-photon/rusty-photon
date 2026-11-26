//! Step definitions for `night.feature`: the night schedule's place, read
//! on `GET /night` and in `night.json`, against rp's civil twilight, and
//! its dusk and dawn against the simulator and a stub power box.

use std::time::Duration;

use bdd_infra::rp_harness::{
    ComputedSky, McpTestClient, OmniSimHandle, PowerBoxStub, SwitchConfig,
};
use chrono::{DateTime, TimeDelta, Utc};
use cucumber::{given, then, when};
use serde_json::{json, Value};

use crate::steps::infrastructure::{
    ensure_camera, ensure_mount, ensure_omnisim, start_rp_service, start_session_runner_service,
    start_session_runner_service_with, try_start_session_runner_with,
};
use crate::world::SessionRunnerWorld;

/// How long the schedule gets to read its first night from rp.
const READ_BUDGET: Duration = Duration::from_secs(20);

#[given("session-runner is running without a night block")]
async fn running_without_night(world: &mut SessionRunnerWorld) {
    start_session_runner_service(world).await;
}

#[given("rp is running with no devices at a site whose dawn has passed")]
async fn rp_bare_at_morning_site(world: &mut SessionRunnerWorld) {
    let sky = ComputedSky::morning_at(Utc::now());
    world.site = Some((sky.latitude_degrees(), sky.longitude_degrees()));
    start_rp_service(world).await;
}

#[given(expr = "rp is running with a site at latitude {float} and longitude {float}")]
async fn rp_with_site(world: &mut SessionRunnerWorld, latitude: f64, longitude: f64) {
    world.site = Some((latitude, longitude));
    start_rp_service(world).await;
}

#[given(expr = "session-runner is running with a night block whose dusk_offset is {string}")]
async fn running_with_night(world: &mut SessionRunnerWorld, dusk_offset: String) {
    start_session_runner_service_with(world, json!({ "night": { "dusk_offset": dusk_offset } }))
        .await;
}

#[when(expr = "session-runner is started with a night block whose run names the workflow {string}")]
async fn started_with_unknown_run(world: &mut SessionRunnerWorld, workflow: String) {
    let night = json!({ "night": { "runs": [ { "workflow": workflow } ] } });
    world.start_error = try_start_session_runner_with(world, night).await.err();
}

#[when("the night schedule's place is read")]
async fn read_place(world: &mut SessionRunnerWorld) {
    world.night_report = Some(get_night(world).await);
}

#[when("the night schedule has read the night ahead")]
async fn read_night_ahead(world: &mut SessionRunnerWorld) {
    let deadline = tokio::time::Instant::now() + READ_BUDGET;
    loop {
        let report = get_night(world).await;
        // A start in the day runs the dawn that has passed first, on the
        // night it ends.
        let dawn_ahead = report["dawn"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .is_some_and(|t| t.with_timezone(&Utc) > Utc::now());
        if dawn_ahead {
            world.night_report = Some(report);
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the schedule did not read a night within {READ_BUDGET:?}: {report}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[then(regex = r"^the night schedule should report the JSON (.+)$")]
fn report_is(world: &mut SessionRunnerWorld, expected: String) {
    let expected: Value = serde_json::from_str(&expected).expect("the step's JSON parses");
    assert_eq!(report(world), &expected);
}

#[then(expr = "the night schedule's phase should be {string}")]
fn phase_is(world: &mut SessionRunnerWorld, phase: String) {
    assert_eq!(report(world)["phase"], json!(phase), "{}", report(world));
}

#[then(
    expr = "the night schedule's dusk should be that night's civil twilight begin moved by {int} minutes"
)]
async fn dusk_is_begin_moved(world: &mut SessionRunnerWorld, minutes: i64) {
    let (begin, _) = civil_twilight(world).await;
    assert_eq!(
        reported_time(world, "dusk"),
        begin + TimeDelta::minutes(minutes)
    );
}

#[then("the night schedule's dawn should be that night's civil twilight end")]
async fn dawn_is_end(world: &mut SessionRunnerWorld) {
    let (_, end) = civil_twilight(world).await;
    assert_eq!(reported_time(world, "dawn"), end);
}

#[then("the night schedule's dawn should be in the future")]
fn dawn_ahead(world: &mut SessionRunnerWorld) {
    let dawn = reported_time(world, "dawn");
    assert!(dawn > Utc::now(), "dawn {dawn} has passed");
}

#[then("night.json should name the night, dusk and dawn the schedule reports")]
fn file_matches_report(world: &mut SessionRunnerWorld) {
    let path = world
        .state_dir
        .as_ref()
        .expect("session-runner was started with a state_dir")
        .path()
        .join("night.json");
    let file: Value = serde_json::from_slice(
        &std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display())),
    )
    .expect("night.json parses");
    let report = report(world);
    for field in ["night", "dusk", "dawn", "phase"] {
        assert_eq!(
            file[field], report[field],
            "{field}: file {file}, report {report}"
        );
    }
}

#[then("session-runner should fail to start")]
fn failed_to_start(world: &mut SessionRunnerWorld) {
    assert!(
        world.session_runner.is_none(),
        "session-runner started; it should have refused its configuration"
    );
    let error = world
        .start_error
        .as_deref()
        .expect("the start failed with an error");
    assert!(error.contains("exited without binding"), "{error}");
}

async fn get_night(world: &SessionRunnerWorld) -> Value {
    let url = format!("{}/night", world.session_runner_url());
    let response = reqwest::get(&url).await.expect("GET /night failed");
    assert_eq!(response.status().as_u16(), 200);
    response.json().await.expect("GET /night answers JSON")
}

const fn report(world: &SessionRunnerWorld) -> &Value {
    world
        .night_report
        .as_ref()
        .expect("the night schedule's place was read")
}

fn reported_time(world: &SessionRunnerWorld, field: &str) -> DateTime<Utc> {
    let text = report(world)[field]
        .as_str()
        .unwrap_or_else(|| panic!("{field} is not a time: {}", report(world)));
    DateTime::parse_from_rfc3339(text)
        .unwrap_or_else(|e| panic!("{field} `{text}` does not parse: {e}"))
        .with_timezone(&Utc)
}

/// The reported night's civil twilight, as rp answers it.
async fn civil_twilight(world: &SessionRunnerWorld) -> (DateTime<Utc>, DateTime<Utc>) {
    let night = report(world)["night"].clone();
    let mcp = McpTestClient::connect(&format!("{}/mcp", world.rp_url()))
        .await
        .expect("connect to rp's MCP server");
    let answer = mcp
        .call_tool("get_twilight", json!({ "date": night, "kind": "civil" }))
        .await
        .expect("get_twilight answers");
    let end = |key: &str| {
        DateTime::parse_from_rfc3339(answer[key].as_str().expect("a civil end at this site"))
            .expect("a civil end parses")
            .with_timezone(&Utc)
    };
    (end("begin_utc"), end("end_utc"))
}

/// How long dawn gets to run its steps against the simulator.
const DAWN_BUDGET: Duration = Duration::from_secs(90);

/// How long dusk gets to power the simulator's camera up, check it and
/// start the night's run.
const DUSK_BUDGET: Duration = Duration::from_secs(120);

#[given(expr = "a stub power box whose daily output {string} feeds the simulator's camera")]
async fn power_box_feeding_camera(world: &mut SessionRunnerWorld, output: String) {
    ensure_omnisim(world).await;
    ensure_camera(world);
    ensure_mount(world);
    let stub = PowerBoxStub::start(&[output.as_str()]);
    world.switches.push(SwitchConfig {
        id: "power-box".to_owned(),
        alpaca_url: stub.url(),
        device_number: 0,
        outputs: vec![json!({ "name": output, "devices": ["main-cam"], "daily": true })],
    });
    world.power_box = Some(stub);
}

#[given(
    expr = "a stub power box whose daily output {string} feeds the simulator's camera and reads off"
)]
async fn power_box_feeding_camera_off(world: &mut SessionRunnerWorld, output: String) {
    power_box_feeding_camera(world, output.clone()).await;
    world
        .power_box
        .as_ref()
        .expect("a stub power box runs")
        .switch(&output, false);
}

#[given("rp is running at an observing site whose dawn has passed")]
async fn rp_at_morning_site(world: &mut SessionRunnerWorld) {
    rp_at_observing_site(world, &ComputedSky::morning_at(Utc::now())).await;
}

#[given("rp is running at an observing site whose dusk has passed")]
async fn rp_at_night_site(world: &mut SessionRunnerWorld) {
    rp_at_observing_site(world, &ComputedSky::night_at(Utc::now())).await;
}

/// rp at `sky`'s site, with the simulated mount moved there.
async fn rp_at_observing_site(world: &mut SessionRunnerWorld, sky: &ComputedSky) {
    let (lat, lon) = (sky.latitude_degrees(), sky.longitude_degrees());
    world.site = Some((lat, lon));
    // rp refuses a mount whose site differs from its own. The site is a
    // profile setting the per-scenario restart does not reset, so the
    // after-hook puts the old one back.
    ensure_omnisim(world).await;
    world.original_telescope_site = Some(
        OmniSimHandle::get_telescope_site()
            .await
            .expect("failed to read the simulated mount's site"),
    );
    OmniSimHandle::set_telescope_site(lat, lon)
        .await
        .expect("failed to set the simulated mount's site");
    start_rp_service(world).await;
}

#[given(expr = "a run of {string} left in the state directory by the previous process")]
async fn run_left_by_previous_process(world: &mut SessionRunnerWorld, workflow: String) {
    let state_dir = world
        .state_dir
        .get_or_insert_with(|| tempfile::tempdir().expect("cannot create a state_dir"));
    let run_id = "run-left-by-the-previous-process".to_owned();
    let session_id = "session-left";
    let manifest = json!({
        "run_id": run_id,
        "session_id": session_id,
        "workflow": workflow,
        "params": {},
        "started_at": Utc::now(),
    });
    std::fs::write(
        state_dir.path().join(format!("{session_id}.run.json")),
        serde_json::to_vec(&manifest).expect("the manifest serializes"),
    )
    .expect("cannot write the run manifest");
    world.left_run_id = Some(run_id);
}

#[when("session-runner starts with a night block")]
async fn starts_with_night(world: &mut SessionRunnerWorld) {
    start_session_runner_service_with(world, json!({ "night": {} })).await;
}

#[when(expr = "session-runner starts with a night block that runs {string}")]
async fn starts_with_night_run(world: &mut SessionRunnerWorld, workflow: String) {
    let night = json!({ "night": { "settle": "1s", "runs": [ { "workflow": workflow } ] } });
    start_session_runner_service_with(world, night).await;
}

#[then(expr = "the stub power box's {string} output should go on")]
async fn output_goes_on(world: &mut SessionRunnerWorld, output: String) {
    let stub = world.power_box.as_ref().expect("a stub power box runs");
    let deadline = tokio::time::Instant::now() + DUSK_BUDGET;
    while !stub.is_on(&output) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{output} is still off after {DUSK_BUDGET:?}; the night reads {}",
            get_night(world).await
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[then(
    expr = "the night's run of {string} should complete under the session id of its night and index {int}"
)]
async fn night_run_completes(world: &mut SessionRunnerWorld, workflow: String, index: usize) {
    let deadline = tokio::time::Instant::now() + DUSK_BUDGET;
    loop {
        let night = get_night(world).await;
        let session_id = format!("night-{}-{index}", night["night"].as_str().unwrap_or(""));
        let runs: Value = reqwest::get(format!("{}/runs", world.session_runner_url()))
            .await
            .expect("GET /runs failed")
            .json()
            .await
            .expect("GET /runs answers JSON");
        let run = runs
            .as_array()
            .expect("GET /runs answers a list")
            .iter()
            .find(|r| r["session_id"] == json!(session_id))
            .cloned();
        if let Some(run) = &run {
            assert_eq!(run["workflow"], json!(workflow), "{run}");
            if run["state"] == json!("complete") {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no run of {workflow} completed under {session_id} within {DUSK_BUDGET:?}; \
             the night reads {night}; the runs read {runs}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[then(expr = "the night schedule should reach phase {string}")]
async fn reaches_phase(world: &mut SessionRunnerWorld, phase: String) {
    let deadline = tokio::time::Instant::now() + DUSK_BUDGET;
    loop {
        let report = get_night(world).await;
        if report["phase"] == json!(phase) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the night did not reach phase {phase}: {report}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[then(expr = "the stub power box's {string} output should go off")]
async fn output_goes_off(world: &mut SessionRunnerWorld, output: String) {
    let stub = world.power_box.as_ref().expect("a stub power box runs");
    let deadline = tokio::time::Instant::now() + DAWN_BUDGET;
    while stub.is_on(&output) {
        if tokio::time::Instant::now() >= deadline {
            let mcp = McpTestClient::connect(&format!("{}/mcp", world.rp_url()))
                .await
                .expect("connect to rp's MCP server");
            let info = mcp
                .call_tool("get_camera_info", json!({ "camera_id": "main-cam" }))
                .await;
            let map = mcp.call_tool("get_power_map", json!({})).await;
            panic!(
                "{output} is still on after {DAWN_BUDGET:?}; the night reads {}; \
                 main-cam reads {info:?}; the power map reads {map:?}",
                get_night(world).await
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[then("rp should report the mount parked")]
async fn mount_parked(world: &mut SessionRunnerWorld) {
    let mcp = McpTestClient::connect(&format!("{}/mcp", world.rp_url()))
        .await
        .expect("connect to rp's MCP server");
    let park = mcp
        .call_tool("get_park_state", json!({}))
        .await
        .expect("get_park_state answers");
    assert_eq!(park["at_park"], json!(true), "{park}");
}

#[then(expr = "the night schedule should be on the night ahead in phase {string}")]
async fn on_night_ahead(world: &mut SessionRunnerWorld, phase: String) {
    let deadline = tokio::time::Instant::now() + DAWN_BUDGET;
    loop {
        let report = get_night(world).await;
        let dusk_ahead = report["dusk"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .is_some_and(|t| t.with_timezone(&Utc) > Utc::now());
        if report["phase"] == json!(phase) && dusk_ahead {
            assert_eq!(report["outputs_left_on"], json!([]), "{report}");
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the night did not move to the night ahead: {report}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[then(expr = "the run the previous process left should end {string}")]
async fn left_run_ends(world: &mut SessionRunnerWorld, state: String) {
    let run_id = world.left_run_id.clone().expect("a run was left");
    let url = format!("{}/runs/{run_id}", world.session_runner_url());
    let deadline = tokio::time::Instant::now() + DAWN_BUDGET;
    loop {
        let record: Value = reqwest::get(&url)
            .await
            .expect("GET /runs/{id} failed")
            .json()
            .await
            .expect("GET /runs/{id} answers JSON");
        if record["state"] == json!(state) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the run did not end {state}: {record}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
