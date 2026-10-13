//! phd2-guider's [`ConfigurableDriver`] implementation, served by the
//! camera facade device (`docs/services/phd2-guider.md` § "Config
//! actions").
//!
//! The actions cover the whole of phd2-guider's config, not only the
//! `camera` block. The service has no reload path, so the disposition
//! is `Restart`: a change is persisted and takes effect at the next
//! start, as `rp`'s does.

use rusty_photon_config::actions::{ApplyDisposition, ConfigurableDriver, FieldError};

use crate::config::{camera_rule_violations, Config};

/// Driver marker wiring phd2-guider's `Config` into the generic protocol.
pub struct Phd2GuiderDriver;

impl ConfigurableDriver for Phd2GuiderDriver {
    type Config = Config;
    /// `--host`/`--port` select the PHD2 to dial for the CLI and a
    /// config-less `serve`; a `serve` with a config file takes none.
    type Overrides = ();

    fn normalize(_config: &mut Config) {}

    fn validate(config: &Config) -> Vec<FieldError> {
        camera_rule_violations(config)
            .into_iter()
            .map(|(path, msg)| FieldError { path, msg })
            .collect()
    }

    fn secret_pointers() -> &'static [&'static str] {
        &[
            "/server/auth/password_hash",
            "/camera/server/auth/password_hash",
        ]
    }

    fn override_paths(_overrides: &()) -> Vec<String> {
        Vec::new()
    }

    fn apply_overrides(_config: &mut Config, _overrides: &()) {}

    fn locked_paths() -> &'static [&'static str] {
        &["camera.unique_id"]
    }

    /// The listeners: changing one would cut the client off the
    /// endpoint it is editing through.
    fn read_only_paths() -> &'static [&'static str] {
        &[
            "server.port",
            "camera.enabled",
            "camera.server.port",
            "camera.server.bind_address",
        ]
    }

    fn apply_disposition() -> ApplyDisposition {
        ApplyDisposition::Restart
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn an_enabled_facade_without_its_fields_fails_validation_by_field() {
        let config: Config = serde_json::from_str(r#"{"camera": {"enabled": true}}"#).unwrap();
        let paths: Vec<String> = Phd2GuiderDriver::validate(&config)
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(
            paths,
            [
                "camera.pixel_size_x_um",
                "camera.pixel_size_y_um",
                "camera.image_dir"
            ]
        );
    }

    #[test]
    fn a_disabled_facade_validates_clean() {
        let config: Config = serde_json::from_str(r#"{"camera": {"enabled": false}}"#).unwrap();
        let paths: Vec<String> = Phd2GuiderDriver::validate(&config)
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(paths, Vec::<String>::new());
    }

    #[test]
    fn changes_apply_at_the_next_start() {
        assert!(matches!(
            Phd2GuiderDriver::apply_disposition(),
            ApplyDisposition::Restart
        ));
    }
}
