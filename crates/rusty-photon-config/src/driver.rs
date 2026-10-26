//! What a service tells its setup pages about its configuration.
//!
//! A service's setup pages (`rusty-photon-setup`) render, validate and save
//! its configuration file. This module holds the per-service half: the
//! [`ConfigurableDriver`] trait, which each service implements for its own
//! `Config` type, the [`FieldError`] its validation reports, and the
//! [`DeviceSection`]s its device pages edit. See
//! [`docs/crates/rusty-photon-setup.md`].
//!
//! [`docs/crates/rusty-photon-setup.md`]: ../../../docs/crates/rusty-photon-setup.md

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// How a service's saved configuration takes effect: by an in-process reload
/// (the default) or only on the next process start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveDisposition {
    /// The service reloads in process (`ServiceRunner::run_with_reload`): a
    /// save fires its `ReloadSignal` after the response flushes.
    Reload,
    /// The service has no in-process reload (rp, which runs under
    /// `ServiceRunner::run`): a save takes effect on the next process start,
    /// and the page names the saved fields still waiting for it.
    Restart,
}

/// Per-service configuration metadata.
///
/// The setup pages hold every rule that is the same for all services; an
/// implementor supplies only what varies: its `Config` type, validation,
/// secrets, the fields its command-line flags pin, and its device sections.
pub trait ConfigurableDriver {
    /// The service's config type, whose JSON Schema the pages render from.
    type Config: Serialize + DeserializeOwned + JsonSchema + Send + Sync;
    /// The service's command-line override carrier (`()` if it has no
    /// overrides). Bound `Sync` so the pages' futures, which borrow it,
    /// stay `Send`.
    type Overrides: Sync;

    /// Trim or canonicalize a submitted config in place before validation and
    /// persist (e.g. trim a whitespace-padded `serial.port`).
    fn normalize(config: &mut Self::Config);

    /// Domain validation. An empty result means valid. Paths are dotted
    /// (`"serial.port"`) so the page renders each error beside its field.
    fn validate(config: &Self::Config) -> Vec<FieldError>;

    /// RFC-6901 JSON pointers to secret leaves. The page renders each as a
    /// password input that never carries the stored value, and a blank one
    /// keeps it. Empty slice if the service stores no secrets.
    ///
    /// A pointer segment may be the wildcard `*`, matching every element of
    /// the array (or every value of the object) at that position — e.g.
    /// `/equipment/cameras/*/auth/password` names that leaf in every camera
    /// entry.
    fn secret_pointers() -> &'static [&'static str];

    /// Dotted paths currently pinned by a command-line override: shown
    /// disabled on the page and never persisted by a save, so a transient
    /// override is never baked into the file.
    fn override_paths(overrides: &Self::Overrides) -> Vec<String>;

    /// Apply command-line overrides onto a config in place, producing the
    /// *effective* config a reloaded server would run.
    fn apply_overrides(config: &mut Self::Config, overrides: &Self::Overrides);

    /// The dotted paths the command-line overrides pin, read against
    /// `config` for a service whose flag lands on one field or another by
    /// the config's shape — the `GTi`'s `--port` is the serial path on a USB
    /// transport and the host on a UDP one. The setup pages show these fields
    /// disabled and never persist them. Defaults to
    /// [`override_paths`](Self::override_paths), which every other service's
    /// flags fit.
    #[must_use]
    fn pinned_paths(config: &Self::Config, overrides: &Self::Overrides) -> Vec<String> {
        let _ = config;
        Self::override_paths(overrides)
    }

    /// RFC-6901 JSON pointers to fields a setup page must not edit. Fields
    /// the service writes at runtime — a mount's park position, captured by
    /// `SetPark`, and its AP-park choices, set by their actions — since a
    /// form filled before the runtime write would otherwise carry the old
    /// value back on its next save; and trust settings a page must not
    /// choose — rp's `ca_cert`, the CA it trusts for every credential it
    /// presents. A setup page names such a field as edited in the
    /// configuration file instead of rendering an input for it. A pointer
    /// segment may be the wildcard `*`. Empty for a service with none.
    #[must_use]
    fn file_only_pointers() -> &'static [&'static str] {
        &[]
    }

    /// How this service's saved changes take effect. The default —
    /// [`SaveDisposition::Reload`] — matches the Alpaca drivers, which run
    /// under `ServiceRunner::run_with_reload`; services with no in-process
    /// reload (rp, phd2-guider, ui-htmx) override to [`SaveDisposition::Restart`].
    #[must_use]
    fn save_disposition() -> SaveDisposition {
        SaveDisposition::Reload
    }

    /// The config section each served device's setup page edits — one
    /// [`DeviceSection`] per device, read from `config` because the devices a
    /// service serves can depend on it (a camera driver's `usb_devices`
    /// list). The server setup page holds every other section. Empty for a
    /// service with no device pages of its own.
    #[must_use]
    fn device_sections(config: &Self::Config) -> Vec<DeviceSection> {
        let _ = config;
        Vec::new()
    }
}

