use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Safety-enforcement settings (rp.md § Safety). The monitors themselves
/// are equipment (`equipment.safety_monitors`); this block holds the
/// enforcement knobs shared across them.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SafetyConfig {
    /// How often every configured `SafetyMonitor` is polled (default `"10s"`).
    #[serde(default = "default_safety_poll_interval", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub poll_interval: Duration,
    /// Operator overrides to the built-in tool classes (rp.md § Safety →
    /// In-Flight Tool Calls → Operator overrides). Both lists default
    /// to empty; every name must exist in the tool catalog and no name
    /// may appear on both sides — checked at load and by
    /// `PUT /api/config` (`crate::mcp::gate::override_errors`).
    #[serde(default)]
    pub gate: GateOverrides,
}

/// The `safety.gate` block: tools to move across the safety gate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GateOverrides {
    /// Tools to treat as gated (refused with `SafetyUnsafe` while
    /// unsafe, cancelled on the unsafe transition) even though the
    /// built-in table has them ungated — e.g. `auto_focus` on a rig
    /// with a fragile focuser.
    #[serde(default)]
    pub gated: Vec<String>,
    /// Tools to treat as ungated even though the built-in table has
    /// them gated — e.g. `open_cover` under a sealed dome.
    #[serde(default)]
    pub ungated: Vec<String>,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            poll_interval: default_safety_poll_interval(),
            gate: GateOverrides::default(),
        }
    }
}

const fn default_safety_poll_interval() -> Duration {
    Duration::from_secs(10)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use crate::config::test_support::MINIMAL_CONFIG_JSON;
    use crate::config::{load_config, validate_config, Config, GateOverrides};

    fn write_and_load(json: &str) -> Result<Config, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, json).unwrap();
        load_config(&path).map_err(|e| e.to_string())
    }

    #[test]
    fn safety_block_omitted_applies_default_poll_interval() {
        let config = write_and_load(MINIMAL_CONFIG_JSON).unwrap();
        assert_eq!(config.safety.poll_interval, Duration::from_secs(10));
        assert_eq!(config.safety.gate, GateOverrides::default());
    }

    #[test]
    fn safety_poll_interval_parses_humantime() {
        let config = write_and_load(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "safety": {"poll_interval": "250ms"},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();
        assert_eq!(config.safety.poll_interval, Duration::from_millis(250));
    }

    #[test]
    fn safety_block_rejects_unknown_field() {
        let msg = write_and_load(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "safety": {"park_on_unsafe": true},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap_err();
        assert!(
            msg.contains("park_on_unsafe") || msg.contains("unknown field"),
            "expected unknown-field diagnostic, got: {msg}"
        );
    }

    #[test]
    fn safety_gate_overrides_parse_into_both_lists() {
        let config = write_and_load(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "safety": {"gate": {"gated": ["auto_focus"], "ungated": ["open_cover"]}},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();
        assert_eq!(config.safety.gate.gated, ["auto_focus"]);
        assert_eq!(config.safety.gate.ungated, ["open_cover"]);
    }

    #[test]
    fn safety_gate_rejects_unknown_field() {
        let msg = write_and_load(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "safety": {"gate": {"blocked": ["slew"]}},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap_err();
        assert!(
            msg.contains("blocked") || msg.contains("unknown field"),
            "expected unknown-field diagnostic, got: {msg}"
        );
    }

    /// Startup aborts on the first offending entry, naming its path.
    #[test]
    fn safety_gate_naming_an_unknown_tool_fails_the_load() {
        let msg = write_and_load(
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "safety": {"gate": {"gated": ["no_such_tool"]}},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap_err();
        assert!(msg.contains("safety.gate.gated.0"), "got: {msg}");
        assert!(msg.contains("no_such_tool"), "got: {msg}");
    }

    /// `PUT /api/config` sees the same rules as field errors.
    #[test]
    fn validate_config_reports_a_tool_on_both_sides() {
        let mut config: Config = serde_json::from_str(MINIMAL_CONFIG_JSON).unwrap();
        config.safety.gate = GateOverrides {
            gated: vec!["open_cover".to_owned()],
            ungated: vec!["open_cover".to_owned()],
        };
        let errors = validate_config(&config);
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert_eq!(errors[0].path, "safety.gate.gated.0");
        assert!(
            errors[0].msg.contains("both gated and ungated"),
            "{}",
            errors[0].msg
        );
    }
}
