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
use crate::service::capture::check_image_dir;

/// Driver marker wiring phd2-guider's `Config` into the generic protocol.
pub struct Phd2GuiderDriver;

impl ConfigurableDriver for Phd2GuiderDriver {
    type Config = Config;
    /// `--host`/`--port` select the PHD2 to dial for the CLI and a
    /// config-less `serve`; a `serve` with a config file takes none.
    type Overrides = ();

    fn normalize(_config: &mut Config) {}

    /// The load-time rules, plus the startup check of `image_dir` (an
    /// existing directory this service can create and remove files in):
    /// with the `Restart` disposition an applied change is only tried at
    /// the next start, and a directory that fails the check would stop
    /// the whole guider then. This runs inside the service, so the probe
    /// file tests the service's own permissions.
    fn validate(config: &Config) -> Vec<FieldError> {
        let mut errors: Vec<FieldError> = camera_rule_violations(config)
            .into_iter()
            .map(|(path, msg)| FieldError { path, msg })
            .collect();
        if config.camera.enabled {
            if let Some(dir) = config.camera.image_dir.as_ref().filter(|d| d.is_absolute()) {
                if let Err(msg) = check_image_dir(dir) {
                    errors.push(FieldError {
                        path: "camera.image_dir".to_string(),
                        msg,
                    });
                }
            }
        }
        errors
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

    fn enabled_on(image_dir: &std::path::Path) -> Config {
        serde_json::from_value(serde_json::json!({
            "camera": {
                "enabled": true,
                "pixel_size_x_um": 2.9,
                "pixel_size_y_um": 2.9,
                "image_dir": image_dir,
            }
        }))
        .unwrap()
    }

    #[test]
    fn an_applied_image_dir_must_exist_as_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let paths = |config: &Config| -> Vec<String> {
            Phd2GuiderDriver::validate(config)
                .into_iter()
                .map(|e| e.path)
                .collect()
        };
        assert_eq!(paths(&enabled_on(dir.path())), Vec::<String>::new());
        assert_eq!(
            paths(&enabled_on(&dir.path().join("missing"))),
            ["camera.image_dir"]
        );
        let file = dir.path().join("plain");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(paths(&enabled_on(&file)), ["camera.image_dir"]);
    }

    #[cfg(unix)]
    #[test]
    fn an_applied_image_dir_this_service_cannot_write_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root writes through the mode bits, so there is nothing to test.
        if std::fs::write(locked.join("root-check"), b"").is_ok() {
            return;
        }
        let errors = Phd2GuiderDriver::validate(&enabled_on(&locked));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "camera.image_dir");
        assert!(errors[0].msg.contains("not writable"), "{errors:?}");
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
