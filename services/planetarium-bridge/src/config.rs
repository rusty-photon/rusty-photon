//! Configuration for planetarium-bridge
//! (docs/services/planetarium-bridge.md § Configuration).
//!
//! Invariants are parsed, not validated
//! (docs/skills/development-workflow.md): range-carrying fields are newtypes
//! whose serde `try_from` rejects a bad value at load with a message naming
//! the field, so a broken config fails at startup rather than mid-session.

use std::path::PathBuf;
use std::time::Duration;

use rp_mcp_client::ClientAuthConfig;
pub use rusty_photon_server_config::AlpacaServerConfig;
use serde::{Deserialize, Serialize};

/// The bridge's default Alpaca port (docs/workspace.md port table).
pub const DEFAULT_PORT: u16 = 11126;

/// Main configuration structure.
///
/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: AlpacaServerConfig,
    #[serde(default)]
    pub site: SiteConfig,
    pub rp: RpConfig,
    #[serde(default)]
    pub device: DeviceConfig,
    #[serde(default)]
    pub spool: SpoolConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: AlpacaServerConfig::new(DEFAULT_PORT),
            site: SiteConfig::default(),
            rp: RpConfig::default(),
            device: DeviceConfig::default(),
            spool: SpoolConfig::default(),
        }
    }
}

/// The startup-default observing site. A client site push (`SkySafari` sends
/// its GPS-derived coordinates after connect) overrides these live; a
/// restart reverts to them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SiteConfig {
    /// WGS84 latitude in degrees, north positive.
    pub site_latitude_deg: LatitudeDeg,
    /// WGS84 longitude in degrees, east positive (ASCOM convention).
    pub site_longitude_deg: LongitudeDeg,
    /// Elevation in meters. Reported via `SiteElevation` only.
    #[serde(default)]
    pub site_elevation_m: f64,
}

impl Default for SiteConfig {
    fn default() -> Self {
        Self {
            site_latitude_deg: LatitudeDeg(33.0),
            site_longitude_deg: LongitudeDeg(-117.0),
            site_elevation_m: 0.0,
        }
    }
}

/// Latitude in degrees, `[-90, 90]`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(into = "f64", try_from = "f64")]
pub struct LatitudeDeg(f64);

impl LatitudeDeg {
    #[must_use]
    pub const fn degrees(self) -> f64 {
        self.0
    }
}

impl From<LatitudeDeg> for f64 {
    fn from(v: LatitudeDeg) -> Self {
        v.0
    }
}

impl TryFrom<f64> for LatitudeDeg {
    type Error = String;

    fn try_from(v: f64) -> Result<Self, Self::Error> {
        if (-90.0..=90.0).contains(&v) {
            Ok(Self(v))
        } else {
            Err(format!(
                "site.site_latitude_deg must be within [-90, 90], got {v}"
            ))
        }
    }
}

/// Longitude in degrees, `[-180, 180]`, east positive.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(into = "f64", try_from = "f64")]
pub struct LongitudeDeg(f64);

impl LongitudeDeg {
    #[must_use]
    pub const fn degrees(self) -> f64 {
        self.0
    }
}

impl From<LongitudeDeg> for f64 {
    fn from(v: LongitudeDeg) -> Self {
        v.0
    }
}

impl TryFrom<f64> for LongitudeDeg {
    type Error = String;

    fn try_from(v: f64) -> Result<Self, Self::Error> {
        if (-180.0..=180.0).contains(&v) {
            Ok(Self(v))
        } else {
            Err(format!(
                "site.site_longitude_deg must be within [-180, 180], got {v}"
            ))
        }
    }
}

/// Where and how the bridge reaches rp's MCP endpoint.
///
/// The credential and CA fields follow the fleet client-wiring shape
/// (ADR-017): doctor provisions them, `rp-mcp-client` enforces the
/// credentials-only-over-verified-HTTPS policy.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RpConfig {
    pub mcp_server_url: McpServerUrl,
    #[serde(default)]
    pub service_auth: Option<ClientAuthConfig>,
    #[serde(default)]
    pub ca_cert: Option<PathBuf>,
}

