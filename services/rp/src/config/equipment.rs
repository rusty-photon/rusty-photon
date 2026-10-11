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

/// Default `temperature_event_delta_c`: half a degree is well above
/// probe noise on every focuser hub seen so far and well under the
/// drift that moves focus on a typical refractor.
const DEFAULT_TEMPERATURE_EVENT_DELTA_C: f64 = 0.5;

/// The drift, in °C since the last emission, at which the Focuser
/// Temperature Watch emits `temperature_changed`
/// (`equipment.temperature_event_delta_c`, rp.md § Focuser Temperature
/// Watch).
///
/// Validated at load (parse-don't-validate): a zero, negative or
/// non-finite delta is rejected during deserialization — zero would
/// emit on every jitter of the probe — so a bad config fails at
/// startup rather than flooding the event stream all night. Serializes
/// transparently as the inner `f64`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "f64")]
pub struct TemperatureEventDeltaC(f64);

impl TemperatureEventDeltaC {
    /// The single validating constructor.
    ///
    /// # Errors
    ///
    /// Returns a message naming the field if `value` is non-finite or
    /// not positive.
    pub fn try_new(value: f64) -> Result<Self, String> {
        if !value.is_finite() || value <= 0.0 {
            return Err(format!(
                "equipment.temperature_event_delta_c must be a finite positive number, got {value}"
            ));
        }
        Ok(Self(value))
    }

    /// The delta in °C.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

impl Default for TemperatureEventDeltaC {
    fn default() -> Self {
        Self(DEFAULT_TEMPERATURE_EVENT_DELTA_C)
    }
}

impl TryFrom<f64> for TemperatureEventDeltaC {
    type Error = String;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

/// An equipment entry's identity pin (rp.md § Device Identity Pin).
///
/// The Alpaca `UniqueID` the device at the entry's `device_number` must
/// report in its server's `configureddevices` list, or the connect is
/// refused.
///
/// Validated at load (parse-don't-validate): an empty value, or one with
/// leading or trailing whitespace, is rejected during deserialization.
/// The pin is compared verbatim with what the server lists, so a padded
/// value could never match, and rejecting it beats trimming a value the
/// operator did not write. Serializes transparently as the inner string;
/// each field carrying one declares its JSON Schema as a plain optional
/// string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UniqueIdPin(String);

impl UniqueIdPin {
    /// The single validating constructor.
    ///
    /// # Errors
    ///
    /// Returns a message naming the field if `value` is empty or has
    /// leading or trailing whitespace.
    pub fn try_new(value: String) -> Result<Self, String> {
        if value.is_empty() {
            return Err(
                "unique_id must not be empty; omit the key to leave the entry unpinned".into(),
            );
        }
        if value.trim() != value {
            return Err(format!(
                "unique_id must not have leading or trailing whitespace, got {value:?}"
            ));
        }
        Ok(Self(value))
    }

    /// The pinned `UniqueID`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for UniqueIdPin {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<UniqueIdPin> for String {
    fn from(pin: UniqueIdPin) -> Self {
        pin.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EquipmentConfig {
    /// Cadence of the reconnect supervisor's per-device session health
    /// checks (rp.md § Device Session Recovery; default `"30s"`). Dead
    /// sessions — a downstream service restarted, or a device that was
    /// unreachable at startup — are re-established at this interval.
    /// Must be greater than zero: a zero interval would turn the
    /// supervisor into a busy loop, so it is rejected at config load
    /// (parse-don't-validate).
    #[serde(
        default = "default_reconnect_interval",
        deserialize_with = "deserialize_reconnect_interval",
        serialize_with = "humantime_serde::serialize"
    )]
    #[schemars(with = "String")]
    pub reconnect_interval: Duration,
    /// Cadence of the Focuser Temperature Watch (rp.md § Focuser
    /// Temperature Watch; default `"30s"`): every interval, each
    /// connected focuser's probe is read once. Tens of seconds is
    /// plenty — a probe drifts a fraction of a degree per minute at
    /// most, and one read per focuser per interval is the whole cost.
    /// A drift is noticed within one interval plus one read timeout
    /// (5 s; the reads of a pass run concurrently, so that bound holds
    /// however many focusers are configured). Must be greater than
    /// zero (a busy loop otherwise), rejected at config load.
    #[serde(
        default = "default_temperature_poll_interval",
        deserialize_with = "deserialize_temperature_poll_interval",
        serialize_with = "humantime_serde::serialize"
    )]
    #[schemars(with = "String")]
    pub temperature_poll_interval: Duration,
    /// The drift since the last emission at which the watch emits
    /// `temperature_changed` (default `0.5` °C).
    #[serde(default)]
    pub temperature_event_delta_c: TemperatureEventDeltaC,
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
            temperature_poll_interval: default_temperature_poll_interval(),
            temperature_event_delta_c: TemperatureEventDeltaC::default(),
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

fn deserialize_reconnect_interval<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_positive_interval(deserializer, "equipment.reconnect_interval")
}

const fn default_temperature_poll_interval() -> Duration {
    Duration::from_secs(30)
}

fn deserialize_temperature_poll_interval<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_positive_interval(deserializer, "equipment.temperature_poll_interval")
}

