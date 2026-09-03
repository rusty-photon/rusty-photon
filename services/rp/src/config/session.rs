use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    pub data_directory: String,
    /// Where the session state file lives (rp.md § Session
    /// Persistence): the session registry, written on every transition
    /// and read back for startup recovery.
    /// Empty (the default) resolves to
    /// `<data_directory>/session_state.json` — see
    /// [`Self::session_state_path`], the one place that derivation
    /// lives.
    #[serde(default)]
    pub session_state_file: String,
    /// Optional template for capture filenames. `None` is the default and
    /// produces filenames of the form `<doc_uuid_8>.fits` plus a matching
    /// `.json` sidecar — fully self-identifying via the UUID-8 suffix that
    /// drives the disk-fallback resolution path. When set, `capture`
    /// writes `<rendered pattern>_<doc_uuid_8>.fits`: the suffix is
    /// appended by rp, never spelled in the pattern, so it survives any
    /// pattern the operator chooses. The pattern is parsed and validated
    /// at config-load time against the token contract in
    /// [`crate::config::naming_template`] (missing quota tokens, a
    /// `{uuid8}` token, unknown tokens, and ambiguous adjacent tokens
    /// all fail startup). `capture` renders it (together with
    /// `directory_pattern`) whenever its `frame_type` parameter is
    /// supplied (Decision 11) — otherwise `capture` still writes
    /// `<doc_uuid_8>.fits` regardless of this value. See
    /// `docs/services/rp.md` (Persistence, Capture Tool Details),
    /// `docs/crates/rp-targets.md` (File-naming template), and Phase 7
    /// of `docs/plans/archive/image-evaluation-tools.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_naming_pattern: Option<String>,
    /// Optional template for the per-frame subdirectory `capture`
    /// nests its rendered filename under (rp-targets.md § File-naming
    /// template). `None` falls back to the documented default,
    /// `"{target}/{night_date}/{frame_type}"`, whenever
    /// `file_naming_pattern` is set — only the file pattern needs
    /// explicit configuration to opt in. Parsed and validated at
    /// config-load time the same way `file_naming_pattern` is, but
    /// without its quota-token requirement (see
    /// [`crate::config::naming_template::validate_directory_pattern`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_pattern: Option<String>,
}

impl SessionConfig {
    /// The resolved session-state-file path: `session_state_file` when
    /// set, else `<data_directory>/session_state.json`. Kept on the
    /// config type so every consumer derives the same path.
    #[must_use]
    pub fn session_state_path(&self) -> PathBuf {
        if self.session_state_file.is_empty() {
            PathBuf::from(&self.data_directory).join("session_state.json")
        } else {
            PathBuf::from(&self.session_state_file)
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use crate::config::load_config;
    use crate::config::test_support::MINIMAL_CONFIG_JSON;

    #[test]
    fn file_naming_pattern_defaults_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, MINIMAL_CONFIG_JSON).unwrap();

        let config = load_config(&path).unwrap();
        assert!(
            config.session.file_naming_pattern.is_none(),
            "omitted file_naming_pattern must deserialize to None"
        );
    }

    #[test]
    fn an_unknown_session_key_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {
                    "data_directory": "/tmp/rp-test",
                    "session_state_flie": "/tmp/typo.json"
                },
                "equipment": {},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let error = load_config(&path).unwrap_err().to_string();
        assert!(
            error.contains("session_state_flie"),
            "a typo'd session key must be rejected, not silently ignored: {error}"
        );
    }

    #[test]
    fn file_naming_pattern_round_trips_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {
                    "data_directory": "/tmp/rp-test",
                    "file_naming_pattern": "{target}_{filter}_{binning}_{exposure_duration}"
                },
                "equipment": {},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(
            config.session.file_naming_pattern.as_deref(),
            Some("{target}_{filter}_{binning}_{exposure_duration}")
        );
    }

    #[test]
    fn invalid_file_naming_pattern_fails_to_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {
                    "data_directory": "/tmp/rp-test",
                    "file_naming_pattern": "{target}_{bogus_token}"
                },
                "equipment": {},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let error = load_config(&path).unwrap_err().to_string();
        assert!(
            error.contains("file_naming_pattern") && error.contains("bogus_token"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn directory_pattern_defaults_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, MINIMAL_CONFIG_JSON).unwrap();

        let config = load_config(&path).unwrap();
        assert!(
            config.session.directory_pattern.is_none(),
            "omitted directory_pattern must deserialize to None"
        );
    }

    #[test]
    fn directory_pattern_round_trips_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {
                    "data_directory": "/tmp/rp-test",
                    "directory_pattern": "{target}/{night_date}/{frame_type}"
                },
                "equipment": {},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(
            config.session.directory_pattern.as_deref(),
            Some("{target}/{night_date}/{frame_type}")
        );
    }

    #[test]
    fn invalid_directory_pattern_fails_to_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {
                    "data_directory": "/tmp/rp-test",
                    "directory_pattern": "{target}_{bogus_token}"
                },
                "equipment": {},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let error = load_config(&path).unwrap_err().to_string();
        assert!(
            error.contains("directory_pattern") && error.contains("bogus_token"),
            "unexpected error: {error}"
        );
    }
}
