use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoverCalibratorConfig {
    pub id: String,
    pub alpaca_url: String,
    #[serde(default)]
    pub device_number: u32,
    /// Poll interval when waiting for cover/calibrator state changes (default `"3s"`)
    #[serde(
        default = "default_cover_calibrator_poll_interval",
        with = "humantime_serde"
    )]
    #[schemars(with = "String")]
    pub poll_interval: Duration,
    /// Optional HTTP Basic Auth credentials for connecting to auth-enabled Alpaca services
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<rp_auth::config::ClientAuthConfig>,
}

const fn default_cover_calibrator_poll_interval() -> Duration {
    Duration::from_secs(3)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use crate::config::load_config;

    #[test]
    fn cover_calibrator_config_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "cover_calibrators": [
                        {
                            "id": "flat-panel",
                            "alpaca_url": "http://localhost:11125",
                            "brightness": 100
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("brightness"), "{err}");
    }
}
