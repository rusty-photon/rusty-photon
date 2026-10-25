//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra`, plus the BFF's own — the BFF started on a temp config file
//! for its setup page, its `rp` block pointing at an address nothing answers
//! on (the page reads no rp), behind its own login or on a file still
//! carrying the retired `sentinel` block, and the page's text checks.

use bdd_infra::setup_pages::{self, SetupPagesWorld as _};
use cucumber::{given, then};
use serde_json::{json, Value};

use crate::world::{UiWorld, UNREACHABLE_PORT};

bdd_infra::setup_pages_steps!(UiWorld);

#[given("ui-htmx is started on a temp config file for its setup page")]
async fn started_for_setup_page(world: &mut UiWorld) {
    world.start_bff_with_rp_at(UNREACHABLE_PORT).await;
    adopt(world);
}

#[given(
    expr = "ui-htmx is started for its setup page presenting rp the username {string} with the password {string}"
)]
async fn started_with_rp_credential(world: &mut UiWorld, username: String, password: String) {
    world.start_bff_with_rp_at(UNREACHABLE_PORT).await;
    present_rp_credential(world, &username, &password);
    adopt(world);
}

#[given(
    expr = "ui-htmx is started behind its own login for its setup page presenting rp the username {string} with the password {string}"
)]
async fn started_behind_login_with_rp_credential(
    world: &mut UiWorld,
    username: String,
    password: String,
) {
    let pki = bdd_infra::tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await;
    world
        .start_bff_with_config(json!({
            "server": { "bind_address": "127.0.0.1", "port": 0, "auth": pki.auth_block() },
            "rp": { "base_url": format!("http://127.0.0.1:{UNREACHABLE_PORT}") }
        }))
        .await;
    present_rp_credential(world, &username, &password);
    adopt(world);
    world.setup_pages.credential = Some((pki.username().to_string(), pki.password().to_string()));
    world.setup_pages.stored_hash = Some(pki.password_hash().to_string());
}

#[given("ui-htmx is started for its setup page on a configuration still carrying the retired sentinel block")]
async fn started_with_retired_sentinel_block(world: &mut UiWorld) {
    world
        .start_bff_with_config(json!({
            "server": { "bind_address": "127.0.0.1", "port": 0 },
            "rp": { "base_url": format!("http://127.0.0.1:{UNREACHABLE_PORT}") },
            "sentinel": { "base_url": "http://127.0.0.1:11114" }
        }))
        .await;
    adopt(world);
}

/// Put the credential the BFF presents to rp into its file. It goes in
/// after the start: the BFF builds its rp client from the file as loaded,
/// and the page reads the file anew.
fn present_rp_credential(world: &UiWorld, username: &str, password: &str) {
    let path = world.ui_config_path.clone().expect("the BFF's config file");
    let mut config: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read the BFF's config"))
            .expect("the BFF's config is JSON");
    config["rp"]["auth"] = json!({ "username": username, "password": password });
    std::fs::write(&path, config.to_string()).expect("write the BFF's config");
}

fn adopt(world: &mut UiWorld) {
    let port = world.ui.as_ref().expect("the BFF started").bound_port();
    let path = world.ui_config_path.clone().expect("the BFF's config file");
    setup_pages::adopt_plain(&mut world.setup_pages, port, path);
}

#[then(expr = "the setup page does not contain {string}")]
fn page_lacks(world: &mut UiWorld, needle: String) {
    let body = &world.setup_pages().last_body;
    assert!(!body.is_empty(), "no page was fetched");
    assert!(
        !body.contains(&needle),
        "{needle:?} is on the page:\n{body}"
    );
}
