#![allow(dead_code)]
//! Test infrastructure: gemini-powerbox-adv3-driver process management and
//! config helpers.

use std::time::Duration;

pub use bdd_infra::ServiceHandle;

// ---------------------------------------------------------------------------
// Config helpers
// ---------------------------------------------------------------------------

/// Build a default test config JSON with both devices enabled.
/// Uses port 0 for OS-assigned port and a short polling interval for fast
/// tests. No `baud_rate`: the hub has one, and the key is a load error.
pub fn default_test_config() -> serde_json::Value {
    serde_json::json!({
        "serial": {
            "port": "/dev/mock",
            "polling_interval": "200ms",
            "timeout": "2s"
        },
        "server": { "port": 0, "discovery_port": null },
        "switch": {
            "enabled": true,
            "name": "Gemini Advanced 3 Switch",
            "unique_id": "gemini-adv3-switch-001",
            "description": "Gemini Astro Power & Data Hubs Advanced 3 Power Control"
        },
        "observingconditions": {
            "enabled": true,
            "name": "Gemini Advanced 3 Weather",
            "unique_id": "gemini-adv3-observingconditions-001",
            "description": "Gemini Astro Power & Data Hubs Advanced 3 Environmental Sensors",
            "averaging_period": "5m"
        }
    })
}

/// Build a test config with only the switch device enabled.
pub fn switch_only_config() -> serde_json::Value {
    let mut config = default_test_config();
    config["observingconditions"]["enabled"] = serde_json::json!(false);
    config
}

/// Build a test config with only the OC device enabled.
pub fn oc_only_config() -> serde_json::Value {
    let mut config = default_test_config();
    config["switch"]["enabled"] = serde_json::json!(false);
    config
}

/// Build a test config with both devices disabled.
pub fn both_disabled_config() -> serde_json::Value {
    let mut config = default_test_config();
    config["switch"]["enabled"] = serde_json::json!(false);
    config["observingconditions"]["enabled"] = serde_json::json!(false);
    config
}

// ---------------------------------------------------------------------------
// A start that is expected to be refused
// ---------------------------------------------------------------------------

/// How a service start the scenario expected to fail ended.
#[derive(Debug, Clone)]
pub struct StartOutcome {
    /// Whether the process exited at all inside the deadline.
    pub exited: bool,
    /// Whether it exited with status 0.
    pub success: bool,
    /// Everything it wrote to stderr, where the refusal is reported.
    pub stderr: String,
}

/// A start that has not exited is given this long before it is killed and
/// reported as not having exited: a refusal takes at most the handshake's
/// one 2 s identity retry.
const EXIT_DEADLINE: Duration = Duration::from_secs(30);

/// The service binary: what Bazel hands the test, or what Cargo built next
/// to it. The same discovery `bdd_infra::ServiceHandle` uses, written out
/// because it is private there.
fn binary() -> String {
    std::env::var("GEMINI_POWERBOX_ADV3_DRIVER_BINARY")
        .ok()
        .or_else(|| option_env!("CARGO_BIN_EXE_gemini-powerbox-adv3-driver").map(String::from))
        .expect(
            "GEMINI_POWERBOX_ADV3_DRIVER_BINARY (Bazel) or \
             CARGO_BIN_EXE_gemini-powerbox-adv3-driver (cargo) must be set",
        )
}

/// Write `config` to a fresh temp file and return its path.
pub async fn write_config(prefix: &str, config: &serde_json::Value) -> String {
    let path = std::env::temp_dir().join(format!(
        "{prefix}-{}.json",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_nanos()
    ));
    tokio::fs::write(
        &path,
        serde_json::to_string_pretty(config).expect("config is JSON"),
    )
    .await
    .expect("failed to write test config");
    path.to_str().expect("temp path is UTF-8").to_string()
}

/// Start the service with `config` and `envs` and wait for it to exit,
/// collecting its stderr. For the scenarios in which the handshake refuses
/// the hub: the service then exits before it binds, which
/// [`ServiceHandle`] reports as a failure without the words the operator
/// was given. A service that is still running at the deadline is killed.
pub async fn run_to_exit(config: &serde_json::Value, envs: &[(&str, &str)]) -> StartOutcome {
    let config_path = write_config("gemini-bdd-refused", config).await;
    let mut command = tokio::process::Command::new(binary());
    command
        .arg("--config")
        .arg(&config_path)
        .envs(envs.iter().copied())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().expect("failed to spawn the service binary");
    match tokio::time::timeout(EXIT_DEADLINE, child.wait_with_output()).await {
        Ok(Ok(output)) => StartOutcome {
            exited: true,
            success: output.status.success(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Ok(Err(error)) => panic!("waiting for the service binary failed: {error}"),
        Err(_) => StartOutcome {
            exited: false,
            success: false,
            stderr: format!("the service did not exit within {EXIT_DEADLINE:?}"),
        },
    }
}
