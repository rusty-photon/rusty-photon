use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::camera::CameraConfig;
use super::cover_calibrator::CoverCalibratorConfig;
use super::dome::DomeConfig;
use super::filter_wheel::FilterWheelConfig;
use super::focuser::FocuserConfig;
use super::mount::MountConfig;
use super::observing_conditions::ObservingConditionsConfig;
use super::optical_train::OpticalTrainConfig;
use super::rotator::RotatorConfig;
use super::safety_monitor::SafetyMonitorConfig;
use super::switch::SwitchConfig;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EquipmentConfig {
    /// Cadence of the reconnect supervisor's per-device session health
    /// checks (rp.md § Device Session Recovery; default `"30s"`). Dead
    /// sessions — a downstream service restarted, or a device that was
    /// unreachable at startup — are re-established at this interval.
    #[serde(default = "default_reconnect_interval", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub reconnect_interval: Duration,
    #[serde(default)]
    pub cameras: Vec<CameraConfig>,
    /// Optical trains (rp.md § Optical Trains): ordered roster
    /// device-id lists, objective side first, terminating in a camera.
    /// The cross-array graph rules are validated by
    /// `crate::equipment::trains::TrainModel::try_from_equipment`.
    #[serde(default)]
    pub optical_trains: Vec<OpticalTrainConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount: Option<MountConfig>,
    #[serde(default)]
    pub focusers: Vec<FocuserConfig>,
    #[serde(default)]
    pub filter_wheels: Vec<FilterWheelConfig>,
    #[serde(default)]
    pub cover_calibrators: Vec<CoverCalibratorConfig>,
    #[serde(default)]
    pub safety_monitors: Vec<SafetyMonitorConfig>,
    #[serde(default)]
    pub switches: Vec<SwitchConfig>,
    #[serde(default)]
    pub rotators: Vec<RotatorConfig>,
    #[serde(default)]
    pub observing_conditions: Vec<ObservingConditionsConfig>,
    #[serde(default)]
    pub domes: Vec<DomeConfig>,
}

impl Default for EquipmentConfig {
    fn default() -> Self {
        Self {
            reconnect_interval: default_reconnect_interval(),
            cameras: Vec::new(),
            optical_trains: Vec::new(),
            mount: None,
            focusers: Vec::new(),
            filter_wheels: Vec::new(),
            cover_calibrators: Vec::new(),
            safety_monitors: Vec::new(),
            switches: Vec::new(),
            rotators: Vec::new(),
            observing_conditions: Vec::new(),
            domes: Vec::new(),
        }
    }
}

const fn default_reconnect_interval() -> Duration {
    Duration::from_secs(30)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use crate::config::load_config;
    use crate::config::test_support::MINIMAL_CONFIG_JSON;

    #[test]
    fn equipment_config_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {"rotator": {}},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("rotator"), "{err}");
    }

    #[test]
    fn reconnect_interval_omitted_applies_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, MINIMAL_CONFIG_JSON).unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.equipment.reconnect_interval, Duration::from_secs(30));
    }

    #[test]
    fn reconnect_interval_parses_humantime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {"reconnect_interval": "500ms"},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(
            config.equipment.reconnect_interval,
            Duration::from_millis(500)
        );
    }
}
