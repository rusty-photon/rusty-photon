#![allow(dead_code)]
//! Test infrastructure: upbv2-driver process management and config helpers.

pub use bdd_infra::ServiceHandle;

// ---------------------------------------------------------------------------
// Config helpers
// ---------------------------------------------------------------------------

/// Build a default test config JSON with both devices enabled.
/// Uses port 0 for OS-assigned port and a short polling interval for fast tests.
pub fn default_test_config() -> serde_json::Value {
    serde_json::json!({
        "serial": {
            "port": "/dev/mock",
            "baud_rate": 9600,
            "polling_interval": "200ms",
            "timeout": "2s"
        },
        "server": { "port": 0, "discovery_port": null },
        "switch": {
            "enabled": true,
            "name": "Pegasus UPBv2 Switch",
            "unique_id": "upbv2-switch-001",
            "description": "Pegasus Astro Ultimate Powerbox v2 Power Control"
        },
        "observingconditions": {
            "enabled": true,
            "name": "Pegasus UPBv2 Weather",
            "unique_id": "upbv2-observingconditions-001",
            "description": "Pegasus Astro Ultimate Powerbox v2 Environmental Sensors",
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