/// A humantime interval that must be greater than zero — both cadences
/// here are loops, and a zero interval would make either a busy loop.
fn deserialize_positive_interval<'de, D>(
    deserializer: D,
    field: &'static str,
) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let interval: Duration = humantime_serde::deserialize(deserializer)?;
    if interval.is_zero() {
        return Err(serde::de::Error::custom(format!(
            "{field} must be greater than zero"
        )));
    }
    Ok(interval)
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

    /// A zero interval would turn the supervisor into a busy loop, so
    /// the config loader rejects it with the field named.
    #[test]
    fn reconnect_interval_rejects_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {"reconnect_interval": "0s"},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err().to_string();
        assert!(
            err.contains("reconnect_interval must be greater than zero"),
            "{err}"
        );
    }

    #[test]
    fn temperature_watch_knobs_omitted_apply_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, MINIMAL_CONFIG_JSON).unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(
            config.equipment.temperature_poll_interval,
            Duration::from_secs(30)
        );
        assert_eq!(config.equipment.temperature_event_delta_c.value(), 0.5);
    }

    #[test]
    fn temperature_watch_knobs_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "temperature_poll_interval": "200ms",
                    "temperature_event_delta_c": 1.5
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(
            config.equipment.temperature_poll_interval,
            Duration::from_millis(200)
        );
        assert_eq!(config.equipment.temperature_event_delta_c.value(), 1.5);
    }

    /// A zero interval would be a busy loop, so the loader rejects it
    /// with the field named — the same rule as `reconnect_interval`.
    #[test]
    fn temperature_poll_interval_rejects_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {"temperature_poll_interval": "0s"},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err().to_string();
        assert!(
            err.contains("temperature_poll_interval must be greater than zero"),
            "{err}"
        );
    }

    /// A zero delta would emit on every jitter of the probe; a negative
    /// one could never be reached. Both fail at load, naming the field.
    #[test]
    fn temperature_event_delta_rejects_non_positive_values() {
        for bad in ["0", "-0.5", "1e400"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.json");
            std::fs::write(
                &path,
                format!(
                    r#"{{
                        "session": {{"data_directory": "/tmp/rp-test"}},
                        "equipment": {{"temperature_event_delta_c": {bad}}},
                        "server": {{ "port": 0 }}
                    }}"#
                ),
            )
            .unwrap();

            let err = load_config(&path).unwrap_err().to_string();
            assert!(
                err.contains("temperature_event_delta_c must be a finite positive number")
                    || err.contains("number out of range"),
                "delta {bad}: {err}"
            );
        }
        assert!(super::TemperatureEventDeltaC::try_new(f64::NAN).is_err());
        assert!(super::TemperatureEventDeltaC::try_new(f64::INFINITY).is_err());
    }

    fn config_with_equipment(equipment: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            format!(
                r#"{{
                    "session": {{"data_directory": "/tmp/rp-test"}},
                    "equipment": {equipment},
                    "server": {{ "port": 0 }}
                }}"#
            ),
        )
        .unwrap();
        (dir, path)
    }

    /// The pin is accepted on an array kind and on the singular mount,
    /// and is held verbatim.
    #[test]
    fn unique_id_pin_loads_on_array_entries_and_the_mount() {
        let (_dir, path) = config_with_equipment(
            r#"{
                "cameras": [{"id": "qhy600m", "alpaca_url": "http://127.0.0.1:11121",
                             "device_number": 1, "unique_id": "QHY600M-a1b2 c3"}],
                "mount": {"alpaca_url": "http://127.0.0.1:11117",
                          "unique_id": "GTi-mount-1"}
            }"#,
        );
        let config = load_config(&path).unwrap();
        let camera_pin = config.equipment.cameras[0].unique_id.as_ref().unwrap();
        assert_eq!(camera_pin.as_str(), "QHY600M-a1b2 c3");
        let mount_pin = config.equipment.mount.unwrap().unique_id.unwrap();
        assert_eq!(mount_pin.as_str(), "GTi-mount-1");
    }

    #[test]
    fn an_absent_unique_id_leaves_the_entry_unpinned() {
        let (_dir, path) = config_with_equipment(
            r#"{"switches": [{"id": "ppba", "alpaca_url": "http://127.0.0.1:11112"}]}"#,
        );
        let config = load_config(&path).unwrap();
        assert!(config.equipment.switches[0].unique_id.is_none());
    }

    /// An empty or padded pin could never match a listed `UniqueID`, so
    /// it fails the load instead of refusing the device every night.
    #[test]
    fn unique_id_pin_rejects_empty_and_padded_values() {
        for (bad, needle) in [
            (r#""""#, "unique_id must not be empty"),
            (r#"" QHY600M-a1b2""#, "leading or trailing whitespace"),
            (r#""QHY600M-a1b2\n""#, "leading or trailing whitespace"),
        ] {
            let (_dir, path) = config_with_equipment(&format!(
                r#"{{"focusers": [{{"id": "f", "alpaca_url": "http://127.0.0.1:11113",
                                    "unique_id": {bad}}}]}}"#
            ));
            let err = load_config(&path).unwrap_err().to_string();
            assert!(err.contains(needle), "pin {bad}: {err}");
        }
    }

    /// A pin serializes back as the plain string it was loaded from, so
    /// `PUT /api/config` persists what the operator wrote.
    #[test]
    fn unique_id_pin_round_trips_as_a_plain_string() {
        let pin = super::UniqueIdPin::try_new("ZWO:ASI1600MM:noserial-0".to_string()).unwrap();
        assert_eq!(
            serde_json::to_value(&pin).unwrap(),
            serde_json::json!("ZWO:ASI1600MM:noserial-0")
        );
    }

    /// The schema advertises the pin as an optional plain string, the
    /// shape the web UI's schema walker renders as a text input
    /// (docs/services/ui-htmx.md § Schema-driven rendering).
    #[test]
    fn unique_id_schema_is_an_optional_string() {
        let schema = schemars::schema_for!(crate::config::DomeConfig);
        let value = serde_json::to_value(&schema).unwrap();
        let field = value
            .pointer("/properties/unique_id")
            .expect("schema must carry the unique_id property");
        assert_eq!(
            field.pointer("/type").unwrap(),
            &serde_json::json!(["string", "null"])
        );
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
