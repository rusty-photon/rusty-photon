//! The glue from the pinned `ascom-alpaca` crate's setup hooks to
//! `rusty-photon-setup`'s pages.
//!
//! The crate routes `GET` and `POST` on `/setup` to the [`ServerSetup`]
//! handler a `Server` carries and on
//! `/setup/v1/{device_type}/{device_number}/setup` to that device's
//! `setup_request`; both hand the request here, and the shared handler
//! renders, saves and reloads. A driver holds one `Arc<SetupPages<D>>` and
//! gives it to its server ([`ServerPages`]) and to each device
//! ([`device_setup_request`]).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ascom_alpaca::{ServerSetup, SetupRequest, SetupResponse};
use http::header::CONTENT_TYPE;
use rusty_photon_config::actions::ConfigurableDriver;
use rusty_photon_setup::{PageRequest, PageResponse, PageScope, SetupPages};

/// The crate's own boxed future shape (`futures::future::BoxFuture`), spelled
/// out so this crate needs no `futures` dependency.
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The crate's request as the shared handler takes it.
fn page_request(request: SetupRequest) -> PageRequest {
    PageRequest {
        method: request.method,
        headers: request.headers,
        query: request.query,
        form: request.form,
    }
}

/// The shared handler's answer as the crate sends it.
fn setup_response(response: PageResponse) -> SetupResponse {
    let is_html = response
        .headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"));
    let mut out = if is_html {
        SetupResponse::html(response.body).with_status(response.status)
    } else {
        SetupResponse::text(response.status, response.body)
    };
    for (name, value) in &response.headers {
        if name != CONTENT_TYPE {
            out = out.with_header(name.clone(), value.clone());
        }
    }
    out
}

/// A device's `setup_request`: answer the request for the page of the device
/// numbered `device_number` of `device_type` (in its Alpaca path spelling).
pub fn device_setup_request<D>(
    pages: &Arc<SetupPages<D>>,
    device_type: &'static str,
    device_number: u32,
    request: SetupRequest,
) -> BoxFuture<'static, SetupResponse>
where
    D: ConfigurableDriver + 'static,
    D::Overrides: Send + Sync,
{
    let pages = Arc::clone(pages);
    Box::pin(async move {
        let scope = PageScope::Device {
            device_type,
            device_number,
        };
        setup_response(pages.handle(scope, page_request(request)).await)
    })
}

/// The server's `/setup` handler: the shared pages' server page.
pub struct ServerPages<D: ConfigurableDriver>(pub Arc<SetupPages<D>>);

impl<D: ConfigurableDriver> fmt::Debug for ServerPages<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ServerPages").field(&self.0).finish()
    }
}

impl<D> ServerSetup for ServerPages<D>
where
    D: ConfigurableDriver + 'static,
    D::Overrides: Send + Sync,
{
    fn setup_request(&self, request: SetupRequest) -> BoxFuture<'_, SetupResponse> {
        Box::pin(async move {
            setup_response(
                self.0
                    .handle(PageScope::Server, page_request(request))
                    .await,
            )
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue, Method, StatusCode};

    #[test]
    fn an_html_answer_keeps_its_status_and_extra_headers() {
        let mut headers = HeaderMap::new();
        _ = headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        _ = headers.insert("refresh", HeaderValue::from_static("6"));
        let out = setup_response(PageResponse {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            headers,
            body: "<p>no</p>".to_string(),
        });
        assert_eq!(out.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(out.body(), "<p>no</p>");
        assert_eq!(out.headers().get("refresh").unwrap(), "6");
        assert_eq!(
            out.headers().get_all(CONTENT_TYPE).iter().count(),
            1,
            "{:?}",
            out.headers()
        );
        assert_eq!(
            out.headers().get(CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
    }

    #[test]
    fn a_text_answer_is_plain_text_under_its_status() {
        let out = setup_response(PageResponse::text(StatusCode::FORBIDDEN, "no"));
        assert_eq!(out.status(), StatusCode::FORBIDDEN);
        assert_eq!(out.body(), "no");
        assert_eq!(
            out.headers().get(CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
    }

    #[test]
    fn the_request_crosses_over_whole() {
        let mut headers = HeaderMap::new();
        _ = headers.insert("host", HeaderValue::from_static("h:1"));
        let request = SetupRequest {
            method: Method::POST,
            headers,
            query: vec![("a".to_string(), "b".to_string())],
            form: vec![("x".to_string(), "y".to_string())],
        };
        let page = page_request(request);
        assert_eq!(page.method, Method::POST);
        assert_eq!(page.headers.get("host").unwrap(), "h:1");
        assert_eq!(page.query, vec![("a".to_string(), "b".to_string())]);
        assert_eq!(page.form, vec![("x".to_string(), "y".to_string())]);
    }
}
