//! Steps shared across the bridge features: config knobs, service lifecycle,
//! the Align gesture, and ASCOM error assertions.

use cucumber::{given, then, when};

use crate::stub_rp::StubRp;
use crate::world::{ascom_code, unreachable_rp_url, BridgeWorld, FloorSetting, DEAD_RP_PORT};

// --- Config knobs (set before the bridge starts) ---

#[given("a stub rp MCP server is running")]
async fn stub_rp_running(world: &mut BridgeWorld) {
    let stub = StubRp::start().await;
    world.rp_url = Some(stub.mcp_url());
    world.stub_rp = Some(stub);
}

#[given("rp is unreachable")]
async fn rp_unreachable(world: &mut BridgeWorld) {
    world.rp_url = Some(unreachable_rp_url().await);
}

#[given(expr = "the reported-position altitude floor is {float} degrees")]
const fn floor_degrees(world: &mut BridgeWorld, floor: f64) {
    world.floor = FloorSetting::Degrees(floor);
}

#[given("the reported-position altitude floor is disabled")]
const fn floor_disabled(world: &mut BridgeWorld) {
    world.floor = FloorSetting::Null;
}

#[given(expr = "assume_epoch is {string}")]
fn assume_epoch(world: &mut BridgeWorld, epoch: String) {
    world.assume_epoch = Some(epoch);
}

#[given(expr = "the simulated slew duration is {string}")]
fn slew_duration(world: &mut BridgeWorld, duration: String) {
    world.slew_duration = Some(duration);
}

#[given(expr = "the spool is bounded to {int} entries")]
const fn spool_bounded(world: &mut BridgeWorld, max_entries: u64) {
    world.spool_max_entries = Some(max_entries);
}

#[given(expr = "the spool replay backoff is capped at {string}")]
fn replay_backoff(world: &mut BridgeWorld, backoff: String) {
    world.replay_backoff_max = Some(backoff);
}

#[given(expr = "the configured site is latitude {float} longitude {float}")]
const fn configured_site(world: &mut BridgeWorld, latitude: f64, longitude: f64) {
    world.site_latitude_deg = Some(latitude);
    world.site_longitude_deg = Some(longitude);
}

#[given("the stub rp rejects add_target calls")]
fn stub_rejects(world: &mut BridgeWorld) {
    world.stub().set_reject(true);
}

// --- Lifecycle ---

#[given("the bridge is running")]
#[when("the bridge is running")]
async fn bridge_running(world: &mut BridgeWorld) {
    world.start().await;
}

#[given("a connected planetarium client")]
async fn connected_client(world: &mut BridgeWorld) {
    world.connect().await;
}

#[when("the bridge is stopped")]
async fn bridge_stopped(world: &mut BridgeWorld) {
    world.stop_bridge().await;
}

#[when("the bridge is restarted with the same configuration")]
async fn bridge_restarted(world: &mut BridgeWorld) {
    world.restart().await;
}

#[when("the stub rp comes up at the configured rp address")]
async fn stub_comes_up(world: &mut BridgeWorld) {
    assert!(world.stub_rp.is_none(), "a stub rp is already running");
    let port = world.configured_rp_port();
    assert_ne!(
        port, DEAD_RP_PORT,
        "the configured rp address is the default dead URL, which no stub can bind: \
         say 'rp is unreachable' first, so the scenario gets a port of its own"
    );
    world.stub_rp = Some(StubRp::start_on(port).await);
}

// --- The Align gesture ---

#[when(expr = "the client aligns at ra {float} hours dec {float} degrees")]
async fn client_aligns(world: &mut BridgeWorld, ra_hours: f64, dec_degrees: f64) {
    world.try_sync(ra_hours, dec_degrees).await;
    assert_eq!(
        world.last_error_code, None,
        "Align at ra {ra_hours}h dec {dec_degrees} deg was rejected"
    );
}

#[when(expr = "the client tries to align at ra {float} hours dec {float} degrees")]
async fn client_tries_align(world: &mut BridgeWorld, ra_hours: f64, dec_degrees: f64) {
    world.try_sync(ra_hours, dec_degrees).await;
}

// --- Assertions ---

#[then(expr = "the call should be rejected with ASCOM {word}")]
fn rejected_with(world: &mut BridgeWorld, code_name: String) {
    let expected = ascom_code(&code_name);
    assert_eq!(
        world.last_error_code,
        Some(expected),
        "expected ASCOM {code_name} ({expected})"
    );
}

#[then("the align call should succeed")]
fn align_succeeded(world: &mut BridgeWorld) {
    assert_eq!(world.last_error_code, None, "the Align was rejected");
}
