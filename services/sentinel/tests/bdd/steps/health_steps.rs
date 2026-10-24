//! BDD step definitions for service discovery and health supervision.

use std::time::Duration;

use cucumber::{given, then, when};

use crate::world::SentinelWorld;

#[given(expr = "a discovered unit {string} in state {string}")]
fn discovered_unit(world: &mut SentinelWorld, unit: String, state: String) {
    world.add_discovered_unit(&unit, &state);
}

#[given(expr = "a stub service whose health endpoint answers {int}")]
async fn health_stub_answering(world: &mut SentinelWorld, status: u16) {
    world.start_health_stub(status).await;
}

#[given(expr = "a stub service whose health endpoint answers {int} with body {string}")]
async fn health_stub_answering_with_body(world: &mut SentinelWorld, status: u16, body: String) {
    world.start_health_stub_with_body(status, body).await;
}

#[given(expr = "the stub service is discovered as {string} in state {string}")]
fn stub_discovered_as(world: &mut SentinelWorld, service: String, state: String) {
    world.discover_health_stub_as(&service, &state);
}

#[given(expr = "a probe domain {string} is configured")]
fn probe_domain_configured(world: &mut SentinelWorld, domain: String) {
    world.sentinel_probe_domain = Some(domain);
}

#[given("sentinel is running with notifiers and no monitors")]
async fn sentinel_with_notifiers(world: &mut SentinelWorld) {
    world.sentinel_has_notifiers = true;
    world.start_sentinel().await;
}

#[given(expr = "the service manager fails restarts of {string}")]
fn manager_fails_restarts(world: &mut SentinelWorld, unit: String) {
    world.fail_restarts_of(&unit);
}

#[given("the service manager leaves restarted units in their prior state")]
fn manager_leaves_units_stuck(world: &mut SentinelWorld) {
    world.leave_restarted_units_stuck();
}

#[when(expr = "the unit {string} appears in state {string}")]
fn unit_appears(world: &mut SentinelWorld, unit: String, state: String) {
    world.add_discovered_unit(&unit, &state);
}

#[when(expr = "the unit {string} is removed")]
fn unit_removed(world: &mut SentinelWorld, unit: String) {
    world.remove_discovered_unit(&unit);
}

#[when(expr = "the dashboard reports service {string} health {string}")]
#[then(expr = "the dashboard reports service {string} health {string}")]
async fn dashboard_reports_service_health(
    world: &mut SentinelWorld,
    name: String,
    expected: String,
) {
    let last = world.wait_for_service_health(&name, &expected).await;
    assert_eq!(
        last.as_deref(),
        Some(expected.as_str()),
        "service '{name}' never reported health '{expected}'"
    );
}

#[then("the service manager records no restarts after a settle period")]
async fn no_restarts_after_settle(world: &mut SentinelWorld) {
    // A negative assertion needs a fixed window: with the stub policy's
    // 200ms probes, 4 consecutive failures (not 1-2) are needed to trip a
    // restart, so a single slow/timed-out probe under CI load never reads
    // as a dead service — see issue #677.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let log = world.restart_log();
    assert!(log.is_empty(), "unexpected restarts recorded: {log:?}");
}

#[when(
    expr = "the service manager records at least {int} restart(s) of {string} within {int} seconds"
)]
#[then(
    expr = "the service manager records at least {int} restart(s) of {string} within {int} seconds"
)]
async fn manager_records_restarts(
    world: &mut SentinelWorld,
    min: usize,
    unit: String,
    ceiling_secs: u64,
) {
    let count = world
        .wait_for_restarts(&unit, min, Duration::from_secs(ceiling_secs))
        .await;
    assert!(
        count >= min,
        "expected at least {min} restart(s) of {unit} within {ceiling_secs}s, saw {count}"
    );
}

#[then(expr = "the service manager records a restart of {string}")]
fn manager_records_a_restart(world: &mut SentinelWorld, unit: String) {
    let log = world.restart_log();
    assert!(
        log.contains(&unit),
        "no restart of {unit} recorded: {log:?}"
    );
}

#[when("the stub service becomes healthy")]
fn stub_becomes_healthy(world: &mut SentinelWorld) {
    world
        .health_stub
        .as_ref()
        .expect("health stub not started")
        .set_status(200);
}

#[when(expr = "the stub service starts answering {int}")]
fn stub_starts_answering(world: &mut SentinelWorld, status: u16) {
    world
        .health_stub
        .as_ref()
        .expect("health stub not started")
        .set_status(status);
}

#[then(expr = "the dashboard reports zero restarts in the current outage for {string}")]
async fn zero_restarts_in_outage(world: &mut SentinelWorld, name: String) {
    let services = world.get_services().await;
    let service = services
        .iter()
        .find(|s| s["name"].as_str() == Some(name.as_str()))
        .unwrap_or_else(|| panic!("service '{name}' not in /api/services: {services:?}"));
    assert_eq!(
        service["restarts_in_outage"].as_u64(),
        Some(0),
        "outage counter did not reset: {service}"
    );
}

