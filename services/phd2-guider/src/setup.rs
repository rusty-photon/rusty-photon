//! phd2-guider's setup page, `/setup` on the service listener.
//!
//! The page covers the whole config, the camera block included
//! (`docs/services/phd2-guider.md` § "Setup page"), and the camera facade's
//! two setup paths send a browser there: the facade's Alpaca server listens
//! on loopback alone and exists only while `camera.enabled` is true, so a
//! page of its own would be out of the operator's reach. The service has no reload path, so the disposition is
//! `Restart`: a save is persisted and takes effect at the next start, as
//! `rp`'s does.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::{ServerSetup, SetupRequest, SetupResponse};
use axum::extract::{Query, State};
use axum::http::{header::HOST, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Form, Router};
use rusty_photon_config::actions::{ApplyDisposition, ConfigurableDriver, FieldError};
use rusty_photon_setup::{PageRequest, PageResponse, PageScope, SetupPages};

use crate::config::{camera_rule_violations, Config};
use crate::service::capture::check_image_dir;

/// Driver marker wiring phd2-guider's `Config` into the setup page.
pub struct Phd2GuiderDriver;

impl ConfigurableDriver for Phd2GuiderDriver {
    type Config = Config;
    /// `--host`/`--port` select the PHD2 to dial for the CLI and a
    /// config-less `serve`; a `serve` with a config file takes none.
    type Overrides = ();

    fn normalize(_config: &mut Config) {}

    /// The load-time rules, the page's own (a zero timeout or interval, a
    /// settle threshold that cannot be met, a PHD2 address that cannot be
    /// dialled), and the startup check of `image_dir` (an existing directory
    /// this service can create and remove files in): with the `Restart`
    /// disposition a save is only tried at the next start, and a directory
    /// that fails the check would stop the whole guider then. This runs
    /// inside the service, so the probe file tests the service's own
    /// permissions.
    fn validate(config: &Config) -> Vec<FieldError> {
        let mut errors: Vec<FieldError> = camera_rule_violations(config)
            .into_iter()
            .chain(page_rule_violations(config))
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

    fn apply_disposition() -> ApplyDisposition {
        ApplyDisposition::Restart
    }
}

/// The rules a save is held to beyond the load's: values the file parses
/// but the guider cannot work with, as `(dotted path, message)` pairs.
fn page_rule_violations(config: &Config) -> Vec<(String, String)> {
    let mut violations = Vec::new();
    let positive = [
        ("stop_timeout", config.stop_timeout),
        ("phd2.connection_timeout", config.phd2.connection_timeout),
        ("phd2.command_timeout", config.phd2.command_timeout),
        ("phd2.reconnect.interval", config.phd2.reconnect.interval),
        ("settling.timeout", config.settling.timeout),
    ];
    for (path, duration) in positive {
        if duration == Duration::ZERO {
            violations.push((path.to_string(), "must be greater than 0".to_string()));
        }
    }
    if !(config.settling.pixels.is_finite() && config.settling.pixels > 0.0) {
        violations.push((
            "settling.pixels".to_string(),
            "must be greater than 0".to_string(),
        ));
    }
    if config.phd2.host.trim().is_empty() {
        violations.push(("phd2.host".to_string(), "must not be empty".to_string()));
    }
    if config.phd2.port == 0 {
        violations.push((
            "phd2.port".to_string(),
            "must be greater than 0".to_string(),
        ));
    }
    violations
}

/// The pages of a running guider; `None` for one started without a config
/// file, which has nowhere to save.
pub type Pages = Option<Arc<SetupPages<Phd2GuiderDriver>>>;

/// The service listener's `/setup`, for `GET` and `POST`.
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
    answer(pages, request).await
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
    answer(pages, request).await
}

async fn answer(pages: Pages, request: PageRequest) -> Response {
    let Some(pages) = pages else {
        return (
            StatusCode::NOT_FOUND,
            "This guider runs on built-in defaults without a configuration file (started with \
             --host or --port and no file at the default path), so it has no setup page.",
        )
            .into_response();
    };
    let PageResponse {
        status,
        headers,
        body,
    } = pages.handle(PageScope::Server, request).await;
    (status, headers, body).into_response()
}

/// Where the camera facade's two setup paths send a browser: the guider's
/// own `/setup`, on the service listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuiderSetupLink {
    /// The service listener's bound address.
    pub addr: SocketAddr,
    /// Whether the service listener serves TLS.
    pub https: bool,
}

