//! Steps of `setup_pages.feature`: the shared steps from `bdd-infra`, plus
//! the guider's own: the guider started for its setup page, naming devices
//! whose servers take a credential, or behind its own login, and the
//! values a save submits that only the suite can make (a directory under a
//! file, a CA certificate on disk).

use bdd_infra::setup_pages::{self, SetupPagesWorld as _};
use bdd_infra::tls_auth::{self, TlsAuthSmokeWorld as _};
use cucumber::{given, then, when};
use serde_json::json;

use crate::world::GuiderWorld;

bdd_infra::setup_pages_steps!(GuiderWorld);

/// Where the devices of these scenarios are: an address nothing answers
/// on, since a save never reaches a device.
const NOBODY: &str = "http://127.0.0.1:1";

#[given("the guider is started for its setup page")]
async fn started_for_setup_page(world: &mut GuiderWorld) {
    world.start_guider().await;
    adopt(world);
}

#[given(
    expr = "the guider is started for its setup page naming a camera and a mount whose servers take the username {string} and the password {string}"
)]
async fn started_with_device_credentials(
    world: &mut GuiderWorld,
    username: String,
    password: String,
) {
    let auth = json!({ "username": username, "password": password });
    world.set_field(
        "/devices/camera",
        json!({ "alpaca_url": NOBODY, "auth": auth }),
    );
    world.set_field(
        "/devices/mount",
        json!({ "alpaca_url": NOBODY, "auth": auth }),
    );
    world.start_guider().await;
    adopt(world);
}

#[given("the guider is started behind its own login for its setup page")]
async fn started_behind_login(world: &mut GuiderWorld) {
    let pki = tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await;
    let mut config = world.base_test_config();
    config["server"] = pki.server_block(0);
    world.tls_auth.pki = Some(pki);
    world.start_with_tls_auth(config).await;
    let path = tls_auth::staged_config_path(&world.tls_auth).expect("the staged config file");
    setup_pages::adopt_tls_auth(&mut world.setup_pages, &world.tls_auth, path);
}

#[when(expr = "I submit the server setup page setting {string} to a directory under a file")]
async fn submit_directory_under_file(world: &mut GuiderWorld, name: String) {
    let file = world.scratch_path().join("a-file");
    std::fs::write(&file, b"").expect("write a file to put a directory under");
    let value = file.join("state").to_string_lossy().into_owned();
    submit(world, &name, &value).await;
}

#[when(
    expr = "I submit the server setup page setting {string} to {string} and {string} to a directory that does not exist yet"
)]
async fn submit_with_new_directory(
    world: &mut GuiderWorld,
    name: String,
    value: String,
    directory_field: String,
) {
    let directory = world
        .scratch_path()
        .join("not-yet")
        .join("state")
        .to_string_lossy()
        .into_owned();
    setup_pages::submit(
        world.setup_pages(),
        setup_pages::SERVER_PATH,
        &[(&name, &value), (&directory_field, &directory)],
        None,
    )
    .await;
}

#[then("the submitted state directory does not exist")]
fn new_directory_absent(world: &mut GuiderWorld) {
    let top = world.scratch_path().join("not-yet");
    assert!(!top.exists(), "{} was created", top.display());
}

#[when(expr = "I submit the server setup page setting {string} to the suite's CA certificate")]
async fn submit_ca_certificate(world: &mut GuiderWorld, name: String) {
    let pki = tls_auth::shared_pki(env!("CARGO_PKG_NAME")).await;
    let value = pki.ca_path().to_string_lossy().into_owned();
    submit(world, &name, &value).await;
}

#[then(expr = "the setup page does not contain {string}")]
fn page_lacks(world: &mut GuiderWorld, needle: String) {
    let body = &world.setup_pages().last_body;
    assert!(!body.is_empty(), "no page was fetched");
    assert!(
        !body.contains(&needle),
        "{needle:?} is on the page:\n{body}"
    );
}

/// Submit the server page with `name` set to `value`, as its own form
/// would.
async fn submit(world: &mut GuiderWorld, name: &str, value: &str) {
    setup_pages::submit(
        world.setup_pages(),
        setup_pages::SERVER_PATH,
        &[(name, value)],
        None,
    )
    .await;
}

/// Hand the page steps the plain guider's address and its config file.
fn adopt(world: &mut GuiderWorld) {
    let port = world
        .guider
        .as_ref()
        .expect("the guider started")
        .bound_port();
    let path = world.scratch_path().join("guider.json");
    setup_pages::adopt_plain(&mut world.setup_pages, port, path);
}
