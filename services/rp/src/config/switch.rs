use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SwitchConfig {
    pub id: String,
    /// Optional friendly label for the roster (falls back to `id` when
    /// absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    fn switch_config_minimal_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "switches": [
                        {"id": "ppba", "alpaca_url": "http://127.0.0.1:11112"}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let sw = &config.equipment.switches[0];
        assert_eq!(sw.id, "ppba");
        assert_eq!(sw.alpaca_url, "http://127.0.0.1:11112");
        assert_eq!(sw.device_number, 0);
        assert!(sw.name.is_none());
        assert!(sw.auth.is_none());
    }

    #[test]
    fn switch_config_name_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "switches": [
                        {"id": "ppba", "name": "Pegasus PPBA", "alpaca_url": "http://127.0.0.1:11112", "device_number": 1}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let sw = &config.equipment.switches[0];
        assert_eq!(sw.name.as_deref(), Some("Pegasus PPBA"));
        assert_eq!(sw.device_number, 1);
    }

    #[test]
    fn switch_config_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "switches": [
                        {"id": "ppba", "alpaca_url": "http://127.0.0.1:11112", "device_type": "switch"}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("device_type"), "{err}");
    }
}