/// One served device and the config section its setup page edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSection {
    /// The device type in its Alpaca path spelling (`covercalibrator`).
    pub device_type: String,
    /// The device number on this server.
    pub device_number: u32,
    /// RFC-6901 pointer to the top-level section (`/cover_calibrator`).
    pub pointer: String,
    /// The device's name, for the server page's device list.
    pub name: String,
    /// Whether the section is enabled: a disabled device is listed but not
    /// served, so it has no page.
    pub enabled: bool,
}

impl DeviceSection {
    /// The section's top-level key (`cover_calibrator`).
    #[must_use]
    pub fn section(&self) -> &str {
        self.pointer.trim_start_matches('/')
    }
}

/// A single field-level validation error: the dotted path of the field and
/// what is wrong with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldError {
    pub path: String,
    pub msg: String,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, JsonSchema)]
    struct TestConfig {
        port: String,
    }

    /// A service that overrides none of the trait's defaults.
    struct TestDriver;

    impl ConfigurableDriver for TestDriver {
        type Config = TestConfig;
        type Overrides = Option<String>;

        fn normalize(_config: &mut TestConfig) {}

        fn validate(_config: &TestConfig) -> Vec<FieldError> {
            Vec::new()
        }

        fn secret_pointers() -> &'static [&'static str] {
            &[]
        }

        fn override_paths(overrides: &Option<String>) -> Vec<String> {
            overrides.iter().map(|_| "port".to_string()).collect()
        }

        fn apply_overrides(config: &mut TestConfig, overrides: &Option<String>) {
            if let Some(port) = overrides {
                config.port.clone_from(port);
            }
        }
    }

    fn config() -> TestConfig {
        TestConfig {
            port: "/dev/ttyACM0".to_string(),
        }
    }

    #[test]
    fn the_default_disposition_is_reload() {
        assert_eq!(TestDriver::save_disposition(), SaveDisposition::Reload);
    }

    #[test]
    fn pinned_paths_default_to_the_override_paths() {
        assert_eq!(
            TestDriver::pinned_paths(&config(), &Some("/dev/ttyUSB0".to_string())),
            vec!["port".to_string()]
        );
        assert_eq!(
            TestDriver::pinned_paths(&config(), &None),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_service_has_no_file_only_fields_or_device_sections_by_default() {
        assert_eq!(TestDriver::file_only_pointers(), Vec::<&str>::new());
        assert_eq!(TestDriver::device_sections(&config()), Vec::new());
    }

    #[test]
    fn a_device_section_names_its_top_level_key() {
        let section = DeviceSection {
            device_type: "covercalibrator".to_string(),
            device_number: 0,
            pointer: "/cover_calibrator".to_string(),
            name: "Flat panel".to_string(),
            enabled: true,
        };
        assert_eq!(section.section(), "cover_calibrator");
    }
}
