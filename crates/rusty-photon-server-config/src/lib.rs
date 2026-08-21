//! The shared `server` block for rusty-photon service configs.
//!
//! Three shapes, all `deny_unknown_fields`: [`ServerConfig`] for services
//! that serve a plain HTTP API (ui-htmx, sentinel, plate-solver,
//! session-runner, calibrator-flats, phd2-guider), [`AlpacaServerConfig`]
//! for Alpaca drivers, which adds the optional UDP discovery responder port,
//! and [`AdvertisingServerConfig`] for services that advertise their own URL
//! to another process, which adds `advertised_url` (rp alone today). They are
//! separate types so neither extra field can appear — accepted but silently
//! inert — in a service that never reads it. The two extended shapes repeat
//! the core fields instead of flattening them because serde's
//! `deny_unknown_fields` does not compose with `flatten`.
//!
//! Every service defines its `server` block as one of these types rather
//! than its own copy, so the shape a service accepts and the shape doctor
//! validates it against cannot drift.
//!
//! Absent `tls` / `auth` means plain, unauthenticated HTTP (ADR-016
//! decision 10(d)): both are switched on by doctor's generated config, never
//! by a serde default. Default ports are per-service and supplied by each
//! service's parent config, not by these types.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

pub mod doctor_toml;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use rp_auth::config::AuthConfig;
use rusty_photon_tls::config::TlsConfig;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The unified default bind address: all interfaces.
const fn default_bind_address() -> IpAddr {
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}

/// Server configuration for non-Alpaca services.
///
/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub port: u16,
    /// Interface to bind. `0.0.0.0` (the default) listens on all interfaces;
    /// an `IpAddr` rather than a string so a malformed address fails at
    /// config load, not at bind time.
    #[serde(default = "default_bind_address")]
    pub bind_address: IpAddr,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
}

impl ServerConfig {
    /// Plain HTTP on `0.0.0.0:{port}` — the shape services use for their
    /// self-created default configs.
    #[must_use]
    pub const fn new(port: u16) -> Self {
        Self {
            port,
            bind_address: default_bind_address(),
            tls: None,
            auth: None,
        }
    }

    /// The address the listener binds to.
    #[must_use]
    pub const fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.bind_address, self.port)
    }
}

/// Server configuration for Alpaca drivers: [`ServerConfig`]'s fields plus
/// the UDP discovery responder port.
///
/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AlpacaServerConfig {
    pub port: u16,
    /// Interface to bind. `0.0.0.0` (the default) listens on all interfaces;
    /// an `IpAddr` rather than a string so a malformed address fails at
    /// config load, not at bind time.
    #[serde(default = "default_bind_address")]
    pub bind_address: IpAddr,
    /// Alpaca UDP discovery responder port (normally 32227). Absent/`null` —
    /// the default — disables discovery: many rusty-photon servers on one
    /// host would collide on the shared discovery port, so it is a per-host
    /// opt-in for single-driver deployments. `skip_serializing_if` so
    /// `config.apply` cannot re-persist a stale key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_port: Option<u16>,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
}

impl AlpacaServerConfig {
    /// Plain HTTP on `0.0.0.0:{port}` with discovery off — the shape drivers
    /// use for their self-created default configs.
    #[must_use]
    pub const fn new(port: u16) -> Self {
        Self {
            port,
            bind_address: default_bind_address(),
            discovery_port: None,
            tls: None,
            auth: None,
        }
    }

    /// The address the listener binds to.
    #[must_use]
    pub const fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.bind_address, self.port)
    }

    /// The common-subset view (everything except `discovery_port`) that
    /// consumers reading the `server` block out of arbitrary service configs
    /// use to treat both shapes uniformly.
    #[must_use]
    pub fn core(&self) -> ServerConfig {
        ServerConfig {
            port: self.port,
            bind_address: self.bind_address,
            tls: self.tls.clone(),
            auth: self.auth.clone(),
        }
    }
}

/// Server configuration for services that advertise their own URL to
/// another process.
///
/// [`ServerConfig`]'s fields plus `advertised_url`. rp is the only one
/// today — it hands an orchestrator its MCP endpoint.
///
/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdvertisingServerConfig {
    pub port: u16,
    /// Interface to bind. `0.0.0.0` (the default) listens on all interfaces;
    /// an `IpAddr` rather than a string so a malformed address fails at
    /// config load, not at bind time.
    #[serde(default = "default_bind_address")]
    pub bind_address: IpAddr,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    /// Overrides the URL advertised to other processes — rp appends `/mcp`
    /// (rp.md § MCP Server). Unset, the URL is derived from the listener;
    /// a wildcard bind advertises the system hostname.
    #[serde(default)]
    pub advertised_url: Option<AdvertisedUrl>,
}

