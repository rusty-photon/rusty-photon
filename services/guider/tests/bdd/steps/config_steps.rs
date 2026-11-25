//! Steps of `configuration.feature`, and the configuration and start
//! steps every feature shares.

use cucumber::{given, then, when};
use serde_json::{json, Value};

use crate::world::GuiderWorld;

#[given(regex = r#"^a guider configuration with "([^"]+)" set to the JSON (.+)$"#)]
fn configuration_with(world: &mut GuiderWorld, pointer: String, value: String) {
    world.set_field(&pointer, json_value(&value));
}

#[given(regex = r#"^the guider configuration's "([^"]+)" is the JSON (.+)$"#)]
fn configuration_field(world: &mut GuiderWorld, pointer: String, value: String) {
    world.set_field(&pointer, json_value(&value));
}

#[given("a guider configuration naming no devices and no focal length")]
const fn naming_nothing(_world: &mut GuiderWorld) {}

#[given(expr = "a guider configuration naming both devices and a focal length of {float} mm")]
fn naming_both_with_focal_length(world: &mut GuiderWorld, focal_length_mm: f64) {
    world.name_camera = true;
    world.name_mount = true;
    world.set_field("/optics/focal_length_mm", json!(focal_length_mm));
}

#[given("a guider configuration naming both devices and no focal length")]
const fn naming_both_without_focal_length(world: &mut GuiderWorld) {
    world.name_camera = true;
    world.name_mount = true;
}

#[given(expr = "a guider configuration naming the camera only and a focal length of {float} mm")]
fn naming_the_camera_with_focal_length(world: &mut GuiderWorld, focal_length_mm: f64) {
    world.name_camera = true;
    world.set_field("/optics/focal_length_mm", json!(focal_length_mm));
}

#[given("a guider configuration naming the camera only and no focal length")]
const fn naming_the_camera_without_focal_length(world: &mut GuiderWorld) {
    world.name_camera = true;
}

#[given(
    "a guider configuration naming a focal length, and a camera and a mount whose servers do not answer"
)]
async fn naming_silent_devices(world: &mut GuiderWorld) {
    let camera = bdd_infra::reserved_test_port_async().await;
    let mount = bdd_infra::reserved_test_port_async().await;
    world.set_field(
        "/devices/camera",
        json!({ "alpaca_url": format!("http://127.0.0.1:{camera}") }),
    );
    world.set_field(
        "/devices/mount",
        json!({ "alpaca_url": format!("http://127.0.0.1:{mount}") }),
    );
    world.set_field("/optics/focal_length_mm", json!(206.265));
}

#[given("a guider configuration file that ends inside its first object")]
fn truncated_file(world: &mut GuiderWorld) {
    world.config_text = Some(r#"{ "server": "#.to_owned());
}

#[given("a guider configuration file named on the command line that does not exist")]
fn missing_file(world: &mut GuiderWorld) {
    world.config_path = Some(world.scratch_path().join("no-such-guider.json"));
}

#[given("a guider configuration whose ca_cert names a file that does not exist")]
fn missing_ca_cert(world: &mut GuiderWorld) {
    let path = world.scratch_path().join("no-such-ca.pem");
    world.set_field("/ca_cert", json!(path));
}

#[given(expr = "a guider configuration whose ca_cert names a file holding {string}")]
fn ca_cert_holding(world: &mut GuiderWorld, text: String) {
    let path = world.scratch_path().join("ca.pem");
    std::fs::write(&path, text).expect("write the CA file");
    world.set_field("/ca_cert", json!(path));
}

#[given("a guider configuration whose state directory lies under a regular file")]
fn state_dir_under_a_file(world: &mut GuiderWorld) {
    let file = world.scratch_path().join("a-file");
    std::fs::write(&file, b"").expect("write the file");
    world.set_field("/state_dir", json!(file.join("state")));
}

#[given("a guider configuration whose state directory cannot be written")]
fn read_only_state_dir(world: &mut GuiderWorld) {
    let dir = world.scratch_path().join("read-only-state");
    std::fs::create_dir(&dir).expect("create the state directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555))
            .expect("make the state directory read-only");
    }
    world.set_field("/state_dir", json!(dir));
}

#[given("a guider configuration whose state directory does not exist yet")]
fn absent_state_dir(world: &mut GuiderWorld) {
    let dir = world.scratch_path().join("not").join("yet");
    world.set_field("/state_dir", json!(dir));
}

#[when("the guider is started")]
async fn the_guider_is_started(world: &mut GuiderWorld) {
    world.start_guider().await;
}

#[then("the guider exits non-zero before it binds")]
fn exits_before_binding(world: &mut GuiderWorld) {
    assert!(
        world.guider.is_none(),
        "the guider started and is serving at {}",
        world.base_url()
    );
    let refusal = world.refusal.as_ref().expect("the guider was not started");
    assert_ne!(refusal.code, Some(0), "the guider exited 0: {refusal:?}");
    assert!(
        !refusal.stdout.contains("bound_addr="),
        "the guider bound before it exited: {refusal:?}"
    );
}

// A regex, not `{string}`: an expected message may itself hold quotes
// (docs/skills/testing.md §2.8).
#[then(regex = r#"^its error output names "(.*)"$"#)]
fn error_output_names(world: &mut GuiderWorld, text: String) {
    let refusal = world.refusal.as_ref().expect("the guider was not refused");
    assert!(
        refusal.stderr.contains(&text),
        "the guider's error output does not name {text:?}:\n{}",
        refusal.stderr
    );
}

#[then("the guider is serving")]
fn is_serving(world: &mut GuiderWorld) {
    assert!(
        world.guider.is_some(),
        "the guider did not start: {:?}",
        world.refusal
    );
}

#[then("the state directory exists")]
fn state_dir_exists(world: &mut GuiderWorld) {
    let dir = world.config["state_dir"]
        .as_str()
        .expect("the configuration names a state directory")
        .to_owned();
    assert!(
        std::path::Path::new(&dir).is_dir(),
        "the state directory {dir} does not exist"
    );
}

/// A table cell or the rest of a step line, read as JSON.
pub fn json_value(text: &str) -> Value {
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{text:?} is not JSON: {e}"))
}
