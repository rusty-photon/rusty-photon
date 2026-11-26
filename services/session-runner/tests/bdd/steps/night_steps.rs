//! Step definitions for `night.feature`: the night schedule's place, read
//! on `GET /night` and in `night.json`, against rp's civil twilight.

use std::time::Duration;

use bdd_infra::rp_harness::McpTestClient;
use chrono::{DateTime, TimeDelta, Utc};
use cucumber::{given, then, when};
use serde_json::{json, Value};

use crate::steps::infrastructure::{
    start_rp_service, start_session_runner_service, start_session_runner_service_with,
    try_start_session_runner_with,
};
use crate::world::SessionRunnerWorld;

/// How long the schedule gets to read its first night from rp.
const READ_BUDGET: Duration = Duration::from_secs(20);

#[given("session-runner is running without a night block")]
async fn running_without_night(world: &mut SessionRunnerWorld) {
    start_session_runner_service(world).await;
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
        if !report["night"].is_null() {
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
