//! Step definitions for `tests/features/phd2_restart.feature`.

use cucumber::{given, then, when};

use crate::world::{GuiderWorld, HttpResponse};

// ---------------------------------------------------------------------------
// Givens
// ---------------------------------------------------------------------------

#[given("the rusty-photon-phd2 unit keeps PHD2 running")]
async fn unit_keeps_phd2(world: &mut GuiderWorld) {
    world.stand_in_for_the_unit(true).await;
}

#[given("the rusty-photon-phd2 unit is inactive")]
async fn unit_inactive(world: &mut GuiderWorld) {
    world.stand_in_for_the_unit(false).await;
}

#[given("a mock PHD2 that is guiding")]
async fn mock_guiding(world: &mut GuiderWorld) {
    world
        .start_mock_env("settle_ok", "stops", &[("MOCK_PHD2_APP_STATE", "Guiding")])
        .await;
}

#[given("a mock PHD2 that ignores shutdown")]
async fn mock_ignoring_shutdown(world: &mut GuiderWorld) {
    world
        .start_mock_env(
            "settle_ok",
            "stops",
            &[("MOCK_PHD2_MODE", "shutdown_fails")],
        )
        .await;
}

#[given("a mock PHD2 that accepts connections and never answers")]
async fn mock_never_answering(world: &mut GuiderWorld) {
    world
        .start_mock_env("settle_ok", "stops", &[("MOCK_PHD2_MODE", "no_answer")])
        .await;
}

#[given("the guider service is running switched over to its managed PHD2")]
async fn service_managed(world: &mut GuiderWorld) {
    let port = world.mock.as_ref().expect("mock PHD2 not started").port;
    world
        .start_service_with_config(GuiderWorld::managed_config(port), true)
        .await;
}

#[given(
    "the guider service is running switched over to its managed PHD2 with the camera facade enabled"
)]
async fn service_managed_with_camera(world: &mut GuiderWorld) {
    let port = world.mock.as_ref().expect("mock PHD2 not started").port;
    let mut config = GuiderWorld::managed_config(port);
    config["camera"] = crate::steps::camera_steps::camera_block(world).await;
    world.start_service_with_config(config, true).await;
}

#[given(expr = "the guider service is running switched over to a PHD2 at {string}")]
async fn service_managed_remote(world: &mut GuiderWorld, host: String) {
    let mut config = GuiderWorld::managed_config(4400);
    config["phd2"]["host"] = host.into();
    world.start_service_with_config(config, false).await;
}

// ---------------------------------------------------------------------------
// Whens
// ---------------------------------------------------------------------------

#[when("the client restarts PHD2")]
async fn restart_phd2(world: &mut GuiderWorld) {
    let url = format!("{}/api/v1/phd2/restart", world.service_url());
    let response = GuiderWorld::http_client()
        .post(&url)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("HTTP request failed");
    world.record_response(response).await;
}

#[when("the client posts a form to the PHD2 restart route")]
async fn restart_phd2_as_a_form(world: &mut GuiderWorld) {
    let url = format!("{}/api/v1/phd2/restart", world.service_url());
    let response = GuiderWorld::http_client()
        .post(&url)
        .form(&[("restart", "true")])
        .send()
        .await
        .expect("HTTP request failed");
    // The refusal is axum's own, in plain text.
    let status = response.status().as_u16();
    let text = response.text().await.expect("read response body");
    world.last_response = Some(HttpResponse {
        status,
        body: serde_json::Value::String(text),
    });
}

// ---------------------------------------------------------------------------
// Thens
// ---------------------------------------------------------------------------

#[then(expr = "the response field {string} should be a whole number")]
async fn response_field_whole(world: &mut GuiderWorld, field: String) {
    let body = &world.last_response().body;
    assert!(
        body[&field].as_u64().is_some(),
        "field {field} is not a whole number in {body}"
    );
}

#[then(expr = "the response field {string} should be at least {int}")]
async fn response_field_at_least(world: &mut GuiderWorld, field: String, floor: u64) {
    let body = &world.last_response().body;
    let value = body[&field]
        .as_u64()
        .unwrap_or_else(|| panic!("field {field} is not a whole number in {body}"));
    assert!(value >= floor, "field {field} is {value}, below {floor}");
}

/// How the unit's first main process ended.
fn first_exit(world: &GuiderWorld) -> std::process::ExitStatus {
    let exits = world
        .unit
        .as_ref()
        .expect("no rusty-photon-phd2 unit in this scenario")
        .exits();
    *exits.first().expect("the unit's first PHD2 has not exited")
}

#[then(expr = "the first PHD2 exited on its own with code {int}")]
async fn first_phd2_exited(world: &mut GuiderWorld, code: i32) {
    let status = first_exit(world);
    assert_eq!(
        status.code(),
        Some(code),
        "the first PHD2 ended with {status}"
    );
}

#[then("the first PHD2 was ended by SIGTERM")]
async fn first_phd2_terminated(world: &mut GuiderWorld) {
    let status = first_exit(world);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        // SIGTERM is 15 on every Unix this suite runs on.
        assert_eq!(
            status.signal(),
            Some(15),
            "the first PHD2 ended with {status}"
        );
    }
    #[cfg(not(unix))]
    panic!("a scenario that reads a signal is tagged @unix; the first PHD2 ended with {status}");
}

#[then(expr = "the guider service should have logged {string}")]
async fn service_logged(world: &mut GuiderWorld, text: String) {
    let handle = world
        .service_handle
        .as_ref()
        .expect("guider service not started");
    // A line reaches the suite through a pipe and a forwarding task.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !handle
        .stderr_lines()
        .iter()
        .any(|line| line.contains(&text))
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the guider service never logged {text:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[then("the mock PHD2s should have received these requests in this order:")]
async fn mock_requests_in_order(world: &mut GuiderWorld, step: &cucumber::gherkin::Step) {
    let table = step.table.as_ref().expect("the step needs a data table");
    let logged = world.logged_rpcs();
    let mut rest = logged.iter();
    for row in table.rows.iter().skip(1) {
        let (method, params) = (&row[0], &row[1]);
        let expected: Option<serde_json::Value> = (!params.is_empty()).then(|| {
            serde_json::from_str(params)
                .unwrap_or_else(|e| panic!("params {params:?} must be JSON: {e}"))
        });
        let found = rest.any(|rpc| {
            rpc["method"] == method.as_str()
                && expected
                    .as_ref()
                    .is_none_or(|expected| &rpc["params"] == expected)
        });
        assert!(
            found,
            "no {method} {params} after the requests before it, in {logged:?}"
        );
    }
}
