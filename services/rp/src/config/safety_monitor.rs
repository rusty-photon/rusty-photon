use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// An ASCOM Alpaca `SafetyMonitor` device gating the session (rp.md
/// § Safety). Polled at `safety.poll_interval`; a monitor that cannot
/// be read counts as unsafe.
///
/// Unknown fields are rejected — stricter than the other equipment
/// entries, deliberately: a typo in safety-critical config must fail
/// at load, not be silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SafetyMonitorConfig {
    pub id: String,
    pub alpaca_url: String,
    #[serde(default)]
    pub device_number: u32,
    /// Optional HTTP Basic Auth credentials for connecting to auth-enabled Alpaca services
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<rp_auth::config::ClientAuthConfig>,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use crate::config::load_config;

    #[test]
    fn safety_monitor_entry_parses_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "safety_monitors": [
                        {"id": "weather-watcher", "alpaca_url": "http://127.0.0.1:32323"}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let sm = &config.equipment.safety_monitors[0];
        assert_eq!(sm.id, "weather-watcher");
        assert_eq!(sm.alpaca_url, "http://127.0.0.1:32323");
        assert_eq!(sm.device_number, 0);
        assert!(sm.auth.is_none());
    }

    #[test]
    fn safety_monitor_entry_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "safety_monitors": [
                        {"id": "sm", "alpaca_url": "http://127.0.0.1:32323", "pol_interval": "1s"}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("pol_interval") || msg.contains("unknown field"),
            "expected unknown-field diagnostic, got: {msg}"
        );
    }
}
