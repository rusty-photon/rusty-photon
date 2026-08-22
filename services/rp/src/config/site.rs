use rusty_photon_config::actions::FieldError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Observer site location.
///
/// Validated at config-load time: latitude must be in [-90, 90] and
/// longitude in [-180, 180]. The IANA timezone is derived from these
/// coordinates at startup via `rp-ephemeris`; elevation is intentionally
/// omitted (see `docs/services/rp.md` §"Site Configuration").
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SiteConfig {
    pub latitude_degrees: f64,
    pub longitude_degrees: f64,
}

impl SiteConfig {
    /// Range-validate the site as field-level errors (empty = valid).
    /// Shared by `load_config` (which aborts startup on the first error)
    /// and the REST `PUT /api/config` validation.
    #[must_use]
    pub fn field_errors(&self) -> Vec<FieldError> {
        let mut errors = Vec::new();
        if !(-90.0..=90.0).contains(&self.latitude_degrees) {
            errors.push(FieldError {
                path: "site.latitude_degrees".to_string(),
                msg: format!("must be in [-90, 90]; got {}", self.latitude_degrees),
            });
        }
        if !(-180.0..=180.0).contains(&self.longitude_degrees) {
            errors.push(FieldError {
                path: "site.longitude_degrees".to_string(),
                msg: format!("must be in [-180, 180]; got {}", self.longitude_degrees),
            });
        }
        errors
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use crate::config::load_config;
    use crate::config::test_support::MINIMAL_CONFIG_JSON;

    #[test]
    fn site_config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "site": {
                    "latitude_degrees": 47.6062,
                    "longitude_degrees": -122.3321
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        let site = config.site.unwrap();
        assert!((site.latitude_degrees - 47.6062).abs() < 1e-9);
        assert!((site.longitude_degrees - (-122.3321)).abs() < 1e-9);
    }

    #[test]
    fn site_config_omitted_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, MINIMAL_CONFIG_JSON).unwrap();
        let config = load_config(&path).unwrap();
        assert!(config.site.is_none());
    }

    #[test]
    fn site_config_rejects_latitude_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "site": {"latitude_degrees": 91.0, "longitude_degrees": 0.0},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("latitude_degrees") && msg.contains("[-90, 90]"),
            "expected latitude range diagnostic, got: {msg}"
        );
    }

    #[test]
    fn site_config_rejects_longitude_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "site": {"latitude_degrees": 0.0, "longitude_degrees": 181.0},
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("longitude_degrees") && msg.contains("[-180, 180]"),
            "expected longitude range diagnostic, got: {msg}"
        );
    }

    #[test]
    fn site_config_rejects_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "site": {
                    "latitude_degrees": 0.0,
                    "longitude_degrees": 0.0,
                    "elevation_meters": 1000
                },
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        // Elevation is explicitly out of v1 scope; surface a parse
        // error so an operator who's read the rp.md plan and added
        // an `elevation_meters` key gets a helpful failure rather
        // than silent ignoring.
        let err = load_config(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("elevation_meters") || msg.contains("unknown field"),
            "expected unknown-field diagnostic, got: {msg}"
        );
    }
}
