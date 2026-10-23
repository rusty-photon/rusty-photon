//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra`, plus the Givens that start the bridge behind TLS and auth
//! and give it a credential to present to rp, and the Then that keeps that
//! credential off the page.

use cucumber::{given, then};

use crate::world::BridgeWorld;

bdd_infra::setup_pages_steps!(BridgeWorld);

#[given("the bridge is running with TLS and auth")]
async fn running_with_tls_and_auth(world: &mut BridgeWorld) {
    world.start_with_tls_and_auth_pages().await;
}

#[given(expr = "the bridge presents the username {string} and password {string} to rp")]
fn presents_rp_credential(world: &mut BridgeWorld, username: String, password: String) {
    world.rp_service_auth = Some((username, password));
}

#[then(expr = "the page does not contain {string}")]
fn page_lacks(world: &mut BridgeWorld, needle: String) {
    let body = &world.setup_pages.last_body;
    assert!(!body.is_empty(), "no page was fetched");
    assert!(
        !body.contains(&needle),
        "{needle:?} is on the page:\n{body}"
    );
}