impl Default for RpConfig {
    fn default() -> Self {
        Self {
            // rp's default port, same-host plain HTTP — the pre-doctor
            // starting point; doctor's client wiring upgrades it.
            mcp_server_url: McpServerUrl("http://127.0.0.1:11115/mcp".to_owned()),
            service_auth: None,
            ca_cert: None,
        }
    }
}

/// A well-formed `http(s)://` URL for rp's MCP endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(into = "String", try_from = "String")]
pub struct McpServerUrl(String);

impl McpServerUrl {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<McpServerUrl> for String {
    fn from(v: McpServerUrl) -> Self {
        v.0
    }
}

impl TryFrom<String> for McpServerUrl {
    type Error = String;

    fn try_from(v: String) -> Result<Self, Self::Error> {
        let rest = v
            .strip_prefix("http://")
            .or_else(|| v.strip_prefix("https://"))
            .ok_or_else(|| {
                format!("rp.mcp_server_url must start with http:// or https://, got {v:?}")
            })?;
        if rest.is_empty() || rest.starts_with('/') {
            return Err(format!("rp.mcp_server_url has no host: {v:?}"));
        }
        Ok(Self(v))
    }
}

/// Virtual-device behavior knobs.
///
/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    /// ASCOM `UniqueID`. Minted as a `UUIDv4` on first run by
    /// `rusty_photon_config::materialize_identity` (JSON pointer
    /// `/device/unique_id`), persisted, and never overwritten. Defaults to
    /// an empty string so an absent or empty value triggers minting.
    #[serde(default)]
    pub unique_id: String,
    /// Simulated slew convergence window: `Slewing` reads true and the
    /// reported position interpolates toward the target for this long.
    #[serde(default = "default_slew_duration", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub slew_duration: Duration,
    /// Frame the client's coordinates are assumed to be in. The device
    /// declares `EquatorialSystem = J2000`; `"jnow"` covers clients that
    /// ignore the declaration (received coordinates are converted to ICRS
    /// once, at receipt).
    #[serde(default)]
    pub assume_epoch: AssumeEpoch,
    /// Reported-position altitude floor in degrees: while the virtual
    /// pointing's computed altitude is below this, `RightAscension` /
    /// `Declination` report the zenith idle point instead (the P3a wedge
    /// defense). Explicit `null` disables the policy and reports raw
    /// virtual pointing.
    #[serde(default = "default_floor")]
    pub report_altitude_floor_deg: Option<FloorDeg>,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            unique_id: String::new(),
            slew_duration: default_slew_duration(),
            assume_epoch: AssumeEpoch::default(),
            report_altitude_floor_deg: default_floor(),
        }
    }
}

const fn default_slew_duration() -> Duration {
    Duration::from_secs(3)
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "a serde `default =` fn must return the field's own `Option` type"
)]
const fn default_floor() -> Option<FloorDeg> {
    Some(FloorDeg(10.0))
}

/// The frame received coordinates are assumed to be in.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum AssumeEpoch {
    /// Received coordinates are ICRS/J2000 (the declared system); used as-is.
    #[default]
    J2000,
    /// Received coordinates are apparent-of-date; converted to ICRS at
    /// receipt via ERFA.
    Jnow,
}

/// An altitude floor in degrees, `[-90, 90]`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(into = "f64", try_from = "f64")]
pub struct FloorDeg(f64);

impl FloorDeg {
    #[must_use]
    pub const fn degrees(self) -> f64 {
        self.0
    }
}

impl From<FloorDeg> for f64 {
    fn from(v: FloorDeg) -> Self {
        v.0
    }
}

impl TryFrom<f64> for FloorDeg {
    type Error = String;

    fn try_from(v: f64) -> Result<Self, Self::Error> {
        if (-90.0..=90.0).contains(&v) {
            Ok(Self(v))
        } else {
            Err(format!(
                "device.report_altitude_floor_deg must be within [-90, 90] (or null), got {v}"
            ))
        }
    }
}