impl AdvertisingServerConfig {
    /// Plain HTTP on `0.0.0.0:{port}`, advertising the derived URL — the
    /// shape services use for their self-created default configs.
    #[must_use]
    pub const fn new(port: u16) -> Self {
        Self {
            port,
            bind_address: default_bind_address(),
            tls: None,
            auth: None,
            advertised_url: None,
        }
    }

    /// The address the listener binds to.
    #[must_use]
    pub const fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.bind_address, self.port)
    }

    /// The common-subset view (everything except `advertised_url`) that
    /// consumers reading the `server` block out of arbitrary service configs
    /// use to treat every shape uniformly.
    #[must_use]
    pub fn core(&self) -> ServerConfig {
        ServerConfig {
            port: self.port,
            bind_address: self.bind_address,
            tls: self.tls.clone(),
            auth: self.auth.clone(),
        }
    }
}

/// An `http://` / `https://` base URL, validated at config load and held
/// with any trailing `/` trimmed so appending a path cannot produce `//`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct AdvertisedUrl(String);

impl AdvertisedUrl {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for AdvertisedUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let scheme_len = if value.starts_with("http://") {
            "http://".len()
        } else if value.starts_with("https://") {
            "https://".len()
        } else {
            return Err(serde::de::Error::custom(format!(
                "server.advertised_url must be an http:// or https:// URL, got {value:?}"
            )));
        };
        let trimmed = value.trim_end_matches('/');
        if trimmed.len() <= scheme_len {
            return Err(serde::de::Error::custom(format!(
                "server.advertised_url has no host: {value:?}"
            )));
        }
        Ok(Self(trimmed.to_string()))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn server_config_minimal_json_gets_defaults() {
        let config: ServerConfig = serde_json::from_str(r#"{"port": 11115}"#).unwrap();
        assert_eq!(config.port, 11115);
        assert_eq!(config.bind_address, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert!(config.tls.is_none());
        assert!(config.auth.is_none());
    }

    #[test]
    fn alpaca_server_config_minimal_json_gets_defaults() {
        let config: AlpacaServerConfig = serde_json::from_str(r#"{"port": 11112}"#).unwrap();
        assert_eq!(config.port, 11112);
        assert_eq!(config.bind_address, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert!(config.discovery_port.is_none());
        assert!(config.tls.is_none());
        assert!(config.auth.is_none());
    }

    #[test]
    fn server_config_rejects_discovery_port() {
        let err = serde_json::from_str::<ServerConfig>(r#"{"port": 1, "discovery_port": 32227}"#)
            .unwrap_err();
        assert!(
            err.to_string().contains("discovery_port"),
            "error should name the rejected field: {err}"
        );
    }

    #[test]
    fn every_shape_rejects_unknown_fields() {
        serde_json::from_str::<ServerConfig>(r#"{"port": 1, "prot": 2}"#).unwrap_err();
        serde_json::from_str::<AlpacaServerConfig>(r#"{"port": 1, "prot": 2}"#).unwrap_err();
        serde_json::from_str::<AdvertisingServerConfig>(r#"{"port": 1, "prot": 2}"#).unwrap_err();
    }

    #[test]
    fn every_shape_requires_port() {
        serde_json::from_str::<ServerConfig>("{}").unwrap_err();
        serde_json::from_str::<AlpacaServerConfig>("{}").unwrap_err();
        serde_json::from_str::<AdvertisingServerConfig>("{}").unwrap_err();
    }

    /// The reason the shapes are separate types: an extra field must not be
    /// accepted-but-inert anywhere it would never be read.
    #[test]
    fn the_extra_fields_do_not_cross_shapes() {
        let err = serde_json::from_str::<ServerConfig>(
            r#"{"port": 1, "advertised_url": "https://host"}"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("advertised_url"),
            "error should name the rejected field: {err}"
        );
        serde_json::from_str::<AlpacaServerConfig>(
            r#"{"port": 1, "advertised_url": "https://host"}"#,
        )
        .unwrap_err();
        serde_json::from_str::<AdvertisingServerConfig>(r#"{"port": 1, "discovery_port": 32227}"#)
            .unwrap_err();
    }

    #[test]
    fn bind_address_parses_loopback() {
        let config: ServerConfig =
            serde_json::from_str(r#"{"port": 1, "bind_address": "127.0.0.1"}"#).unwrap();
        assert_eq!(config.bind_address, IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn bind_address_rejects_non_ip_at_load() {
        serde_json::from_str::<ServerConfig>(r#"{"port": 1, "bind_address": "localhost"}"#)
            .unwrap_err();
    }

    #[test]
    fn bind_address_parses_ipv6() {
        let config: ServerConfig =
            serde_json::from_str(r#"{"port": 1, "bind_address": "::1"}"#).unwrap();
        assert!(config.bind_address.is_ipv6());
    }

    #[test]
    fn socket_addr_composes_bind_address_and_port() {
        assert_eq!(
            ServerConfig::new(11115).socket_addr().to_string(),
            "0.0.0.0:11115"
        );
        assert_eq!(
            AlpacaServerConfig::new(11112).socket_addr().to_string(),
            "0.0.0.0:11112"
        );
    }

    #[test]
    fn absent_discovery_port_is_not_serialized() {
        let json = serde_json::to_value(AlpacaServerConfig::new(11112)).unwrap();
        assert!(json.get("discovery_port").is_none());
    }

    #[test]
    fn present_discovery_port_round_trips() {
        let mut config = AlpacaServerConfig::new(11112);
        config.discovery_port = Some(32227);
        let json = serde_json::to_string(&config).unwrap();
        let back: AlpacaServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.discovery_port, Some(32227));
    }

    #[test]
    fn tls_and_auth_round_trip() {
        let json = r#"{
            "port": 11112,
            "tls": {"cert": "/pki/cert.pem", "key": "/pki/key.pem"},
            "auth": {"username": "observatory", "password_hash": "$argon2id$stub"}
        }"#;
        let config: AlpacaServerConfig = serde_json::from_str(json).unwrap();
        let tls = config.tls.as_ref().unwrap();
        assert_eq!(tls.cert, "/pki/cert.pem");
        assert_eq!(tls.key, "/pki/key.pem");
        assert_eq!(config.auth.as_ref().unwrap().username, "observatory");
        let back: AlpacaServerConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(back.auth.unwrap().username, "observatory");
    }

    #[test]
    fn core_view_carries_everything_but_discovery_port() {
        let mut config = AlpacaServerConfig::new(11112);
        config.bind_address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        config.discovery_port = Some(32227);
        config.auth = Some(AuthConfig {
            username: "observatory".to_string(),
            password_hash: "$argon2id$stub".to_string(),
        });
        let core = config.core();
        assert_eq!(core.port, 11112);
        assert_eq!(core.bind_address, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(core.auth.unwrap().username, "observatory");
    }

    #[test]
    fn json_schema_describes_every_shape() {
        let schema = serde_json::to_value(schemars::schema_for!(ServerConfig)).unwrap();
        assert!(schema["properties"]["bind_address"].is_object());
        assert!(schema["properties"]["discovery_port"].is_null());
        let schema = serde_json::to_value(schemars::schema_for!(AlpacaServerConfig)).unwrap();
        assert!(schema["properties"]["discovery_port"].is_object());
        let schema = serde_json::to_value(schemars::schema_for!(AdvertisingServerConfig)).unwrap();
        assert!(schema["properties"]["advertised_url"].is_object());
    }

    #[test]
    fn advertising_core_view_drops_advertised_url_only() {
        let mut config = AdvertisingServerConfig::new(11115);
        config.bind_address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        config.advertised_url = Some(serde_json::from_str(r#""https://rp.example""#).unwrap());
        config.auth = Some(AuthConfig {
            username: "observatory".to_string(),
            password_hash: "$argon2id$stub".to_string(),
        });
        let core = config.core();
        assert_eq!(core.port, 11115);
        assert_eq!(core.bind_address, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(core.auth.unwrap().username, "observatory");
        assert_eq!(
            AdvertisingServerConfig::new(11115)
                .socket_addr()
                .to_string(),
            "0.0.0.0:11115"
        );
    }

    #[test]
    fn advertised_url_trims_its_trailing_slash() {
        let url: AdvertisedUrl = serde_json::from_str(r#""https://rp.example:11115/""#).unwrap();
        assert_eq!(url.as_str(), "https://rp.example:11115");
    }

    #[test]
    fn advertised_url_accepts_plain_http() {
        let url: AdvertisedUrl = serde_json::from_str(r#""http://rp.example:11115""#).unwrap();
        assert_eq!(url.as_str(), "http://rp.example:11115");
    }

    #[test]
    fn advertised_url_rejects_a_non_http_scheme() {
        let err = serde_json::from_str::<AdvertisedUrl>(r#""ftp://rp.example""#).unwrap_err();
        assert!(err.to_string().contains("http://"), "{err}");
    }

    #[test]
    fn advertised_url_rejects_a_missing_host() {
        let err = serde_json::from_str::<AdvertisedUrl>(r#""https://""#).unwrap_err();
        assert!(err.to_string().contains("no host"), "{err}");
    }

    #[test]
    fn advertised_url_round_trips_through_serialization() {
        let mut config = AdvertisingServerConfig::new(11115);
        config.advertised_url = Some(serde_json::from_str(r#""https://rp.example""#).unwrap());
        let back: AdvertisingServerConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(back.advertised_url.unwrap().as_str(), "https://rp.example");
    }
}