#[then(expr = "the notification history records an autonomous restart of {string}")]
async fn history_records_autonomous_restart(world: &mut SentinelWorld, name: String) {
    let history = world
        .wait_for_history(|record| {
            record["monitor_name"].as_str() == Some(name.as_str())
                && record["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("restarted autonomously"))
        })
        .await;
    assert!(
        history.iter().any(|record| {
            record["monitor_name"].as_str() == Some(name.as_str())
                && record["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("restarted autonomously"))
        }),
        "no autonomous-restart record for '{name}' in history: {history:?}"
    );
}

#[then(
    expr = "the notification history records an escalated still-unhealthy notification for {string}"
)]
async fn history_records_escalation(world: &mut SentinelWorld, name: String) {
    let history = world
        .wait_for_history(|record| {
            record["monitor_name"].as_str() == Some(name.as_str())
                && record["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("still unhealthy"))
        })
        .await;
    assert!(
        history.iter().any(|record| {
            record["monitor_name"].as_str() == Some(name.as_str())
                && record["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("still unhealthy"))
        }),
        "no still-unhealthy escalation for '{name}' in history: {history:?}"
    );
}

#[then(expr = "the dashboard reports a scheduled next restart for {string}")]
async fn dashboard_reports_next_restart(world: &mut SentinelWorld, name: String) {
    for _ in 0..60 {
        let services = world.get_services().await;
        if let Some(service) = services
            .iter()
            .find(|s| s["name"].as_str() == Some(name.as_str()))
        {
            if service["next_restart_epoch_ms"].as_u64().is_some() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("service '{name}' never reported a scheduled next restart");
}

#[when("the services endpoint is requested")]
async fn request_services_endpoint(world: &mut SentinelWorld) {
    world.http_get("/api/services").await;
}

/// Find `name` in the captured `/api/services` response body.
fn captured_service(world: &SentinelWorld, name: &str) -> serde_json::Value {
    let body = world
        .last_response_body
        .as_ref()
        .expect("no response body captured");
    let services: Vec<serde_json::Value> = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("services response is not JSON ({e}): {body}"));
    services
        .iter()
        .find(|s| s["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("service '{name}' not listed: {services:?}"))
        .clone()
}

#[then(expr = "the services response lists {string} with health {string}")]
fn services_response_lists_health(world: &mut SentinelWorld, name: String, health: String) {
    let service = captured_service(world, &name);
    assert_eq!(
        service["health"].as_str(),
        Some(health.as_str()),
        "unexpected health: {service}"
    );
}

#[then(expr = "the services response lists {string} with health message {string}")]
fn services_response_lists_health_message(
    world: &mut SentinelWorld,
    name: String,
    message: String,
) {
    let service = captured_service(world, &name);
    assert_eq!(
        service["health_message"].as_str(),
        Some(message.as_str()),
        "unexpected health message: {service}"
    );
}

#[then(expr = "the services response lists {string} with no health message")]
fn services_response_lists_no_health_message(world: &mut SentinelWorld, name: String) {
    let service = captured_service(world, &name);
    assert!(
        service["health_message"].is_null(),
        "expected no health message: {service}"
    );
}

#[then(expr = "the services response lists {string} with run state {string}")]
fn services_response_lists_run_state(world: &mut SentinelWorld, name: String, run_state: String) {
    let service = captured_service(world, &name);
    assert_eq!(
        service["run_state"].as_str(),
        Some(run_state.as_str()),
        "unexpected run state: {service}"
    );
}

#[then(expr = "the services response does not list {string}")]
fn services_response_does_not_list(world: &mut SentinelWorld, name: String) {
    let body = world
        .last_response_body
        .as_ref()
        .expect("no response body captured");
    let services: Vec<serde_json::Value> = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("services response is not JSON ({e}): {body}"));
    assert!(
        !services
            .iter()
            .any(|s| s["name"].as_str() == Some(name.as_str())),
        "service '{name}' should not be listed: {services:?}"
    );
}

#[when(expr = "the services response eventually lists {string} with run state {string}")]
#[then(expr = "the services response eventually lists {string} with run state {string}")]
async fn services_eventually_lists(world: &mut SentinelWorld, name: String, run_state: String) {
    let mut last: Option<serde_json::Value> = None;
    for _ in 0..60 {
        let services = world.get_services().await;
        if let Some(service) = services
            .iter()
            .find(|s| s["name"].as_str() == Some(name.as_str()))
        {
            if service["run_state"].as_str() == Some(run_state.as_str()) {
                return;
            }
            last = Some(service.clone());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("service '{name}' never reported run state '{run_state}': {last:?}");
}

#[then(expr = "the services response eventually does not list {string}")]
async fn services_eventually_does_not_list(world: &mut SentinelWorld, name: String) {
    for _ in 0..60 {
        let services = world.get_services().await;
        if !services
            .iter()
            .any(|s| s["name"].as_str() == Some(name.as_str()))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("service '{name}' was never dropped from /api/services");
}
