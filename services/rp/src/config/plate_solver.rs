use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// HTTP-client connection to the `plate-solver` rp-managed service.
///
/// `timeout` is the connection-side outer timeout (the
/// belt-and-suspenders backstop per Tenet 1) — *not* the wrapper's
/// per-solve deadline, which is set by the `plate_solve` MCP tool's
/// per-call `timeout` parameter.
///
/// `default_search_radius_deg` is the operator-set radius applied
/// when the per-call MCP parameter is omitted; per-call overrides
/// for loaded-from-disk images where the configured rig default
/// may not match.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlateSolverConfig {
    pub url: String,
    #[serde(default = "default_plate_solver_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub timeout: Duration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_search_radius_deg: Option<f64>,
    /// Optional HTTP Basic Auth credentials for connecting to an
    /// auth-enabled plate-solver service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<rp_auth::config::ClientAuthConfig>,
}

const fn default_plate_solver_timeout() -> Duration {
    Duration::from_mins(1)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use crate::config::load_config;
    use crate::config::test_support::MINIMAL_CONFIG_JSON;

    #[test]
    fn plate_solver_block_omitted_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, MINIMAL_CONFIG_JSON).unwrap();

        let config = load_config(&path).unwrap();
        assert!(
            config.plate_solver.is_none(),
            "expected plate_solver to be None when omitted from config"
        );
    }

    #[test]
    fn plate_solver_url_only_applies_default_timeout_and_no_radius() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "plate_solver": {"url": "http://127.0.0.1:11131"},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let ps = config.plate_solver.expect("plate_solver should parse");
        assert_eq!(ps.url, "http://127.0.0.1:11131");
        assert_eq!(ps.timeout, Duration::from_mins(1));
        assert!(ps.default_search_radius_deg.is_none());
        assert!(ps.auth.is_none());
    }

    #[test]
    fn plate_solver_with_full_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "plate_solver": {
                    "url": "http://127.0.0.1:11131",
                    "timeout": "30s",
                    "default_search_radius_deg": 4.0,
                    "auth": {"username": "observatory", "password": "secret"}
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let ps = config.plate_solver.expect("plate_solver should parse");
        assert_eq!(ps.url, "http://127.0.0.1:11131");
        assert_eq!(ps.timeout, Duration::from_secs(30));
        assert_eq!(ps.default_search_radius_deg, Some(4.0));
        let auth = ps.auth.as_ref().unwrap();
        assert_eq!(auth.username, "observatory");
        assert_eq!(auth.password, "secret");
    }

    #[test]
    fn plate_solver_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "plate_solver": {
                    "url": "http://127.0.0.1:11131",
                    "bogus_field": 1
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bogus_field") || msg.contains("unknown field"),
            "expected unknown-field diagnostic, got: {msg}"
        );
    }
}
