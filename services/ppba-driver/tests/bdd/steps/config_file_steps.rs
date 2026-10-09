//! Step definitions for `config_file.feature`

use crate::steps::infrastructure::ServiceHandle;
use crate::world::PpbaWorld;
use cucumber::{given, then, when};

/// A hand-written config naming only the serial port and the server, the
/// shape an operator writes first. Port 0 lets the OS pick the port; the
/// mock transport accepts any serial path.
fn serial_and_server_only() -> serde_json::Value {
    serde_json::json!({
        "serial": { "port": "/dev/mock", "polling_interval": "200ms" },
        "server": { "port": 0 }
    })
}

/// Whether `s` is a hyphenated `UUIDv4`: 8-4-4-4-12 hex digits, version
/// nibble `4`, variant nibble `8`–`b`.
fn is_uuid_v4(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    let shape = groups.iter().map(|g| g.len()).collect::<Vec<_>>() == [8, 4, 4, 4, 12];
    shape
        && groups
            .iter()
            .all(|g| g.chars().all(|c| c.is_ascii_hexdigit()))
        && groups[2].starts_with('4')
        && groups[3].starts_with(['8', '9', 'a', 'b', 'A', 'B'])
}

impl PpbaWorld {
    /// The config file as it is on disk now, parsed.
    fn config_file_now(&self) -> serde_json::Value {
        let (path, _) = self.config_file.as_ref().expect("no config file staged");
        let text = std::fs::read_to_string(path).expect("read config file");
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("config file is not JSON ({e}):\n{text}"))
    }

    /// stderr of the refused start, for an assertion message.
    fn refused_stderr(&self) -> String {
        self.refused_start
            .as_ref()
            .map_or_default(|out| String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

// ============================================================================
// Given steps
// ============================================================================

#[given("a config file holding only the serial and server sections")]
fn config_file_serial_and_server_only(world: &mut PpbaWorld) {
    world.config = serial_and_server_only();
}

#[given(expr = "the config file's serial section carries the unknown key {string}")]
fn config_file_unknown_serial_key(world: &mut PpbaWorld, key: String) {
    world.config["serial"][key] = serde_json::json!(9600);
}

#[given(expr = "the config file has an empty {string} section")]
fn config_file_empty_section(world: &mut PpbaWorld, section: String) {
    world.config[section] = serde_json::json!({});
}

#[given("a config file that ends in the middle of the serial section")]
fn config_file_cut_off(world: &mut PpbaWorld) {
    world.config_file_text = Some("{\n  \"serial\": {\n    \"port\": \"/dev/mock\",\n".to_string());
}

// ============================================================================
// When steps
// ============================================================================

#[when("the driver starts with the config file")]
async fn driver_starts_with_config_file(world: &mut PpbaWorld) {
    let dir = bdd_infra::scratch::new_dir("ppba-config-file-").expect("scratch dir");
    let path = dir.path().join("ppba.json");
    let text = world
        .config_file_text
        .take()
        .unwrap_or_else(|| serde_json::to_string_pretty(&world.config).expect("serialize config"));
    std::fs::write(&path, &text).expect("write config file");
    world.config_file = Some((path.clone(), text.into_bytes()));
    world.config_dir = Some(dir);

    // A driver that comes up is held for the Then steps (and stopped by the
    // after hook). One that does not is run again to its exit, to capture the
    // status and stderr the refusal printed.
    let path = path.to_str().expect("utf-8 scratch path");
    match ServiceHandle::try_start(env!("CARGO_PKG_NAME"), path).await {
        Ok(handle) => {
            world.base_url = Some(handle.base_url.clone());
            world.ppba = Some(handle);
        }
        Err(_) => {
            world.refused_start = Some(
                bdd_infra::run_once_async(env!("CARGO_PKG_NAME"), &["--config", path], None).await,
            );
        }
    }
}

// ============================================================================
// Then steps
// ============================================================================

#[then("the driver is serving")]
fn driver_is_serving(world: &mut PpbaWorld) {
    assert!(
        world.ppba.is_some(),
        "the driver did not start; stderr:\n{}",
        world.refused_stderr()
    );
}

#[then("the driver refuses to start")]
fn driver_refuses_to_start(world: &mut PpbaWorld) {
    assert!(world.ppba.is_none(), "the driver started");
    let out = world
        .refused_start
        .as_ref()
        .expect("no refused start recorded");
    assert!(
        !out.status.success(),
        "the driver exited successfully; stderr:\n{}",
        world.refused_stderr()
    );
}

#[then(expr = "the start error contains {string}")]
fn start_error_contains(world: &mut PpbaWorld, needle: String) {
    let stderr = world.refused_stderr();
    assert!(
        stderr.contains(&needle),
        "expected the start error to contain {needle:?}; stderr:\n{stderr}"
    );
}

#[then("the config file is byte for byte as it was written")]
fn config_file_unchanged(world: &mut PpbaWorld) {
    let (path, written) = world.config_file.as_ref().expect("no config file staged");
    let now = std::fs::read(path).expect("read config file");
    assert!(
        now == *written,
        "the config file changed:\n--- written\n{}\n--- now\n{}",
        String::from_utf8_lossy(written),
        String::from_utf8_lossy(&now)
    );
}

#[then(expr = "the config file's {string} section has the name {string}")]
fn config_file_section_name(world: &mut PpbaWorld, section: String, expected: String) {
    let config = world.config_file_now();
    assert_eq!(
        config[&section]["name"],
        serde_json::json!(expected),
        "{section} section: {}",
        config[&section]
    );
}

#[then(expr = "the config file's {string} section has the description {string}")]
fn config_file_section_description(world: &mut PpbaWorld, section: String, expected: String) {
    let config = world.config_file_now();
    assert_eq!(
        config[&section]["description"],
        serde_json::json!(expected),
        "{section} section: {}",
        config[&section]
    );
}

#[then(expr = "the config file's {string} section has a minted UUIDv4 unique_id")]
fn config_file_section_uuid(world: &mut PpbaWorld, section: String) {
    let config = world.config_file_now();
    let id = config[&section]["unique_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{section} section has no string unique_id: {config}"));
    assert!(is_uuid_v4(id), "{section}.unique_id {id:?} is not a UUIDv4");
}
