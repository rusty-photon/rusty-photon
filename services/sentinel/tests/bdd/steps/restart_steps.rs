//! BDD step definitions for the service restart API feature.
//!
//! The discovered units, the recorded restarts, and the recovery answers all
//! live in the stub service manager (`SENTINEL_SERVICE_MANAGER_DIR`); the
//! discovery/manager Given/Then steps themselves are shared with the health
//! feature and live in `health_steps.rs`.

use bdd_infra::setup_pages::dom;
use cucumber::{then, when};

use crate::world::SentinelWorld;

fn restart_path(name: &str) -> String {
    format!("/api/services/{name}/restart")
}

/// Posted as a client that is not a browser posts it: naming the
/// dashboard's own origin.
#[when(expr = "the restart endpoint is requested for {string}")]
async fn request_restart(world: &mut SentinelWorld, name: String) {
    let origin = world.dashboard_origin();
    world
        .http_post(&restart_path(&name), &[("origin", &origin)])
        .await;
}

#[when(expr = "the restart endpoint is requested for {string} with Sec-Fetch-Site {string}")]
async fn request_restart_with_fetch_site(world: &mut SentinelWorld, name: String, site: String) {
    world
        .http_post(&restart_path(&name), &[("sec-fetch-site", &site)])
        .await;
}

#[when(expr = "the restart endpoint is requested for {string} with Origin {string}")]
async fn request_restart_with_origin(world: &mut SentinelWorld, name: String, origin: String) {
    world
        .http_post(&restart_path(&name), &[("origin", &origin)])
        .await;
}

#[when(expr = "the restart endpoint is requested for {string} naming no origin")]
async fn request_restart_naming_no_origin(world: &mut SentinelWorld, name: String) {
    world.http_post(&restart_path(&name), &[]).await;
}

#[when("the dashboard page is requested")]
async fn request_dashboard_page(world: &mut SentinelWorld) {
    world.http_get("/").await;
}

#[then(expr = "the restart response reports status {string} and recovery {string}")]
fn restart_status_and_recovery(world: &mut SentinelWorld, status: String, recovery: String) {
    let json = restart_body(world);
    assert_eq!(
        json["status"].as_str(),
        Some(status.as_str()),
        "body: {json}"
    );
    assert_eq!(
        json["recovery"].as_str(),
        Some(recovery.as_str()),
        "body: {json}"
    );
}

#[then(expr = "the restart response reports status {string}")]
fn restart_status(world: &mut SentinelWorld, status: String) {
    let json = restart_body(world);
    assert_eq!(
        json["status"].as_str(),
        Some(status.as_str()),
        "body: {json}"
    );
}

#[then(expr = "the restart response's error names {string}")]
fn restart_error_names(world: &mut SentinelWorld, needle: String) {
    let json = restart_body(world);
    let error = json["error"]
        .as_str()
        .unwrap_or_else(|| panic!("no error in the body: {json}"));
    assert!(error.contains(&needle), "the error: {error}");
}

/// The restart endpoint answers only once its command has run, so a
/// refusal that has answered ran nothing, and the log can be read at once.
#[then("the service manager has recorded no restarts")]
fn no_restarts_recorded(world: &mut SentinelWorld) {
    let log = world.restart_log();
    assert!(log.is_empty(), "unexpected restarts recorded: {log:?}");
}

#[then(expr = "the dashboard's row for {string} has a Restart button posting to {string}")]
fn row_has_restart_button(world: &mut SentinelWorld, name: String, action: String) {
    let body = world
        .last_response_body
        .as_ref()
        .expect("no response body captured");
    let css = format!(
        r#"#service-body form.restart[method="post"][action="{action}"][data-service="{name}"] button[type="submit"]"#
    );
    assert!(
        dom::matches(body, &css),
        "no Restart button for {name} posting to {action}:\n{body}"
    );
}

fn restart_body(world: &SentinelWorld) -> serde_json::Value {
    let body = world
        .last_response_body
        .as_ref()
        .expect("no response body captured");
    serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("restart response is not JSON ({e}): {body}"))
}
