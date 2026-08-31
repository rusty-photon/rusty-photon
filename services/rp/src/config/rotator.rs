use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RotatorConfig {
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
    fn rotator_config_minimal_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "rotators": [
                        {"id": "falcon", "alpaca_url": "http://127.0.0.1:11118"}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let r = &config.equipment.rotators[0];
        assert_eq!(r.id, "falcon");
        assert_eq!(r.alpaca_url, "http://127.0.0.1:11118");
        assert_eq!(r.device_number, 0);
        assert!(r.name.is_none());
        assert!(r.auth.is_none());
    }

    #[test]
    fn rotator_config_name_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "rotators": [
                        {"id": "falcon", "name": "Falcon Rotator", "alpaca_url": "http://127.0.0.1:11118", "device_number": 1}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let r = &config.equipment.rotators[0];
        assert_eq!(r.name.as_deref(), Some("Falcon Rotator"));
        assert_eq!(r.device_number, 1);
    }

    #[test]
    fn rotator_config_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "rotators": [
                        {"id": "falcon", "alpaca_url": "http://127.0.0.1:11118", "device_type": "rotator"}
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
