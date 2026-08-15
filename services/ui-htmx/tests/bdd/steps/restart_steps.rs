//! Step definitions for the Restart-via-Sentinel feature: a real sentinel is
//! spawned alongside the driver + BFF, discovering units from the stub
//! service manager (`SENTINEL_SERVICE_MANAGER_DIR`) and recording restarts in
//! its log — proving the whole BFF → sentinel → service-manager chain end to
//! end.

use cucumber::{given, then, when};

use crate::dom;
use crate::world::UiWorld;

#[given(expr = "a sentinel that has discovered the running {string} unit")]
async fn sentinel_with_running_unit(world: &mut UiWorld, service: String) {
    world.add_discovered_unit(&format!("rusty-photon-{service}"), "running");
    world.start_sentinel_and_rewire_bff().await;
}

#[given(expr = "the service manager fails restarts of {string}")]
fn service_manager_fails_restarts(world: &mut UiWorld, unit: String) {
    world.fail_restarts_of(&unit);
}

#[given("a sentinel that has discovered no services")]
async fn sentinel_with_no_services(world: &mut UiWorld) {
    world.start_sentinel_and_rewire_bff().await;
}

#[when("I request a restart of the dsd-fp2 driver")]
async fn request_restart(world: &mut UiWorld) {
    world.request_restart().await;
}

#[when("I request a restart of rp")]
async fn request_restart_of_rp(world: &mut UiWorld) {
    world.request_restart_at("/config/rp").await;
}

#[then("the page offers to restart the driver via Sentinel")]
fn offers_restart(world: &mut UiWorld) {
    let restart_path = format!("{}/restart", UiWorld::device_config_path());
    assert_eq!(
        dom::attr(&world.last_body, "button.restart-sentinel", "hx-post").as_deref(),
        Some(restart_path.as_str()),
        "missing restart affordance:\n{}",
        world.last_body
    );
}

#[then("the page offers no restart affordance")]
fn no_restart_affordance(world: &mut UiWorld) {
    assert!(
        !dom::matches(&world.last_body, "button.restart-sentinel"),
        "unexpected restart affordance with no sentinel configured:\n{}",
        world.last_body
    );
}

#[then(expr = "the service manager records a restart of {string}")]
fn service_manager_recorded_restart(world: &mut UiWorld, unit: String) {
    let log = world.restart_log();
    assert!(
        log.iter().any(|line| line == &unit),
        "no restart of {unit} in the service manager's log: {log:?}"
    );
}

#[then("the page reports the driver is restarting")]
fn reports_restarting(world: &mut UiWorld) {
    assert!(
        dom::text_contains(&world.last_body, "div.banner.applying", "restart"),
        "missing restarting banner:\n{}",
        world.last_body
    );
}

#[then("the page reports the restart failed")]
fn reports_restart_failed(world: &mut UiWorld) {
    assert!(
        dom::text_contains(
            &world.last_body,
            "div.banner.error",
            "could not restart the driver"
        ),
        "missing restart-failed banner:\n{}",
        world.last_body
    );
}

#[then("the page reports Sentinel does not supervise the driver")]
fn reports_not_supervised(world: &mut UiWorld) {
    assert!(
        dom::text_contains(&world.last_body, "div.banner.error", "does not supervise"),
        "missing not-supervised banner:\n{}",
        world.last_body
    );
}
