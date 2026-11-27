//! The guider's setup page, `/setup` (docs/services/guider.md § Setup page).
//!
//! The shared crate's server page over the guider's configuration. The
//! guider serves no Alpaca device, so it has no device pages, and it has no
//! in-process reload, so a save takes effect at the next start
//! (`SaveDisposition::Restart`). Its own part of the page is a note that a
//! restart ends a guiding run.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Form, Router};
use maud::{html, Markup};
use rusty_photon_config::driver::{ConfigurableDriver, FieldError, SaveDisposition};
use rusty_photon_setup::{
    BoxFuture, CustomSection, DriverPages, FormValues, PageRequest, PageResponse, PageScope,
    SectionView, SetupPages,
};
use serde_json::Value;

use crate::config::{CliOverrides, Config};
use crate::error::GuiderError;

/// Driver marker wiring the guider's `Config` into the setup page.
pub struct GuiderDriver;

impl ConfigurableDriver for GuiderDriver {
    type Config = Config;
    type Overrides = CliOverrides;

    fn normalize(_config: &mut Config) {}

    /// What the next start checks on disk: a `ca_cert` that cannot be read
    /// or holds no certificate, and a state directory that cannot be
    /// created or written. The state directory is created, as the next
    /// start would create it. Every other rule is the configuration's own,
    /// held as it is read.
    fn validate(config: &Config) -> Vec<FieldError> {
        let mut errors = Vec::new();
        if let Some(path) = config.ca_cert_path() {
            if let Err(error) = crate::check_ca_cert(path) {
                errors.push(field_error("ca_cert", error));
            }
        }
        if let Err(error) = config
            .state_dir()
            .and_then(|dir| crate::prepare_state_dir(&dir))
        {
            errors.push(field_error("state_dir", error));
        }
        errors
    }

    /// The server-auth password hash, which the Security section edits and
    /// never shows, and the passwords the guider presents to its devices'
    /// servers.
    fn secret_pointers() -> &'static [&'static str] {
        &[
            "/server/auth/password_hash",
            "/devices/camera/auth/password",
            "/devices/mount/auth/password",
        ]
    }

    fn override_paths(overrides: &CliOverrides) -> Vec<String> {
        let mut paths = Vec::new();
        if overrides.port.is_some() {
            paths.push("server.port".to_owned());
        }
        if overrides.bind_address.is_some() {
            paths.push("server.bind_address".to_owned());
        }
        paths
    }

    fn apply_overrides(config: &mut Config, overrides: &CliOverrides) {
        overrides.apply(config);
    }

    fn save_disposition() -> SaveDisposition {
        SaveDisposition::Restart
    }
}

/// `error` beside the field at `path`: the message alone, since the page
/// names the field.
fn field_error(path: &str, error: GuiderError) -> FieldError {
    let msg = match error {
        GuiderError::CaCert { message, .. } | GuiderError::StateDir { message, .. } => message,
        other => other.to_string(),
    };
    FieldError {
        path: path.to_owned(),
        msg,
    }
}

/// The page's handler, shared by its routes.
pub type Pages = Arc<SetupPages<GuiderDriver>>;

/// The page over `config`, as the guider runs it with `overrides`, saved to
/// the file at `path`.
///
/// Each device's credential is bound to that device's address and to the
/// CA that verifies its server: a save that names another server, or
/// trusts another CA for it, asks before it removes the credential, so the
/// page cannot send a server's password to whatever address is typed.
#[must_use]
pub fn pages(config: &Config, path: PathBuf, overrides: CliOverrides) -> Pages {
    Arc::new(
        SetupPages::new("guider", config.clone(), path, overrides)
            .with_https(config.server.tls.is_some())
            .with_bound_credential("/devices/camera/alpaca_url", "/devices/camera/auth")
            .with_bound_credential("/ca_cert", "/devices/camera/auth")
            .with_bound_credential("/devices/mount/alpaca_url", "/devices/mount/auth")
            .with_bound_credential("/ca_cert", "/devices/mount/auth")
            .with_driver_pages(Arc::new(RestartNote)),
    )
}

/// `/setup`, for `GET` and `POST`.
pub fn router(pages: Pages) -> Router {
    Router::new()
        .route("/setup", get(show).post(save))
        .with_state(pages)
}

async fn show(
    State(pages): State<Pages>,
    headers: HeaderMap,
    Query(query): Query<Vec<(String, String)>>,
) -> Response {
    let request = PageRequest {
        method: Method::GET,
        headers,
        query,
        form: Vec::new(),
    };
    answer(&pages, request).await
}

async fn save(
    State(pages): State<Pages>,
    headers: HeaderMap,
    Query(query): Query<Vec<(String, String)>>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let request = PageRequest {
        method: Method::POST,
        headers,
        query,
        form,
    };
    answer(&pages, request).await
}