/// The bounded on-disk FIFO spool for imports rp could not be reached for.
///
/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpoolConfig {
    /// Spool file location. `null` (the default) resolves to
    /// `<platform config root>/planetarium-bridge/spool.jsonl`.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// At this bound the oldest entry is dropped to admit the newest, with
    /// an error log per drop and a `dropped_total` counter increment.
    #[serde(default = "default_max_entries")]
    pub max_entries: MaxEntries,
    /// Cap of the exponential backoff between replay reconnect attempts
    /// (1 s doubling up to this).
    #[serde(default = "default_replay_backoff_max", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub replay_backoff_max: Duration,
}

impl Default for SpoolConfig {
    fn default() -> Self {
        Self {
            path: None,
            max_entries: default_max_entries(),
            replay_backoff_max: default_replay_backoff_max(),
        }
    }
}

const fn default_max_entries() -> MaxEntries {
    MaxEntries(1000)
}

const fn default_replay_backoff_max() -> Duration {
    Duration::from_mins(5)
}

/// A positive spool bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(into = "u64", try_from = "u64")]
pub struct MaxEntries(u64);

impl MaxEntries {
    #[must_use]
    pub fn get(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }
}

impl From<MaxEntries> for u64 {
    fn from(v: MaxEntries) -> Self {
        v.0
    }
}

impl TryFrom<u64> for MaxEntries {
    type Error = String;

    fn try_from(v: u64) -> Result<Self, Self::Error> {
        if v == 0 {
            Err("spool.max_entries must be at least 1".to_owned())
        } else {
            Ok(Self(v))
        }
    }
}

