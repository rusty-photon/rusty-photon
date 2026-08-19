//! rp's `server` block is the shared advertising shape — the plain-HTTP
//! fields plus `advertised_url`, which rp uses to tell an orchestrator
//! where its MCP endpoint is (the invocation's `mcp_server_url`).
//!
//! It is the shared type rather than an rp-local copy of it so that the
//! shape rp accepts is by construction the shape `rusty-photon-doctor`
//! validates rp's config against.

pub use rusty_photon_server_config::{AdvertisedUrl, AdvertisingServerConfig as ServerConfig};

/// rp's default `server` block when the config file omits it: port 11115 on
/// all interfaces, plain HTTP.
pub(crate) const fn default_server() -> ServerConfig {
    ServerConfig::new(11115)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use crate::config::load_config;

    #[test]
    fn server_config_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "server": {"port": 11115, "discovery_port": 32227}
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("discovery_port"), "{err}");
    }

    fn write_config(dir: &tempfile::TempDir, server: &str) -> std::path::PathBuf {
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            format!(
                r#"{{
                    "session": {{"data_directory": "/tmp/rp-test"}},
                    "equipment": {{}},
                    "server": {server}
                }}"#
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn advertised_url_loads_with_trailing_slash_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            r#"{"port": 11115, "advertised_url": "https://observatory.example:11115/"}"#,
        );

        let config = load_config(&path).unwrap();
        assert_eq!(
            config.server.advertised_url.unwrap().as_str(),
            "https://observatory.example:11115"
        );
    }

    #[test]
    fn advertised_url_rejects_non_http_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            r#"{"port": 11115, "advertised_url": "ftp://observatory.example"}"#,
        );

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("server.advertised_url"), "{err}");
        assert!(err.contains("http://"), "{err}");
    }

    #[test]
    fn advertised_url_rejects_missing_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, r#"{"port": 11115, "advertised_url": "https://"}"#);

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("no host"), "{err}");
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod doctor_toml_parity {
    use rusty_photon_server_config::doctor_toml::{parse, ServerClass};
    use rusty_photon_server_config::AdvertisingServerConfig;

    use super::{default_server, ServerConfig};

    /// `pkg/doctor.toml` is this service's catalog entry for
    /// `rusty-photon-doctor` and must match the config defaults
    /// (docs/services/doctor.md §The derived catalog).
    #[test]
    fn pkg_doctor_toml_matches_config_defaults() {
        let meta = parse(include_str!("../../pkg/doctor.toml")).unwrap();
        assert_eq!(meta.port, default_server().port);
        assert_eq!(meta.class, ServerClass::Advertising);
    }

    /// The declared class says which shape doctor validates rp's `server`
    /// block against; this pins it to the type rp actually deserializes,
    /// so reintroducing a service-local shape is a compile error here
    /// rather than a false `config.server-shape` failure on a rig.
    #[test]
    fn the_declared_class_is_the_shape_rp_uses() {
        let _: ServerConfig = AdvertisingServerConfig::new(11115);
    }
}
