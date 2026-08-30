use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Conservative default focuser step rate: 500 steps/sec. Deliberately
/// slower than a typical EAF/Q-Focuser so the predicted move duration
/// over-estimates and the deadline won't false-abort a healthy move.
const DEFAULT_FOCUSER_STEPS_PER_SEC: f64 = 500.0;

/// Assumed focuser step rate in steps/sec, feeding the predictive
/// `move_focuser` deadline (`predicted = |target − current| / rate`).
///
/// The Alpaca `Focuser` trait exposes no step-*rate* property
/// (`MaxIncrement` / `MaxStep` are step *counts*, not rates), so this
/// config value is the rate source; set it per-rig for a tighter deadline.
///
/// Validated at load (parse-don't-validate): a non-finite or non-positive
/// rate is rejected during deserialization, so a bad config fails at
/// startup rather than at move time.
/// Serializes transparently as the inner `f64` (and the JSON Schema is a
/// plain number), matching the `try_from = "f64"` deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "f64")]
pub struct FocuserStepsPerSec(f64);

impl FocuserStepsPerSec {
    /// The single validating constructor.
    ///
    /// # Errors
    ///
    /// Returns a message naming the field if `value` is non-finite or
    /// not positive.
    pub fn try_new(value: f64) -> Result<Self, String> {
        if !value.is_finite() || value <= 0.0 {
            return Err(format!(
                "steps_per_sec must be a finite positive number, got {value}"
            ));
        }
        Ok(Self(value))
    }

    /// The rate in steps/sec.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

impl Default for FocuserStepsPerSec {
    fn default() -> Self {
        Self(DEFAULT_FOCUSER_STEPS_PER_SEC)
    }
}

impl TryFrom<f64> for FocuserStepsPerSec {
    type Error = String;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FocuserConfig {
    pub id: String,
    pub alpaca_url: String,
    #[serde(default)]
    pub device_number: u32,
    /// Operator-supplied lower bound for `move_focuser` validation. The
    /// device-reported `max_step` is the hardware ceiling; these fields
    /// let the operator enforce a tighter safe-travel range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_position: Option<i32>,
    /// Operator-supplied upper bound for `move_focuser` validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_position: Option<i32>,
    /// Assumed focuser step rate (steps/sec) used to size the predictive
    /// `move_focuser` deadline. Defaults to 500 (a conservative slow rate);
    /// set per-rig for a tighter bound.
    #[serde(default)]
    pub steps_per_sec: FocuserStepsPerSec,
    /// Optional HTTP Basic Auth credentials for connecting to auth-enabled Alpaca services
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<rp_auth::config::ClientAuthConfig>,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use crate::config::load_config;

    #[test]
    fn focuser_config_minimal_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "focusers": [
                        {
                            "id": "main-focuser",
                            "alpaca_url": "http://localhost:11113"
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.equipment.focusers.len(), 1);
        let f = &config.equipment.focusers[0];
        assert_eq!(f.id, "main-focuser");
        assert_eq!(f.alpaca_url, "http://localhost:11113");
        assert_eq!(f.device_number, 0);
        assert!(f.min_position.is_none());
        assert!(f.max_position.is_none());
        assert!(f.auth.is_none());
    }

    #[test]
    fn focuser_config_with_bounds_and_auth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "focusers": [
                        {
                            "id": "main-focuser",
                            "alpaca_url": "http://localhost:11113",
                            "device_number": 2,
                            "min_position": 0,
                            "max_position": 100000,
                            "auth": {"username": "u", "password": "p"}
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let f = &config.equipment.focusers[0];
        assert_eq!(f.device_number, 2);
        assert_eq!(f.min_position, Some(0));
        assert_eq!(f.max_position, Some(100_000));
        let auth = f.auth.as_ref().unwrap();
        assert_eq!(auth.username, "u");
        assert_eq!(auth.password, "p");
    }

    #[test]
    fn focuser_config_steps_per_sec_defaults_to_500() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "focusers": [
                        {"id": "main-focuser", "alpaca_url": "http://localhost:11113"}
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.equipment.focusers[0].steps_per_sec.value(), 500.0);
    }

    #[test]
    fn focuser_config_steps_per_sec_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "focusers": [
                        {
                            "id": "main-focuser",
                            "alpaca_url": "http://localhost:11113",
                            "steps_per_sec": 1200
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.equipment.focusers[0].steps_per_sec.value(), 1200.0);
    }

    #[test]
    fn focuser_config_steps_per_sec_rejects_non_positive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "focusers": [
                        {
                            "id": "main-focuser",
                            "alpaca_url": "http://localhost:11113",
                            "steps_per_sec": 0
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err().to_string();
        assert!(
            err.contains("steps_per_sec must be a finite positive number"),
            "expected the validation message, got: {err}"
        );
    }

    #[test]
    fn focuser_config_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "focusers": [
                        {
                            "id": "main-focuser",
                            "alpaca_url": "http://localhost:11113",
                            "backlash": 50
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("backlash"), "{err}");
    }

    #[test]
    fn focuser_steps_per_sec_newtype_validation_boundaries() {
        use super::FocuserStepsPerSec;
        assert_eq!(FocuserStepsPerSec::default().value(), 500.0);
        assert_eq!(FocuserStepsPerSec::try_new(1200.0).unwrap().value(), 1200.0);
        // The `<= 0.0` edge and the non-finite branch (unreachable from
        // JSON, defensive-only) are rejected and name the field.
        assert!(FocuserStepsPerSec::try_new(0.0)
            .unwrap_err()
            .contains("steps_per_sec"));
        assert!(FocuserStepsPerSec::try_new(-1.0).is_err());
        assert!(FocuserStepsPerSec::try_new(f64::NAN).is_err());
        assert!(FocuserStepsPerSec::try_new(f64::INFINITY).is_err());
    }
}
