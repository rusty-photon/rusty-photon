//! Step definitions for `setup_pages.feature`: the shared steps from
//! `bdd-infra`, plus rp's own — the temp configurations the roster
//! scenarios start from, the page reached by another host name, the
//! roster rows' links and states, and the saves that edit the page drawn
//! again.
//!
//! Like `config_rest.feature`, these scenarios spawn their own rp on port 0
//! from a scenario-private temp config, its entries pointing at addresses
//! nothing answers on (or at the stub camera facade), so no simulator runs.

use bdd_infra::rp_harness::FacadeCameraStub;
use bdd_infra::setup_pages::{self, dom, SetupPagesWorld as _};
use bdd_infra::ServiceHandle;
use cucumber::gherkin::Step;
use cucumber::{given, then, when};
use serde_json::{json, Value};

use super::config_rest_steps::write_scenario_config;
use crate::world::RpWorld;

bdd_infra::setup_pages_steps!(RpWorld);

/// The scenario's config file as JSON.
fn config_file(world: &RpWorld) -> (std::path::PathBuf, Value) {
    let path = world
        .config_rest_path
        .clone()
        .expect("no temp rp config — add a 'Given a temp rp config ...' step");
    let value = serde_json::from_str(&std::fs::read_to_string(&path).expect("read rp config"))
        .expect("rp config is JSON");
    (path, value)
}

fn write_config_file(path: &std::path::Path, value: &Value) {
    std::fs::write(
        path,
        serde_json::to_string_pretty(value).expect("serialize rp config"),
    )
    .expect("write rp config");
}

/// The table's rows after its header, as `(header, cell)` maps.
fn table_rows(step: &Step) -> Vec<Vec<(String, String)>> {
    let table = step.table.as_ref().expect("the step carries a table");
    let mut rows = table.rows.iter();
    let header = rows.next().expect("the table has a header").clone();
    rows.map(|row| header.iter().cloned().zip(row.iter().cloned()).collect())
        .collect()
}

#[given("a temp rp config whose roster holds:")]
fn config_with_roster(world: &mut RpWorld, step: &Step) {
    let mut equipment = json!({});
    for row in table_rows(step) {
        let cell = |name: &str| {
            row.iter().find(|(h, _)| h == name).map_or_else(
                || panic!("the roster table has no {name} column"),
                |(_, v)| v.clone(),
            )
        };
        let entry = json!({
            "id": cell("id"),
            "alpaca_url": cell("alpaca_url"),
            "device_number": cell("device_number").parse::<u32>().expect("a device number"),
        });
        let kind = cell("kind");
        match equipment.get_mut(&kind).and_then(Value::as_array_mut) {
            Some(entries) => entries.push(entry),
            None => equipment[kind] = json!([entry]),
        }
    }
    write_scenario_config(world, equipment);
}

#[given(
    expr = "a temp rp config whose camera {string} is the stub camera facade and whose mount guides through {string}"
)]
async fn config_with_facade(world: &mut RpWorld, id: String, guider: String) {
    let facade = FacadeCameraStub::start().await;
    write_scenario_config(
        world,
        json!({
            "cameras": [{ "id": id, "alpaca_url": facade.url(), "device_number": 0 }],
            "mount": {
                "alpaca_url": "http://127.0.0.1:1",
                "device_number": 0,
                "guiding": { "url": guider }
            }
        }),
    );
    world.facade_stub = Some(facade);
}

#[given(expr = "the entry {string} presents the username {string} with the password {string}")]
fn entry_presents_credential(world: &mut RpWorld, id: String, username: String, password: String) {
    let (path, mut config) = config_file(world);
    let entry = config["equipment"]
        .as_object_mut()
        .expect("the config has an equipment block")
        .values_mut()
        .filter_map(Value::as_array_mut)
        .flatten()
        .find(|entry| entry["id"] == id.as_str())
        .unwrap_or_else(|| panic!("no entry {id} in the roster"));
    entry["auth"] = json!({ "username": username, "password": password });
    write_config_file(&path, &config);
}

#[given("rp is started on that config file for its setup page")]
async fn rp_started_for_setup_page(world: &mut RpWorld) {
    let (path, _) = config_file(world);
    let handle =
        ServiceHandle::start(env!("CARGO_PKG_NAME"), path.to_str().expect("utf-8 path")).await;
    setup_pages::adopt_plain(&mut world.setup_pages, handle.bound_port(), path);
    world.rp = Some(handle);
    assert!(
        world.wait_for_rp_healthy().await,
        "rp did not become healthy"
    );
}

