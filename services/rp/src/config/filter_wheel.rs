use schemars::JsonSchema;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

/// A filter's wavelength in nanometres (`filters[].wavelength_nm`).
///
/// Validated at load: a non-finite or non-positive value is rejected
/// during deserialization. Serializes transparently as the inner `f64`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "f64")]
pub struct WavelengthNm(f64);

impl WavelengthNm {
    /// The single validating constructor.
    ///
    /// # Errors
    ///
    /// Returns a message naming the field if `value` is non-finite or
    /// not positive.
    pub fn try_new(value: f64) -> Result<Self, String> {
        if !value.is_finite() || value <= 0.0 {
            return Err(format!(
                "wavelength_nm must be a positive finite number, got {value}"
            ));
        }
        Ok(Self(value))
    }

    /// The wavelength in nanometres.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for WavelengthNm {
    type Error = String;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

/// The object form of a `filters[]` entry: a filter whose wavelength
/// is known.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilterDetail {
    pub name: String,
    pub wavelength_nm: WavelengthNm,
}

/// One `filter_wheels[].filters[]` entry.
///
/// A bare name, or `{name, wavelength_nm}` for a filter whose
/// wavelength is known (rp.md § Train optics). Serializes back in the
/// form it was written, so a config round-trips through `GET` /
/// `PUT /api/config` unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum FilterEntry {
    Name(String),
    Detailed(FilterDetail),
}

impl FilterEntry {
    /// The slot's name — what every name-resolving tool sees.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Name(name) => name,
            Self::Detailed(detail) => &detail.name,
        }
    }

    /// The slot's wavelength in nanometres, when the entry carries one.
    #[must_use]
    pub const fn wavelength_nm(&self) -> Option<f64> {
        match self {
            Self::Name(_) => None,
            Self::Detailed(detail) => Some(detail.wavelength_nm.value()),
        }
    }
}

impl From<&str> for FilterEntry {
    fn from(name: &str) -> Self {
        Self::Name(name.to_string())
    }
}

// Hand-written so the object form's own messages reach the operator:
// serde's untagged dispatch answers "data did not match any variant",
// which would hide `wavelength_nm must be a positive finite number` and
// the unknown-field name behind a shrug.
impl<'de> Deserialize<'de> for FilterEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(name) => Ok(Self::Name(name)),
            serde_json::Value::Object(_) => serde_json::from_value::<FilterDetail>(value)
                .map(Self::Detailed)
                .map_err(D::Error::custom),
            other => Err(D::Error::custom(format!(
                "a filters entry is a name or {{\"name\", \"wavelength_nm\"}}, got {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilterWheelConfig {
    pub id: String,
    pub alpaca_url: String,
    #[serde(default)]
    pub device_number: u32,
    /// The slot names in position order, each optionally with its
    /// wavelength ([`FilterEntry`]).
    #[serde(default)]
    pub filters: Vec<FilterEntry>,
    /// Optional HTTP Basic Auth credentials for connecting to auth-enabled Alpaca services
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<rp_auth::config::ClientAuthConfig>,
}

impl FilterWheelConfig {
    /// The slot names in position order — the list `set_filter`
    /// resolves a name against and `get_train_info` reports as
    /// `filters`.
    #[must_use]
    pub fn filter_names(&self) -> Vec<String> {
        self.filters
            .iter()
            .map(|entry| entry.name().to_string())
            .collect()
    }

    /// The configured name of slot `position`, if the config names it.
    #[must_use]
    pub fn filter_name_at(&self, position: usize) -> Option<&str> {
        self.filters.get(position).map(FilterEntry::name)
    }

    /// The slot a configured name sits in.
    #[must_use]
    pub fn filter_position(&self, name: &str) -> Option<usize> {
        self.filters.iter().position(|entry| entry.name() == name)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::load_config;

    fn wheel_with_filters(filters: &str) -> Result<crate::config::Config, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            format!(
                r#"{{
                "session": {{"data_directory": "/tmp/rp-test"}},
                "equipment": {{
                    "filter_wheels": [
                        {{
                            "id": "main-fw",
                            "alpaca_url": "http://localhost:11123",
                            "filters": {filters}
                        }}
                    ]
                }},
                "server": {{ "port": 0 }}
            }}"#
            ),
        )
        .unwrap();
        load_config(&path).map_err(|e| e.to_string())
    }

    #[test]
    fn filter_entries_mix_names_and_wavelength_objects() {
        let config =
            wheel_with_filters(r#"["Luminance", {"name": "Ha", "wavelength_nm": 656.28}]"#)
                .unwrap();
        let wheel = &config.equipment.filter_wheels[0];
        assert_eq!(wheel.filter_names(), ["Luminance", "Ha"]);
        assert_eq!(wheel.filters[0].wavelength_nm(), None);
        assert_eq!(wheel.filters[1].wavelength_nm(), Some(656.28));
        assert_eq!(wheel.filter_name_at(1), Some("Ha"));
        assert_eq!(wheel.filter_name_at(2), None);
        assert_eq!(wheel.filter_position("Ha"), Some(1));
        assert_eq!(wheel.filter_position("OIII"), None);
    }

    #[test]
    fn filter_entries_serialize_back_in_the_form_they_were_written() {
        let config =
            wheel_with_filters(r#"["Luminance", {"name": "Ha", "wavelength_nm": 656.0}]"#).unwrap();
        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(
            value.pointer("/equipment/filter_wheels/0/filters").unwrap(),
            &serde_json::json!(["Luminance", {"name": "Ha", "wavelength_nm": 656.0}])
        );
    }

    #[test]
    fn a_non_positive_wavelength_is_rejected_with_the_field_named() {
        let err = wheel_with_filters(r#"[{"name": "Ha", "wavelength_nm": 0}]"#).unwrap_err();
        assert!(
            err.contains("wavelength_nm must be a positive finite number"),
            "{err}"
        );
    }

    #[test]
    fn an_unknown_key_in_a_filter_object_is_rejected_by_name() {
        let err = wheel_with_filters(r#"[{"name": "Ha", "bandwidth_nm": 3}]"#).unwrap_err();
        assert!(err.contains("unknown field `bandwidth_nm`"), "{err}");
    }

    #[test]
    fn a_filter_entry_of_another_shape_is_rejected() {
        let err = wheel_with_filters("[7]").unwrap_err();
        assert!(err.contains("a filters entry is a name or"), "{err}");
    }

    #[test]
    fn wavelength_newtype_validation_boundaries() {
        assert!(WavelengthNm::try_new(656.0).is_ok());
        assert!(WavelengthNm::try_new(0.0).is_err());
        assert!(WavelengthNm::try_new(-1.0).is_err());
        assert!(WavelengthNm::try_new(f64::NAN).is_err());
        assert!(WavelengthNm::try_new(f64::INFINITY).is_err());
        assert_eq!(FilterEntry::from("Lum").name(), "Lum");
    }

    #[test]
    fn filter_wheel_config_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {
                    "filter_wheels": [
                        {
                            "id": "main-fw",
                            "alpaca_url": "http://localhost:11123",
                            "positions": 8
                        }
                    ]
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("positions"), "{err}");
    }
}
