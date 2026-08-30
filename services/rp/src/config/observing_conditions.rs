use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservingConditionsConfig {
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
    fn observing_conditions_config_minimal_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "observing_conditions": [
                        {"id": "ppba-weather", "alpaca_url": "http://127.0.0.1:11112"}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let oc = &config.equipment.observing_conditions[0];
        assert_eq!(oc.id, "ppba-weather");
        assert_eq!(oc.alpaca_url, "http://127.0.0.1:11112");
        assert_eq!(oc.device_number, 0);
        assert!(oc.name.is_none());
        assert!(oc.auth.is_none());
    }

    #[test]
    fn observing_conditions_config_name_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "observing_conditions": [
                        {"id": "ppba-weather", "name": "PPBA Weather", "alpaca_url": "http://127.0.0.1:11112", "device_number": 1}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let oc = &config.equipment.observing_conditions[0];
        assert_eq!(oc.name.as_deref(), Some("PPBA Weather"));
        assert_eq!(oc.device_number, 1);
    }

    #[test]
    fn observing_conditions_config_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "observing_conditions": [
                        {"id": "ppba-weather", "alpaca_url": "http://127.0.0.1:11112", "device_type": "observingconditions"}
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
