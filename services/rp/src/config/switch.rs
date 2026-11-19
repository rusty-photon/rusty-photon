use rusty_photon_config::driver::FieldError;
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
    /// Optional identity pin: the Alpaca `UniqueID` the device at
    /// `device_number` must report in its server's `configureddevices`
    /// list, or the connect is refused (rp.md § Device Identity Pin).
    /// Omitted, the entry binds whatever device its server lists there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub unique_id: Option<super::UniqueIdPin>,
    /// Optional HTTP Basic Auth credentials for connecting to auth-enabled Alpaca services
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<rp_auth::config::ClientAuthConfig>,
    /// The power map: the box's outputs, the devices each one feeds,
    /// and the ones never switched off through rp (rp.md § Power
    /// Switching). An output not listed here is never switched through
    /// rp.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<OutputConfig>,
}

impl SwitchConfig {
    /// The rules one entry's power map holds by itself, with `index`
    /// naming the entry's position in `equipment.switches`: every output
    /// has a name, and no name repeats. The rules that need the rest of
    /// the roster are [`super::EquipmentConfig::power_map_errors`].
    #[must_use]
    pub fn field_errors(&self, index: usize) -> Vec<FieldError> {
        let mut errors = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (j, output) in self.outputs.iter().enumerate() {
            let path = format!("equipment.switches.{index}.outputs.{j}.name");
            if output.name.trim().is_empty() {
                errors.push(FieldError {
                    path,
                    msg: format!(
                        "must name the output as the box's driver reports it (switch '{}')",
                        self.id
                    ),
                });
            } else if !seen.insert(output.name.as_str()) {
                errors.push(FieldError {
                    path,
                    msg: format!(
                        "output '{}' is listed more than once (switch '{}')",
                        output.name, self.id
                    ),
                });
            }
        }
        errors
    }

    /// The power map's entry for the output named `name`, matched
    /// exactly.
    #[must_use]
    pub fn output(&self, name: &str) -> Option<&OutputConfig> {
        self.outputs.iter().find(|o| o.name == name)
    }
}

/// One output of a power box in rp's power map (rp.md § Power
/// Switching).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    /// The name the box's driver reports for the output
    /// (`GetSwitchName`): on rusty-photon's power box drivers the
    /// operator's label where the driver sets one, and the built-in name
    /// otherwise. Matched exactly.
    pub name: String,
    /// Roster ids of the devices the output feeds, with `mount` for the
    /// mount.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<String>,
    /// Why the output is never switched off through rp: it feeds the
    /// host or its network link, the box's own link, a safety monitor or
    /// another power box. The refusal quotes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub protected: Option<ProtectedReason>,
}

/// The reason an output is protected, as the operator wrote it.
///
/// Validated at load (parse-don't-validate): a blank reason is rejected
/// during deserialization, since the refusal quotes it and the key's
/// absence is how an output goes unprotected. Serializes transparently
/// as the inner string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProtectedReason(String);

impl ProtectedReason {
    /// The single validating constructor.
    ///
    /// # Errors
    ///
    /// Returns a message naming the field if `value` is blank.
    pub fn try_new(value: String) -> Result<Self, String> {
        if value.trim().is_empty() {
            return Err(
                "protected must say why the output is never switched off; omit the key to leave it unprotected"
                    .into(),
            );
        }
        Ok(Self(value))
    }

    /// The reason, as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ProtectedReason {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<ProtectedReason> for String {
    fn from(reason: ProtectedReason) -> Self {
        reason.0
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use crate::config::load_config;

    fn switch_with_outputs(outputs: serde_json::Value) -> super::SwitchConfig {
        serde_json::from_value(serde_json::json!({
            "id": "ppba",
            "alpaca_url": "http://127.0.0.1:11112",
            "outputs": outputs
        }))
        .unwrap()
    }

    #[test]
    fn an_output_needs_a_name_that_does_not_repeat() {
        let config = switch_with_outputs(serde_json::json!([
            {"name": "Quad 12V Output"},
            {"name": "  "},
            {"name": "Quad 12V Output", "devices": ["main-cam"]}
        ]));

        let errors = config.field_errors(3);

        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "equipment.switches.3.outputs.1.name",
                "equipment.switches.3.outputs.2.name"
            ]
        );
        assert!(
            errors[1].msg.contains("more than once"),
            "{}",
            errors[1].msg
        );
    }

    #[test]
    fn a_blank_protected_reason_fails_the_load() {
        let err = serde_json::from_value::<super::SwitchConfig>(serde_json::json!({
            "id": "ppba",
            "alpaca_url": "http://127.0.0.1:11112",
            "outputs": [{"name": "Adjustable Output", "protected": " "}]
        }))
        .unwrap_err();
        assert!(err.to_string().contains("protected must say why"), "{err}");
    }

    #[test]
    fn the_power_map_round_trips_and_an_empty_one_is_left_out() {
        let config = switch_with_outputs(serde_json::json!([
            {"name": "Adjustable Output", "protected": "the Pi runs off it"},
            {"name": "USB Hub", "devices": ["guide-cam"]}
        ]));
        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(
            value["outputs"],
            serde_json::json!([
                {"name": "Adjustable Output", "protected": "the Pi runs off it"},
                {"name": "USB Hub", "devices": ["guide-cam"]}
            ])
        );

        let bare = switch_with_outputs(serde_json::json!([]));
        assert!(serde_json::to_value(&bare)
            .unwrap()
            .get("outputs")
            .is_none());
    }

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
