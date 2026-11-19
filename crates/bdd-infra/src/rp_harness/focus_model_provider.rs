//! The real `focus-model` tool provider, started beside rp for suites whose
//! workflow documents call `focus_train`.
//!
//! session-runner checks every `tool` node of a document against rp's tool
//! catalog when a run starts, so a document that names `focus_train` needs
//! the provider registered even on paths that never call it. The provider is
//! started first, on a config naming rp's MCP URL at a port reserved in
//! advance, because rp dials it at startup and must find it answering.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use super::config::STARLESS_THRESHOLD_SIGMA;
use super::launcher::write_temp_config_file;
use super::scratch::scratch_dir;
use crate::ServiceHandle;

/// The name rp knows the provider by.
pub const FOCUS_MODEL_PROVIDER_NAME: &str = "focus-model";

/// The provider's tools: none is gated, as focus-model.md § Registration
/// in `rp` gives the registration.
pub const FOCUS_MODEL_TOOLS: [&str; 8] = [
    "focus_train",
    "determine_filter_offsets",
    "calibrate_temperature",
    "get_sweep_plan",
    "get_focus_model",
    "get_focus_runs",
    "set_focus_offsets",
    "reset_focus_model",
];

/// The rp tools the provider calls — the registration's `requires_tools`.
pub const FOCUS_MODEL_REQUIRED_RP_TOOLS: [&str; 15] = [
    "get_train_info",
    "get_refocus_plan",
    "get_focuser_position",
    "get_focuser_temperature",
    "move_focuser",
    "get_filter",
    "set_filter",
    "capture",
    "measure_stars",
    "get_guiding_stats",
    "pause_guiding",
    "resume_guiding",
    "auto_focus",
    "acquire_train_lease",
    "release_train_lease",
];

static STORE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A `trains.<id>` block whose sweeps read every simulator frame as
/// starless.
///
/// Short exposures, a detection threshold no frame clears (testing.md
/// §5.13), a configured grid so the simulator's incomplete optics need
/// not size it, and one attempt, since a retry of a sweep that cannot
/// fit only doubles its time.
#[must_use]
pub fn starless_focus_train() -> Value {
    serde_json::json!({
        "duration": "100ms",
        "min_area": 5,
        "max_area": 65536,
        "threshold_sigma": STARLESS_THRESHOLD_SIGMA,
        "step_size": 100,
        "half_width": 200,
        "max_attempts": 1
    })
}

/// A running provider and the `plugins[]` entry that registers it with rp.
pub struct FocusModelProvider {
    pub handle: ServiceHandle,
    pub registration: Value,
}

impl FocusModelProvider {
    /// Start focus-model with `trains` as its `trains` map, dialling rp at
    /// `rp_mcp_url`, and build its registration: every tool ungated, and
    /// `focus_train` declared a focus tool so rp brackets it with the focus
    /// events. Each start gets a store of its own in the harness scratch
    /// directory.
    pub async fn start(rp_mcp_url: &str, trains: Value) -> Self {
        let seq = STORE_SEQ.fetch_add(1, Ordering::Relaxed);
        let store_path = scratch_dir().join(format!("focus-model-{seq}.redb"));
        let config = serde_json::json!({
            "mcp_server_url": rp_mcp_url,
            "store_path": store_path.to_string_lossy(),
            "server": { "port": 0, "bind_address": "127.0.0.1" },
            "trains": trains,
        });
        let config_path = write_temp_config_file("focus-model-config", &config).await;
        let handle = ServiceHandle::start("focus-model", &config_path).await;
        let registration = registration(&format!("{}/mcp", handle.base_url()));
        Self {
            handle,
            registration,
        }
    }
}

/// The `plugins[]` entry registering a provider served at `mcp_server_url`.
fn registration(mcp_server_url: &str) -> Value {
    let gate: serde_json::Map<String, Value> = FOCUS_MODEL_TOOLS
        .iter()
        .map(|tool| ((*tool).to_string(), Value::String("none".to_string())))
        .collect();
    serde_json::json!({
        "name": FOCUS_MODEL_PROVIDER_NAME,
        "type": "tool_provider",
        "mcp_server_url": mcp_server_url,
        "gate": gate,
        "focus_tools": { "focus_train": "train_id" },
        "requires_tools": FOCUS_MODEL_REQUIRED_RP_TOOLS,
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_registration_ungates_every_tool_and_brackets_focus_train() {
        let reg = registration("http://127.0.0.1:1/mcp");
        assert_eq!(reg["name"], "focus-model");
        assert_eq!(reg["type"], "tool_provider");
        assert_eq!(reg["mcp_server_url"], "http://127.0.0.1:1/mcp");
        assert_eq!(reg["focus_tools"]["focus_train"], "train_id");
        let gate = reg["gate"].as_object().unwrap();
        assert_eq!(gate.len(), FOCUS_MODEL_TOOLS.len());
        assert!(gate.values().all(|v| v == "none"), "{gate:?}");
        assert_eq!(
            reg["requires_tools"].as_array().unwrap().len(),
            FOCUS_MODEL_REQUIRED_RP_TOOLS.len()
        );
    }

    #[test]
    fn the_starless_train_configures_its_grid_and_one_attempt() {
        let train = starless_focus_train();
        assert_eq!(train["threshold_sigma"], STARLESS_THRESHOLD_SIGMA);
        assert_eq!(train["step_size"], 100);
        assert_eq!(train["half_width"], 200);
        assert_eq!(train["max_attempts"], 1);
    }
}