/// Load the configuration from a JSON file. A present-but-corrupt file is
/// surfaced (naming the path) rather than silently reset.
///
/// # Errors
///
/// Returns an error naming the path if the file cannot be read or does
/// not parse as a [`Config`].
pub fn load_config(
    path: &std::path::Path,
) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read config file {}: {e}", path.display()))?;
    let config: Config = serde_json::from_str(&content)
        .map_err(|e| format!("config file {} is invalid: {e}", path.display()))?;
    Ok(config)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_the_design_doc() {
        let config = Config::default();
        assert_eq!(config.server.port, 11126);
        assert_eq!(config.site.site_latitude_deg.degrees(), 33.0);
        assert_eq!(config.site.site_longitude_deg.degrees(), -117.0);
        assert_eq!(config.site.site_elevation_m, 0.0);
        assert_eq!(
            config.rp.mcp_server_url.as_str(),
            "http://127.0.0.1:11115/mcp"
        );
        assert!(config.rp.service_auth.is_none());
        assert!(config.rp.ca_cert.is_none());
        assert_eq!(config.device.unique_id, "");
        assert_eq!(config.device.slew_duration, Duration::from_secs(3));
        assert_eq!(config.device.assume_epoch, AssumeEpoch::J2000);
        assert_eq!(
            config
                .device
                .report_altitude_floor_deg
                .map(FloorDeg::degrees),
            Some(10.0)
        );
        assert!(config.spool.path.is_none());
        assert_eq!(config.spool.max_entries.get(), 1000);
        assert_eq!(config.spool.replay_backoff_max, Duration::from_mins(5));
    }

    #[test]
    fn minimal_config_gets_all_defaults() {
        let config: Config = serde_json::from_str(
            r#"{"server": {"port": 0}, "rp": {"mcp_server_url": "http://127.0.0.1:1/mcp"}}"#,
        )
        .unwrap();
        assert_eq!(config.device.slew_duration, Duration::from_secs(3));
        assert_eq!(
            config
                .device
                .report_altitude_floor_deg
                .map(FloorDeg::degrees),
            Some(10.0)
        );
    }

    #[test]
    fn explicit_null_floor_disables_the_policy() {
        let json = r#"{
            "server": {"port": 0},
            "rp": {"mcp_server_url": "http://127.0.0.1:1/mcp"},
            "device": {"report_altitude_floor_deg": null}
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.device.report_altitude_floor_deg, None);
    }

    #[test]
    fn null_floor_round_trips_as_null() {
        let mut config = Config::default();
        config.device.report_altitude_floor_deg = None;
        let json = serde_json::to_value(&config).unwrap();
        assert!(json["device"]["report_altitude_floor_deg"].is_null());
        let back: Config = serde_json::from_value(json).unwrap();
        assert_eq!(back.device.report_altitude_floor_deg, None);
    }

    #[test]
    fn out_of_range_floor_is_rejected_naming_the_field() {
        let json = r#"{
            "server": {"port": 0},
            "rp": {"mcp_server_url": "http://127.0.0.1:1/mcp"},
            "device": {"report_altitude_floor_deg": 91.0}
        }"#;
        let err = serde_json::from_str::<Config>(json)
            .unwrap_err()
            .to_string();
        assert!(err.contains("report_altitude_floor_deg"), "{err}");
    }

    #[test]
    fn out_of_range_latitude_is_rejected_naming_the_field() {
        let json = r#"{
            "server": {"port": 0},
            "site": {"site_latitude_deg": 91.0, "site_longitude_deg": 0.0},
            "rp": {"mcp_server_url": "http://127.0.0.1:1/mcp"}
        }"#;
        let err = serde_json::from_str::<Config>(json)
            .unwrap_err()
            .to_string();
        assert!(err.contains("site_latitude_deg"), "{err}");
    }

    #[test]
    fn out_of_range_longitude_is_rejected_naming_the_field() {
        let err = LongitudeDeg::try_from(180.5).unwrap_err();
        assert!(err.contains("site_longitude_deg"), "{err}");
    }

    #[test]
    fn zero_max_entries_is_rejected_naming_the_field() {
        let json = r#"{
            "server": {"port": 0},
            "rp": {"mcp_server_url": "http://127.0.0.1:1/mcp"},
            "spool": {"max_entries": 0}
        }"#;
        let err = serde_json::from_str::<Config>(json)
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_entries"), "{err}");
    }

    #[test]
    fn a_non_http_mcp_url_is_rejected_naming_the_field() {
        let err = McpServerUrl::try_from("ftp://host/mcp".to_owned()).unwrap_err();
        assert!(err.contains("mcp_server_url"), "{err}");
        let err = McpServerUrl::try_from("http:///mcp".to_owned()).unwrap_err();
        assert!(err.contains("mcp_server_url"), "{err}");
    }

    #[test]
    fn assume_epoch_parses_the_documented_values() {
        let device: DeviceConfig = serde_json::from_str(r#"{"assume_epoch": "jnow"}"#).unwrap();
        assert_eq!(device.assume_epoch, AssumeEpoch::Jnow);
        let device: DeviceConfig = serde_json::from_str(r#"{"assume_epoch": "j2000"}"#).unwrap();
        assert_eq!(device.assume_epoch, AssumeEpoch::J2000);
        serde_json::from_str::<DeviceConfig>(r#"{"assume_epoch": "b1950"}"#).unwrap_err();
    }

    #[test]
    fn humantime_durations_parse() {
        let device: DeviceConfig = serde_json::from_str(r#"{"slew_duration": "1s"}"#).unwrap();
        assert_eq!(device.slew_duration, Duration::from_secs(1));
        let spool: SpoolConfig = serde_json::from_str(r#"{"replay_backoff_max": "2s"}"#).unwrap();
        assert_eq!(spool.replay_backoff_max, Duration::from_secs(2));
    }

    #[test]
    fn typoed_fields_are_rejected_loudly() {
        let err = serde_json::from_str::<Config>(r#"{"servre": {}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("servre"), "{err}");
        serde_json::from_str::<DeviceConfig>(r#"{"slew_secs": 3}"#).unwrap_err();
        serde_json::from_str::<SpoolConfig>(r#"{"max": 10}"#).unwrap_err();
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
        assert_eq!(meta.class, ServerClass::Alpaca);
        // A virtual device: no serial port, no USB identity.
        assert!(meta.serial.is_none());
        assert!(meta.usb.is_none());
    }
}