impl GuiderSetupLink {
    /// The page's URL for a browser that reached the facade with these
    /// headers: the host it used when the service listens on every
    /// interface, which the facade's own loopback host is one of, else the
    /// one address the service listens on.
    fn url(&self, headers: &HeaderMap) -> String {
        let scheme = if self.https { "https" } else { "http" };
        if self.addr.ip().is_unspecified() {
            let host = headers.get(HOST).and_then(|v| v.to_str().ok()).map_or_else(
                || "localhost".to_string(),
                rusty_photon_setup::origin::host_without_port,
            );
            format!("{scheme}://{host}:{}/setup", self.addr.port())
        } else {
            format!("{scheme}://{}/setup", self.addr)
        }
    }

    /// A `303 See Other` to the guider's page, for `GET` and `POST` alike:
    /// the facade has no form of its own.
    #[must_use]
    pub fn redirect(&self, request: &SetupRequest) -> SetupResponse {
        SetupResponse::see_other(&self.url(&request.headers))
    }
}

/// The facade server's `/setup`.
impl ServerSetup for GuiderSetupLink {
    fn setup_request(
        &self,
        request: SetupRequest,
    ) -> Pin<Box<dyn Future<Output = SetupResponse> + Send + '_>> {
        let response = self.redirect(&request);
        Box::pin(async move { response })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn paths(config: &Config) -> Vec<String> {
        Phd2GuiderDriver::validate(config)
            .into_iter()
            .map(|e| e.path)
            .collect()
    }

    #[test]
    fn an_enabled_facade_without_its_fields_fails_validation_by_field() {
        let config: Config = serde_json::from_str(r#"{"camera": {"enabled": true}}"#).unwrap();
        assert_eq!(
            paths(&config),
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
    fn a_saved_image_dir_must_exist_as_a_directory() {
        let dir = tempfile::tempdir().unwrap();
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
    fn a_saved_image_dir_this_service_cannot_write_is_refused() {
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
        assert_eq!(paths(&config), Vec::<String>::new());
    }

    #[test]
    fn a_zero_timeout_interval_or_threshold_is_refused_by_field() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "stop_timeout": "0s",
            "phd2": {
                "host": " ",
                "port": 0,
                "connection_timeout": "0s",
                "command_timeout": "0s",
                "reconnect": { "interval": "0s" }
            },
            "settling": { "pixels": 0.0, "time": "0s", "timeout": "0s" }
        }))
        .unwrap();
        assert_eq!(
            paths(&config),
            [
                "stop_timeout",
                "phd2.connection_timeout",
                "phd2.command_timeout",
                "phd2.reconnect.interval",
                "settling.timeout",
                "settling.pixels",
                "phd2.host",
                "phd2.port"
            ]
        );
    }

    #[test]
    fn the_defaults_validate_clean() {
        assert_eq!(paths(&Config::default()), Vec::<String>::new());
    }

    #[test]
    fn changes_apply_at_the_next_start() {
        assert!(matches!(
            Phd2GuiderDriver::apply_disposition(),
            ApplyDisposition::Restart
        ));
    }

    fn request_with_host(host: Option<&'static str>) -> SetupRequest {
        let mut headers = HeaderMap::new();
        if let Some(host) = host {
            _ = headers.insert(HOST, HeaderValue::from_static(host));
        }
        SetupRequest {
            method: Method::GET,
            headers,
            query: Vec::new(),
            form: Vec::new(),
        }
    }

    fn location(link: GuiderSetupLink, host: Option<&'static str>) -> String {
        let response = link.redirect(&request_with_host(host));
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        response
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn a_service_on_every_interface_is_reached_by_the_facades_host() {
        let link = GuiderSetupLink {
            addr: "0.0.0.0:11130".parse().unwrap(),
            https: false,
        };
        assert_eq!(
            location(link, Some("127.0.0.1:11128")),
            "http://127.0.0.1:11130/setup"
        );
        assert_eq!(
            location(link, Some("[::1]:11128")),
            "http://[::1]:11130/setup"
        );
        assert_eq!(location(link, None), "http://localhost:11130/setup");
    }

    #[test]
    fn a_service_on_one_address_is_reached_there_over_its_scheme() {
        let link = GuiderSetupLink {
            addr: "[::1]:11130".parse().unwrap(),
            https: true,
        };
        assert_eq!(
            location(link, Some("localhost:11128")),
            "https://[::1]:11130/setup"
        );
    }

    #[tokio::test]
    async fn the_facade_servers_page_sends_the_browser_to_the_guiders() {
        let link = GuiderSetupLink {
            addr: "192.0.2.10:11130".parse().unwrap(),
            https: false,
        };
        let mut post = request_with_host(Some("127.0.0.1:11128"));
        post.method = Method::POST;
        let response = ServerSetup::setup_request(&link, post).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .unwrap(),
            "http://192.0.2.10:11130/setup"
        );
    }
}