async fn answer(pages: &Pages, request: PageRequest) -> Response {
    let PageResponse {
        status,
        headers,
        body,
    } = pages.handle(PageScope::Server, request).await;
    (status, headers, body).into_response()
}

/// The guider's own part of the page: the note that a restart, which every
/// save waits for, ends a guiding run.
struct RestartNote;

impl DriverPages for RestartNote {
    fn owned_keys(&self) -> Vec<String> {
        Vec::new()
    }

    fn sections<'a>(
        &'a self,
        scope: PageScope<'a>,
        _saving: bool,
    ) -> BoxFuture<'a, Vec<Box<dyn CustomSection>>> {
        let sections: Vec<Box<dyn CustomSection>> = match scope {
            PageScope::Server => vec![Box::new(RestartNoteSection)],
            PageScope::Device { .. } => Vec::new(),
        };
        Box::pin(async move { sections })
    }
}

struct RestartNoteSection;

impl CustomSection for RestartNoteSection {
    fn render(&self, _view: SectionView<'_>) -> Markup {
        html! {
            p.note id="restart-ends-run" {
                "A restart of the guider ends a guiding run, so restart it between runs."
            }
        }
    }

    fn has_controls(&self) -> bool {
        false
    }

    fn apply(&self, _form: &FormValues, _config: &mut Value) -> Vec<FieldError> {
        Vec::new()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use serde_json::json;

    use super::*;

    fn config(value: serde_json::Value) -> Config {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_state_directory_it_can_write_and_no_ca_cert_are_valid() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(json!({ "state_dir": dir.path().join("state") }));
        assert_eq!(GuiderDriver::validate(&config), Vec::new());
        assert!(dir.path().join("state").is_dir());
    }

    #[test]
    fn a_state_directory_under_a_file_is_refused_beside_its_field() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        let errors = GuiderDriver::validate(&config(json!({ "state_dir": file.join("state") })));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "state_dir");
        assert!(errors[0].msg.starts_with("cannot be created"), "{errors:?}");
    }

    #[test]
    fn a_ca_cert_that_cannot_be_read_is_refused_beside_its_field() {
        let dir = tempfile::tempdir().unwrap();
        let errors = GuiderDriver::validate(&config(json!({
            "state_dir": dir.path().join("state"),
            "ca_cert": dir.path().join("missing.pem"),
        })));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "ca_cert");
        assert!(errors[0].msg.starts_with("cannot be read"), "{errors:?}");
    }

    #[test]
    fn a_ca_cert_holding_a_certificate_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        rusty_photon_tls::test_cert::generate_ca(dir.path()).unwrap();
        let config = config(json!({
            "state_dir": dir.path().join("state"),
            "ca_cert": rusty_photon_tls::config::ca_cert_path(dir.path()),
        }));
        assert_eq!(GuiderDriver::validate(&config), Vec::new());
    }

    #[test]
    fn the_flags_pin_the_port_and_the_bind_address() {
        let overrides = CliOverrides {
            port: Some(9000),
            bind_address: Some("127.0.0.1".parse().unwrap()),
        };
        assert_eq!(
            GuiderDriver::override_paths(&overrides),
            vec!["server.port", "server.bind_address"]
        );
        let mut config = Config::default();
        GuiderDriver::apply_overrides(&mut config, &overrides);
        assert_eq!(config.server.port, 9000);
        assert_eq!(config.server.bind_address.to_string(), "127.0.0.1");
        assert_eq!(
            GuiderDriver::override_paths(&CliOverrides::default()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_save_waits_for_the_restart() {
        assert_eq!(GuiderDriver::save_disposition(), SaveDisposition::Restart);
    }

    #[tokio::test]
    async fn the_note_is_on_the_server_page_and_no_device_page() {
        let note = RestartNote;
        assert_eq!(note.owned_keys(), Vec::<String>::new());
        let server = note.sections(PageScope::Server, false).await;
        assert_eq!(server.len(), 1);
        assert!(!server[0].has_controls());
        let view = SectionView {
            config: &Value::Null,
            submitted: None,
            errors: &[],
            host: None,
        };
        let markup = server[0].render(view).into_string();
        assert!(markup.contains("ends a guiding run"), "{markup}");
        let mut file = json!({});
        assert_eq!(
            server[0].apply(&FormValues::default(), &mut file),
            Vec::new()
        );
        assert_eq!(file, json!({}));
        let device = note
            .sections(
                PageScope::Device {
                    device_type: "camera",
                    device_number: 0,
                },
                false,
            )
            .await;
        assert_eq!(device.len(), 0);
    }
}
