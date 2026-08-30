use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DomeConfig {
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
    fn dome_config_minimal_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "domes": [
                        {"id": "roll-off", "alpaca_url": "http://127.0.0.1:11140"}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let d = &config.equipment.domes[0];
        assert_eq!(d.id, "roll-off");
        assert_eq!(d.alpaca_url, "http://127.0.0.1:11140");
        assert_eq!(d.device_number, 0);
        assert!(d.name.is_none());
        assert!(d.auth.is_none());
    }

    #[test]
    fn dome_config_name_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "domes": [
                        {"id": "roll-off", "name": "Roll-off Roof", "alpaca_url": "http://127.0.0.1:11140", "device_number": 1}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let d = &config.equipment.domes[0];
        assert_eq!(d.name.as_deref(), Some("Roll-off Roof"));
        assert_eq!(d.device_number, 1);
    }

    #[test]
    fn dome_config_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "domes": [
                        {"id": "roll-off", "alpaca_url": "http://127.0.0.1:11140", "device_type": "dome"}
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
