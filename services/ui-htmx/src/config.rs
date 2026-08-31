//! Configuration for the `ui-htmx` BFF.
//!
//! The BFF is not an ASCOM device; this is its own small config, and its
//! source of truth is **rp's roster** (ADR-016 decision 9, tightened by
//! issue #569): the config is the listening port plus the required
//! [`RpTarget`], and every config target comes from the roster at request
//! time — there is no second, hand-maintained device list. The retired
//! `drivers` override map fails loudly at load (`deny_unknown_fields`);
//! doctor's `config.retired-keys` fix deletes it (see
//! `docs/services/ui-htmx.md`).

use std::path::{Path, PathBuf};

pub use rusty_photon_server_config::ServerConfig;
use serde::{Deserialize, Serialize};

/// Top-level BFF configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_server")]
    pub server: ServerConfig,
    /// The rp orchestrator — the roster source of truth for every surface
    /// (`/` settings page, `/equipment` roster, `/stream` feed, and the
    /// roster-derived device config pages). **Required**: an rp-less BFF has
    /// no purpose, so a config without the block (or with `"rp": null`)
    /// fails loudly at load instead of starting a useless server.
    pub rp: RpTarget,
    /// Where Sentinel's dashboard/REST API lives. Absent (the default) means
    /// no restart affordances are rendered anywhere — Sentinel is optional
    /// infrastructure. See `docs/services/ui-htmx.md` §Restart via Sentinel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sentinel: Option<SentinelTarget>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: default_server(),
            rp: RpTarget::default(),
            sentinel: None,
        }
    }
}

/// The BFF's default `server` block when the config file omits it: port 11120
/// on all interfaces, plain HTTP.
const fn default_server() -> ServerConfig {
    ServerConfig::new(11120)
}

/// How to reach Sentinel's dashboard/REST API (the restart endpoint).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SentinelTarget {
    #[serde(default = "default_sentinel_base_url")]
    pub base_url: String,
    /// Optional HTTP Basic credentials for an auth-enabled dashboard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<DriverAuth>,
    /// Optional PEM CA path for a TLS-enabled dashboard (trusted via `rusty-photon-tls`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<PathBuf>,
}

/// HTTP Basic credentials the BFF presents to a driver.
#[derive(Clone, Serialize, Deserialize, derive_more::Debug)]
#[serde(deny_unknown_fields)]
pub struct DriverAuth {
    pub username: String,
    /// Plaintext Basic-Auth password — redacted from `Debug` so it never lands
    /// in logs or test output.
    #[debug("<redacted>")]
    pub password: String,
}

/// How to reach the rp orchestrator's REST API (`/api/config`, `/api/equipment`,
/// `/api/events/subscribe`, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpTarget {
    #[serde(default = "default_rp_base_url")]
    pub base_url: String,
    /// Optional HTTP Basic credentials for an auth-enabled rp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<DriverAuth>,
    /// Optional PEM CA path for a TLS-enabled rp (trusted via `rusty-photon-tls`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<PathBuf>,
}

impl Default for RpTarget {
    fn default() -> Self {
        Self {
            base_url: default_rp_base_url(),
            auth: None,
            ca_cert_path: None,
        }
    }
}

fn default_rp_base_url() -> String {
    // rp's default server port (services/rp/src/config/server.rs).
    "http://127.0.0.1:11115".to_string()
}

fn default_sentinel_base_url() -> String {
    "http://127.0.0.1:11114".to_string()
}

