//! The BFF's own setup page, `/setup` (`docs/services/ui-htmx.md` § "Setup
//! page").
//!
//! The shared crate's server page over this service's configuration: the
//! listening server, and where rp is. The BFF is not an Alpaca device, so it
//! has no device pages, and it has no in-process reload, so a save takes
//! effect at the next start (`ApplyDisposition::Restart`), as rp's does.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Form, Router};
use rusty_photon_config::actions::{ApplyDisposition, ConfigurableDriver, FieldError};
use rusty_photon_setup::{PageRequest, PageResponse, PageScope, SetupPages};

use crate::config::{CliOverrides, Config};

/// Driver marker wiring the BFF's `Config` into the setup page.
pub struct UiHtmxDriver;

impl ConfigurableDriver for UiHtmxDriver {
    type Config = Config;
    type Overrides = CliOverrides;

    /// Trim surrounding whitespace from rp's address, which is otherwise
    /// joined verbatim into every URL the BFF requests.
    fn normalize(config: &mut Config) {
        let trimmed = config.rp.base_url.trim();
        if trimmed.len() != config.rp.base_url.len() {
            config.rp.base_url = trimmed.to_string();
        }
    }

    /// rp's address must be an `http` or `https` URL that carries no
    /// credentials, the rule the BFF's start holds it to: a credential in
    /// the URL would reach error messages and logs that echo it.
    fn validate(config: &Config) -> Vec<FieldError> {
        let error = |msg: &str| {
            vec![FieldError {
                path: "rp.base_url".to_string(),
                msg: msg.to_string(),
            }]
        };
        let Ok(url) = reqwest::Url::parse(&config.rp.base_url) else {
            return error("must be a URL, such as http://127.0.0.1:11115");
        };
        if !matches!(url.scheme(), "http" | "https") {
            return error("must be an http or https URL");
        }
        if !url.username().is_empty() || url.password().is_some() {
            return error("must not carry credentials; set rp.auth instead");
        }
        Vec::new()
    }

    /// The server-auth password hash, which the Security section edits and
    /// never shows, and the plaintext password the BFF presents to rp.
    fn secret_pointers() -> &'static [&'static str] {
        &["/server/auth/password_hash", "/rp/auth/password"]
    }

    fn override_paths(overrides: &CliOverrides) -> Vec<String> {
        overrides
            .port
            .map(|_| "server.port".to_string())
            .into_iter()
            .collect()
    }

    fn apply_overrides(config: &mut Config, overrides: &CliOverrides) {
        if let Some(port) = overrides.port {
            config.server.port = port;
        }
    }

    fn apply_disposition() -> ApplyDisposition {
        ApplyDisposition::Restart
    }
}

/// The page's handler, shared by its routes.
pub type Pages = Arc<SetupPages<UiHtmxDriver>>;

/// The page over `config`, as the BFF runs it with `overrides`, saved to the
/// file at `path`.
#[must_use]
pub fn pages(config: &Config, path: PathBuf, overrides: CliOverrides) -> Pages {
    Arc::new(
        SetupPages::new("ui-htmx", config.clone(), path, overrides)
            .with_https(config.server.tls.is_some()),
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn with_rp(base_url: &str) -> Config {
        let mut config = Config::default();
        config.rp.base_url = base_url.to_string();
        config
    }

    #[test]
    fn an_http_or_https_address_for_rp_is_valid() {
        assert_eq!(
            UiHtmxDriver::validate(&with_rp("http://127.0.0.1:11115")),
            Vec::new()
        );
        assert_eq!(
            UiHtmxDriver::validate(&with_rp("https://rig.local:11115")),
            Vec::new()
        );
    }

    #[test]
    fn an_address_for_rp_that_is_not_a_url_is_refused() {
        let errors = UiHtmxDriver::validate(&with_rp("rig.local:11115"));
        assert_eq!(errors[0].path, "rp.base_url");
    }

    #[test]
    fn an_address_for_rp_carrying_credentials_is_refused() {
        let errors = UiHtmxDriver::validate(&with_rp("http://obs:secret@127.0.0.1:11115"));
        assert_eq!(errors[0].path, "rp.base_url");
        assert!(errors[0].msg.contains("credentials"), "{errors:?}");
    }

    #[test]
    fn a_non_http_address_for_rp_is_refused() {
        let errors = UiHtmxDriver::validate(&with_rp("ftp://127.0.0.1:11115"));
        assert!(errors[0].msg.contains("http"), "{errors:?}");
    }

    #[test]
    fn normalize_trims_rps_address() {
        let mut config = with_rp("  http://127.0.0.1:11115 ");
        UiHtmxDriver::normalize(&mut config);
        assert_eq!(config.rp.base_url, "http://127.0.0.1:11115");
    }

    #[test]
    fn the_port_flag_pins_server_port() {
        let overrides = CliOverrides { port: Some(9000) };
        assert_eq!(
            UiHtmxDriver::override_paths(&overrides),
            vec!["server.port"]
        );
        let mut config = Config::default();
        UiHtmxDriver::apply_overrides(&mut config, &overrides);
        assert_eq!(config.server.port, 9000);
        assert_eq!(
            UiHtmxDriver::override_paths(&CliOverrides::default()),
            Vec::<String>::new()
        );
    }
}