#[given("rp is started behind TLS and auth for its setup page")]
async fn rp_started_behind_tls_and_auth(world: &mut RpWorld) {
    write_scenario_config(world, json!({}));
    let (path, mut config) = config_file(world);
    config["server"]["tls"] = world.pki().tls_block();
    config["server"]["auth"] = world.pki().auth_block();
    write_config_file(&path, &config);
    let handle =
        ServiceHandle::start(env!("CARGO_PKG_NAME"), path.to_str().expect("utf-8 path")).await;
    let pki = world
        .pki
        .clone()
        .expect("no PKI — add 'Given generated TLS certificates'");
    let url = format!("https://localhost:{}/health", handle.port());
    bdd_infra::tls_auth::wait_until_ready(
        &pki.https_client(),
        &url,
        pki.username(),
        pki.password(),
    )
    .await;
    let state = &mut world.setup_pages;
    state.host = Some("https://localhost");
    state.port = Some(handle.bound_port());
    state.client = Some(pki.https_client());
    state.credential = Some((pki.username().to_string(), pki.password().to_string()));
    state.stored_hash = Some(pki.password_hash().to_string());
    state.config_path = Some(path);
    world.rp = Some(handle);
}

#[given(expr = "the configuration file gains the camera {string} behind the page's back")]
#[when(expr = "the configuration file gains the camera {string} behind the page's back")]
fn file_gains_camera(world: &mut RpWorld, id: String) {
    let (path, mut config) = config_file(world);
    config["equipment"]["cameras"]
        .as_array_mut()
        .expect("the config has a camera list")
        .push(json!({ "id": id, "alpaca_url": "http://127.0.0.1:9", "device_number": 0 }));
    write_config_file(&path, &config);
}

#[when(expr = "I open rp's setup page reached as {string}")]
async fn open_reached_as(world: &mut RpWorld, host: String) {
    let state = world.setup_pages();
    let port = state.port.as_ref().expect("rp was not started").get();
    let response = state
        .client
        .as_ref()
        .expect("rp was not started")
        .get(format!(
            "http://127.0.0.1:{port}{}",
            setup_pages::SERVER_PATH
        ))
        .header(reqwest::header::HOST, format!("{host}:{port}"))
        .send()
        .await
        .expect("the setup page GET failed");
    state.last_status = Some(response.status().as_u16());
    state.last_body = response.text().await.unwrap_or_default();
    state.last_path = Some(setup_pages::SERVER_PATH.to_string());
}

#[when("I submit the server setup page with:")]
async fn submit_with_table(world: &mut RpWorld, step: &Step) {
    let table = step.table.as_ref().expect("the step carries a table");
    let changes: Vec<(String, String)> = table
        .rows
        .iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect();
    let changes: Vec<(&str, &str)> = changes
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    setup_pages::submit(
        world.setup_pages(),
        setup_pages::SERVER_PATH,
        &changes,
        None,
    )
    .await;
}

/// Save the page last drawn — the form a rejected save drew again, or the
/// page opened before the file changed — as its own form would, with one
/// control changed.
#[when(expr = "I save the page again setting {string} to {string}")]
async fn save_again_setting(world: &mut RpWorld, name: String, value: String) {
    let state = world.setup_pages();
    let path = state
        .last_path
        .clone()
        .expect("no page was drawn to save again");
    let mut controls = dom::successful_controls(&state.last_body);
    controls.retain(|(k, _)| *k != name);
    controls.push((name, value));
    setup_pages::post(state, &path, &controls, None).await;
}

fn row_css(id: &str) -> String {
    format!(r#"details.entry[data-entry="{id}"]"#)
}

fn assert_page_matches(world: &mut RpWorld, css: &str, what: &str) {
    let body = &world.setup_pages().last_body;
    assert!(dom::matches(body, css), "{what}:\n{body}");
}

#[then(expr = "the roster row {string} links its driver's setup page at {string}")]
fn row_links_driver(world: &mut RpWorld, id: String, href: String) {
    let css = format!(r#"{} a.server-setup[href="{href}"]"#, row_css(&id));
    assert_page_matches(world, &css, &format!("{id} does not link {href}"));
}

#[then(expr = "the roster row {string} links its device's page at {string}")]
fn row_links_device(world: &mut RpWorld, id: String, href: String) {
    let css = format!(r#"{} a.device-setup[href="{href}"]"#, row_css(&id));
    assert_page_matches(world, &css, &format!("{id} does not link {href}"));
}

#[then(expr = "the roster row {string} links the guider's setup page at {string}")]
fn row_links_guider(world: &mut RpWorld, id: String, href: String) {
    let css = format!(r#"{} a.guider-setup[href="{href}"]"#, row_css(&id));
    assert_page_matches(
        world,
        &css,
        &format!("{id} does not link the guider at {href}"),
    );
}

#[then(expr = "the roster row {string} reads {string}")]
fn row_reads(world: &mut RpWorld, id: String, state: String) {
    let body = &world.setup_pages().last_body;
    let css = format!("{} dd.entry-state", row_css(&id));
    assert!(
        dom::text_contains(body, &css, &state),
        "the row {id} does not read {state:?}:\n{body}"
    );
}

#[then(expr = "the page does not contain {string}")]
fn page_lacks(world: &mut RpWorld, needle: String) {
    let body = &world.setup_pages().last_body;
    assert!(!body.is_empty(), "no page was fetched");
    assert!(
        !body.contains(&needle),
        "{needle:?} is on the page:\n{body}"
    );
}