/// Load BFF configuration from a JSON file.
///
/// # Errors
///
/// Returns an error if the file cannot be read or does not parse as a
/// [`Config`].
pub fn load_config(path: &Path) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let content = std::fs::read_to_string(path)?;
    let config = serde_json::from_str(&content)?;
    Ok(config)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_roster_first() {
        // The single-box default: rp on localhost — rp's roster is the only
        // device source (ADR-016 decision 9, tightened by #569).
        let c = Config::default();
        assert_eq!(c.server.bind_address.to_string(), "0.0.0.0");
        assert_eq!(c.server.port, 11120);
        assert!(c.server.tls.is_none());
        assert!(c.server.auth.is_none());
        assert_eq!(c.rp.base_url, "http://127.0.0.1:11115");
    }

    #[test]
    fn deserialises_with_defaults_for_omitted_fields() {
        let json = r#"{ "server": { "port": 9000 }, "rp": {} }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.server.port, 9000);
        assert_eq!(c.server.bind_address.to_string(), "0.0.0.0");
        assert_eq!(c.rp.base_url, "http://127.0.0.1:11115");
    }

    #[test]
    fn missing_rp_target_is_rejected() {
        // rp is required: an rp-less BFF has no purpose (every surface is
        // rp-backed), so the config must name where rp is.
        let err = serde_json::from_str::<Config>(r#"{ "server": { "port": 9000 } }"#).unwrap_err();
        assert!(err.to_string().contains("rp"), "{err}");
    }

    #[test]
    fn retired_drivers_map_is_rejected() {
        // The static drivers override map is retired (#569): rp's roster is
        // the only device source, and a config still carrying the key —
        // even empty — fails loudly instead of being silently ignored.
        let err = serde_json::from_str::<Config>(r#"{ "rp": {}, "drivers": {} }"#).unwrap_err();
        assert!(err.to_string().contains("drivers"), "{err}");
    }

    #[test]
    fn default_scaffold_round_trips_through_load_config() {
        // main() writes `Config::default()` to the XDG path on first start
        // (resolve_and_init); that serialized form must load back cleanly.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui-htmx.json");
        let scaffold = serde_json::to_string_pretty(&Config::default()).unwrap();
        std::fs::write(&path, scaffold).unwrap();
        let c = load_config(&path).unwrap();
        assert_eq!(c.server.port, 11120);
        assert_eq!(c.rp.base_url, "http://127.0.0.1:11115");
    }

    #[test]
    fn sentinel_block_absent_by_default() {
        let c = Config::default();
        assert!(c.sentinel.is_none());
    }

    #[test]
    fn deserialises_sentinel_block() {
        let json = r#"{
            "rp": {},
            "sentinel": {
                "base_url": "http://127.0.0.1:19114",
                "auth": { "username": "obs", "password": "secret" }
            }
        }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        let sentinel = c.sentinel.unwrap();
        assert_eq!(sentinel.base_url, "http://127.0.0.1:19114");
        assert_eq!(sentinel.auth.unwrap().username, "obs");
    }

    #[test]
    fn sentinel_base_url_defaults_to_dashboard_port() {
        let json = r#"{ "rp": {}, "sentinel": {} }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.sentinel.unwrap().base_url, "http://127.0.0.1:11114");
    }

    #[test]
    fn load_config_reads_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui-htmx.json");
        std::fs::write(&path, r#"{ "server": { "port": 8080 }, "rp": {} }"#).unwrap();
        let c = load_config(&path).unwrap();
        assert_eq!(c.server.port, 8080);
    }

    #[test]
    fn load_config_missing_file_errors() {
        load_config(Path::new("/tmp/ui_htmx_nonexistent_4242.json")).unwrap_err();
    }

    #[test]
    fn rp_target_null_is_rejected() {
        // The pre-#569 "run without rp (pure driver UI)" mode is gone with
        // the drivers map; an explicit null is a config error, not a mode.
        serde_json::from_str::<Config>(r#"{ "rp": null }"#).unwrap_err();
    }

    #[test]
    fn rp_target_parses_when_present() {
        let json = r#"{
            "rp": {
                "base_url": "https://pi.local:11115",
                "auth": { "username": "obs", "password": "secret" }
            }
        }"#;
        let c: Config = serde_json::from_str(json).unwrap();
        assert_eq!(c.rp.base_url, "https://pi.local:11115");
        assert_eq!(c.rp.auth.unwrap().username, "obs");
    }

    #[test]
    fn rp_target_defaults_base_url_to_rp_port() {
        let c: Config = serde_json::from_str(r#"{ "rp": {} }"#).unwrap();
        assert_eq!(c.rp.base_url, "http://127.0.0.1:11115");
    }

    #[test]
    fn rp_target_default_impl_matches_the_serde_defaults() {
        // `RpTarget::default()` and `serde_json::from_str("{}")` must agree —
        // both paths hand out the same scaffold.
        let d = RpTarget::default();
        assert_eq!(d.base_url, "http://127.0.0.1:11115");
        assert!(d.auth.is_none());
        assert!(d.ca_cert_path.is_none());
        // And an rp target round-trips through serialization unchanged.
        let json = serde_json::to_string(&d).unwrap();
        let back: RpTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back.base_url, d.base_url);
        assert!(back.auth.is_none());
        assert!(back.ca_cert_path.is_none());
    }

    #[test]
    fn config_rejects_unknown_top_level_field() {
        let err = serde_json::from_str::<Config>(r#"{"plugins": []}"#).unwrap_err();
        assert!(err.to_string().contains("plugins"), "{err}");
    }

    #[test]
    fn sentinel_target_rejects_unknown_field() {
        let json = r#"{"sentinel": {"webhook_url": "http://x"}}"#;
        let err = serde_json::from_str::<Config>(json).unwrap_err();
        assert!(err.to_string().contains("webhook_url"), "{err}");
    }

    #[test]
    fn driver_auth_rejects_unknown_field() {
        let json = r#"{"rp": {"auth": {"username": "u", "password": "p", "realm": "x"}}}"#;
        let err = serde_json::from_str::<Config>(json).unwrap_err();
        assert!(err.to_string().contains("realm"), "{err}");
    }

    #[test]
    fn rp_target_rejects_unknown_field() {
        let json = r#"{"rp": {"discovery_port": 32227}}"#;
        let err = serde_json::from_str::<Config>(json).unwrap_err();
        assert!(err.to_string().contains("discovery_port"), "{err}");
    }

    #[test]
    fn driver_auth_password_redacted_in_debug() {
        let auth = DriverAuth {
            username: "obs".to_string(),
            password: "hunter2".to_string(),
        };
        let rendered = format!("{auth:?}");
        assert!(!rendered.contains("hunter2"), "password leaked: {rendered}");
        assert!(rendered.contains("obs"));
        assert!(rendered.contains("<redacted>"));
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod doctor_toml_parity {
    use rusty_photon_server_config::doctor_toml::{parse, ServerClass};

    use super::Config;

    /// `pkg/doctor.toml` is this service's catalog entry for
    /// `rusty-photon-doctor` and must match the config defaults
    /// (docs/services/doctor.md §The derived catalog).
    #[test]
    fn pkg_doctor_toml_matches_config_defaults() {
        let meta = parse(include_str!("../pkg/doctor.toml")).unwrap();
        assert_eq!(meta.port, Config::default().server.port);
        assert_eq!(meta.class, ServerClass::Core);
    }
}

#[cfg(test)]
mod persisted_config_shape {
    use rusty_photon_server_config::unset::explicit_nulls;

    use super::Config;

    /// An unset optional field is spelled by its key's absence, never by an
    /// explicit `null` — see [`rusty_photon_server_config::unset`] for why.
    /// A field that grows without `skip_serializing_if` trips here rather
    /// than filling operators' config files with nulls.
    #[test]
    fn the_default_config_persists_no_explicit_nulls() {
        let persisted = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(
            explicit_nulls(&persisted),
            Vec::<String>::new(),
            "unset optional fields must be omitted, not written as null: {persisted}"
        );
    }
}
