use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

pub use rusty_photon_server_config::ServerConfig;

use crate::error::{CalibratorFlatsError, Result};

/// Filter plan entry: which filter, how many frames.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterPlan {
    pub name: String,
    pub count: u32,
}

/// Flat calibration plan passed via the orchestrator plugin config. This is
/// also the service's config file, so it carries the HTTP `server` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlatPlan {
    /// The HTTP server for `/runs`, `/status`, `/health`. Plan files
    /// without a `server` block keep loading via the default.
    #[serde(default = "default_server")]
    pub server: ServerConfig,
    /// `rp`'s MCP endpoint, used by every `POST /runs` run; without it
    /// `/runs` answers `400`.
    #[serde(default)]
    pub mcp_server_url: Option<String>,
    pub camera_id: String,
    /// Filter wheel to drive between capture groups. Absent, `null`, or
    /// `""` means the rig has no filter wheel (one-shot color): the
    /// workflow never calls `set_filter` and `filters` entries are plain
    /// capture groups whose `name` is only a label. (`""` matches the
    /// document port's sentinel — its parameter grammar has no
    /// optional-without-default.)
    #[serde(default)]
    pub filter_wheel_id: Option<String>,
    pub calibrator_id: String,
    /// Target median as fraction of max ADU (default 0.5 = 50%)
    #[serde(default = "default_target_adu_fraction")]
    pub target_adu_fraction: f64,
    /// Acceptable deviation from target (default 0.05 = 5%)
    #[serde(default = "default_tolerance")]
    pub tolerance: f64,
    /// Max iterations to find correct exposure time per filter
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Starting exposure time (humantime, e.g. `"1s"`, `"500ms"`)
    #[serde(default = "default_initial_duration", with = "humantime_serde")]
    pub initial_duration: Duration,
    /// Calibrator brightness (null/absent = `max_brightness`)
    #[serde(default)]
    pub brightness: Option<u32>,
    /// Filters to capture flats for
    pub filters: Vec<FilterPlan>,
    /// HTTP Basic credentials presented to `rp` on MCP calls. The D6
    /// observatory credential; doctor `--fix` wires it (ADR-017).
    #[serde(default)]
    pub service_auth: Option<rp_mcp_client::ClientAuthConfig>,
    /// PEM CA path used to trust a TLS-enabled `rp`. Per the ADR-017
    /// policy, `service_auth` is only sent when this is set and the URL
    /// is https.
    #[serde(default)]
    pub ca_cert: Option<String>,
}

impl FlatPlan {
    /// The filter wheel to drive, with the no-wheel spellings (absent,
    /// `null`, `""`) normalized to `None`.
    #[must_use]
    pub fn filter_wheel(&self) -> Option<&str> {
        self.filter_wheel_id.as_deref().filter(|id| !id.is_empty())
    }

    #[must_use]
    pub const fn rp_auth(&self) -> Option<&rp_mcp_client::ClientAuthConfig> {
        self.service_auth.as_ref()
    }

    pub fn rp_ca(&self) -> Option<&std::path::Path> {
        self.ca_cert.as_deref().map(std::path::Path::new)
    }
}

/// calibrator-flats' default `server` block when the plan file omits it:
/// port 11170 on all interfaces, plain HTTP.
pub(crate) const fn default_server() -> ServerConfig {
    ServerConfig::new(11170)
}

/// CLI overrides layered over the file config after load: `--port` and
/// `--bind-address` pin `server.port` / `server.bind_address` over whatever
/// the file (or the `default_server()` fallback) supplied.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `--port` → `server.port`.
    pub port: Option<u16>,
    /// `--bind-address` → `server.bind_address`.
    pub bind_address: Option<IpAddr>,
}

impl CliOverrides {
    /// Apply the overrides onto `plan` in place.
    pub const fn apply(&self, plan: &mut FlatPlan) {
        if let Some(port) = self.port {
            plan.server.port = port;
        }
        if let Some(bind_address) = self.bind_address {
            plan.server.bind_address = bind_address;
        }
    }
}

const fn default_target_adu_fraction() -> f64 {
    0.5
}

const fn default_tolerance() -> f64 {
    0.05
}

const fn default_max_iterations() -> u32 {
    10
}

const fn default_initial_duration() -> Duration {
    Duration::from_secs(1)
}

/// Load a [`FlatPlan`] from the JSON file at `path`.
///
/// # Errors
///
/// Returns [`CalibratorFlatsError::Config`] if the file cannot be read
/// or does not parse as a [`FlatPlan`].
pub fn load_config(path: &Path) -> Result<FlatPlan> {
    let contents = std::fs::read_to_string(path).map_err(|e| {
        CalibratorFlatsError::Config(format!(
            "failed to read config file '{}': {}",
            path.display(),
            e
        ))
    })?;
    serde_json::from_str(&contents).map_err(|e| {
        CalibratorFlatsError::Config(format!(
            "failed to parse config file '{}': {}",
            path.display(),
            e
        ))
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn deserialize_flat_plan_with_defaults() {
        let json = r#"{
            "camera_id": "main-cam",
            "filter_wheel_id": "main-fw",
            "calibrator_id": "flat-panel",
            "filters": [
                {"name": "Luminance", "count": 20}
            ]
        }"#;
        let plan: FlatPlan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.camera_id, "main-cam");
        assert_eq!(plan.target_adu_fraction, 0.5);
        assert_eq!(plan.tolerance, 0.05);
        assert_eq!(plan.max_iterations, 10);
        assert_eq!(plan.initial_duration, Duration::from_secs(1));
        assert!(plan.brightness.is_none());
        assert_eq!(plan.filters.len(), 1);
        // A plan file without a `server` block keeps loading via the default.
        assert_eq!(plan.server.port, 11170);
        assert_eq!(plan.server.bind_address.to_string(), "0.0.0.0");
        assert!(plan.server.tls.is_none());
        assert!(plan.server.auth.is_none());
    }

    #[test]
    fn filter_wheel_absent_null_and_empty_all_mean_no_wheel() {
        for wheel_field in [
            "",
            r#""filter_wheel_id": null,"#,
            r#""filter_wheel_id": "","#,
        ] {
            let json = format!(
                r#"{{
                    "camera_id": "osc-cam",
                    {wheel_field}
                    "calibrator_id": "flat-panel",
                    "filters": [{{"name": "OSC", "count": 20}}]
                }}"#
            );
            let plan: FlatPlan = serde_json::from_str(&json).unwrap();
            assert_eq!(
                plan.filter_wheel(),
                None,
                "spelling {wheel_field:?} must mean no filter wheel"
            );
        }
    }

    #[test]
    fn filter_wheel_present_resolves_to_the_id() {
        let json = r#"{
            "camera_id": "main-cam",
            "filter_wheel_id": "main-fw",
            "calibrator_id": "flat-panel",
            "filters": [{"name": "Luminance", "count": 20}]
        }"#;
        let plan: FlatPlan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.filter_wheel(), Some("main-fw"));
    }

    #[test]
    fn deserialize_flat_plan_with_server_block() {
        let json = r#"{
            "server": { "port": 12000, "bind_address": "127.0.0.1" },
            "camera_id": "main-cam",
            "filter_wheel_id": "main-fw",
            "calibrator_id": "flat-panel",
            "filters": [{"name": "Luminance", "count": 20}]
        }"#;
        let plan: FlatPlan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.server.socket_addr().to_string(), "127.0.0.1:12000");
    }

    #[test]
    fn cli_overrides_pin_port_and_bind_address() {
        let json = r#"{
            "camera_id": "main-cam",
            "filter_wheel_id": "main-fw",
            "calibrator_id": "flat-panel",
            "filters": [{"name": "Luminance", "count": 20}]
        }"#;
        let mut plan: FlatPlan = serde_json::from_str(json).unwrap();
        let overrides = CliOverrides {
            port: Some(12345),
            bind_address: Some("127.0.0.1".parse().unwrap()),
        };
        overrides.apply(&mut plan);
        assert_eq!(plan.server.socket_addr().to_string(), "127.0.0.1:12345");
    }

    #[test]
    fn empty_cli_overrides_leave_the_plan_untouched() {
        let json = r#"{
            "camera_id": "main-cam",
            "filter_wheel_id": "main-fw",
            "calibrator_id": "flat-panel",
            "filters": [{"name": "Luminance", "count": 20}]
        }"#;
        let mut plan: FlatPlan = serde_json::from_str(json).unwrap();
        CliOverrides::default().apply(&mut plan);
        assert_eq!(plan.server.socket_addr().to_string(), "0.0.0.0:11170");
    }

    #[test]
    fn deserialize_flat_plan_with_overrides() {
        let json = r#"{
            "camera_id": "main-cam",
            "filter_wheel_id": "main-fw",
            "calibrator_id": "flat-panel",
            "target_adu_fraction": 0.4,
            "tolerance": 0.1,
            "max_iterations": 5,
            "initial_duration": "500ms",
            "brightness": 80,
            "filters": [
                {"name": "Red", "count": 10},
                {"name": "Blue", "count": 15}
            ]
        }"#;
        let plan: FlatPlan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.target_adu_fraction, 0.4);
        assert_eq!(plan.tolerance, 0.1);
        assert_eq!(plan.max_iterations, 5);
        assert_eq!(plan.initial_duration, Duration::from_millis(500));
        assert_eq!(plan.brightness, Some(80));
        assert_eq!(plan.filters.len(), 2);
    }

    #[test]
    fn load_config_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.json");
        std::fs::write(
            &path,
            r#"{
                "camera_id": "main-cam",
                "filter_wheel_id": "main-fw",
                "calibrator_id": "flat-panel",
                "filters": [{"name": "Luminance", "count": 20}]
            }"#,
        )
        .unwrap();

        let plan = load_config(&path).unwrap();
        assert_eq!(plan.camera_id, "main-cam");
        assert_eq!(plan.filters.len(), 1);
    }

    #[test]
    fn load_config_missing_file() {
        let err = load_config(Path::new("/nonexistent/calibrator-flats/plan.json")).unwrap_err();
        assert!(err.to_string().contains("failed to read config file"));
    }

    #[test]
    fn load_config_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.json");
        std::fs::write(&path, "not valid json").unwrap();

        let err = load_config(&path).unwrap_err();
        assert!(err.to_string().contains("failed to parse config file"));
    }

    #[test]
    fn flat_plan_rejects_unknown_field() {
        let json = r#"{
            "camera_id": "main-cam",
            "filter_wheel_id": "main-fw",
            "calibrator_id": "flat-panel",
            "filters": [{"name": "Luminance", "count": 20}],
            "dither_pixels": 5.0
        }"#;
        let err = serde_json::from_str::<FlatPlan>(json).unwrap_err();
        assert!(err.to_string().contains("dither_pixels"), "{err}");
    }

    #[test]
    fn filter_plan_rejects_unknown_field() {
        let json = r#"{"name": "Luminance", "count": 20, "binning": 2}"#;
        let err = serde_json::from_str::<FilterPlan>(json).unwrap_err();
        assert!(err.to_string().contains("binning"), "{err}");
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod doctor_toml_parity {
    use rusty_photon_server_config::doctor_toml::{parse, ServerClass};

    use super::default_server;

    /// `pkg/doctor.toml` is this service's catalog entry for
    /// `rusty-photon-doctor` and must match the config defaults
    /// (docs/services/doctor.md §The derived catalog).
    #[test]
    fn pkg_doctor_toml_matches_config_defaults() {
        let meta = parse(include_str!("../pkg/doctor.toml")).unwrap();
        assert_eq!(meta.port, default_server().port);
        assert_eq!(meta.class, ServerClass::Core);
        assert!(
            meta.config_gated,
            "calibrator-flats has no sensible default config"
        );
    }
}
