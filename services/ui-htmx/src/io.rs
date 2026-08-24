//! HTTP client abstraction for testability.
//!
//! Mirrors `sentinel`'s `io.rs`: a small `HttpClient` trait (mocked with
//! `mockall` in tests) plus a `reqwest`-backed production implementation built
//! through `rusty-photon-tls` so it trusts the Rusty Photon CA and can present HTTP Basic
//! credentials to an auth-enabled driver.

use std::path::Path;

use async_trait::async_trait;

/// A failure issuing an HTTP request to a driver.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct HttpError(pub String);

/// HTTP response from a driver request.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// Abstraction over the HTTP client, so the config-action layer can be unit
/// tested without a live driver.
#[async_trait]
#[cfg_attr(test, mockall::automock)]
pub trait HttpClient: Send + Sync {
    /// Send a GET request to `url`.
    async fn get(&self, url: &str) -> Result<HttpResponse, HttpError>;

    /// Send a PUT request with a form-encoded body. ASCOM `action` calls are
    /// `PUT` with `Action` / `Parameters` form fields.
    async fn put_form(&self, url: &str, params: &[(&str, &str)])
        -> Result<HttpResponse, HttpError>;

    /// Send a PUT request with a JSON body. rp's REST config endpoint
    /// (`PUT /api/config`) takes the full Config JSON directly.
    async fn put_json(&self, url: &str, body: &str) -> Result<HttpResponse, HttpError>;

    /// Send a POST request with an empty body. Sentinel's REST restart
    /// endpoint (`POST /api/services/{name}/restart`) takes no body.
    async fn post(&self, url: &str) -> Result<HttpResponse, HttpError>;
}

/// Production HTTP client using `reqwest`, with optional CA trust and Basic auth.
pub struct ReqwestHttpClient {
    client: reqwest::Client,
    auth: Option<(String, String)>,
}

impl ReqwestHttpClient {
    /// Build a client that trusts the Rusty Photon CA at `ca_cert_path` (when
    /// `Some`), with no credentials.
    ///
    /// # Errors
    ///
    /// Returns an [`HttpError`] if the reqwest client cannot be built
    /// (a missing, unreadable, or non-PEM CA file).
    pub fn new(ca_cert_path: Option<&Path>) -> Result<Self, HttpError> {
        let client = rusty_photon_tls::client::build_reqwest_client(ca_cert_path)
            .map_err(|e| HttpError(format!("failed to build HTTP client: {e}")))?;
        Ok(Self { client, auth: None })
    }

    /// Build a client with CA trust and HTTP Basic credentials, sent on every
    /// request so the BFF can talk to an auth-enabled driver.
    ///
    /// # Errors
    ///
    /// Returns [`Self::new`]'s failure — an [`HttpError`] if the
    /// underlying client cannot be built.
    pub fn with_auth(
        ca_cert_path: Option<&Path>,
        username: String,
        password: String,
    ) -> Result<Self, HttpError> {
        let mut client = Self::new(ca_cert_path)?;
        client.auth = Some((username, password));
        Ok(client)
    }
}

#[async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn get(&self, url: &str) -> Result<HttpResponse, HttpError> {
        tracing::debug!("GET {url}");
        // Don't reuse connections: a driver applies config by reloading
        // (tearing its server down and rebinding), which leaves any pooled
        // keep-alive connection stale. A fresh connection per request lets the
        // reconnect poll recover the moment the driver is back. Config actions
        // are low-frequency, so the lost pooling is immaterial.
        let mut request = self
            .client
            .get(url)
            .header(reqwest::header::CONNECTION, "close");
        if let Some((user, pass)) = &self.auth {
            request = request.basic_auth(user, Some(pass));
        }
        let response = request
            .send()
            .await
            .map_err(|e| HttpError(format!("GET {url} failed: {e}")))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| HttpError(format!("reading response body: {e}")))?;
        tracing::debug!("GET {url} -> {status} ({} bytes)", body.len());
        Ok(HttpResponse { status, body })
    }

    async fn put_form(
        &self,
        url: &str,
        params: &[(&str, &str)],
    ) -> Result<HttpResponse, HttpError> {
        tracing::debug!("PUT {url}");
        // `Connection: close` for the same reason as `get` — avoid a stale
        // pooled connection across a driver reload.
        let mut request = self
            .client
            .put(url)
            .form(params)
            .header(reqwest::header::CONNECTION, "close");
        if let Some((user, pass)) = &self.auth {
            request = request.basic_auth(user, Some(pass));
        }
        let response = request
            .send()
            .await
            .map_err(|e| HttpError(format!("PUT {url} failed: {e}")))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| HttpError(format!("reading response body: {e}")))?;
        tracing::debug!("PUT {url} -> {status} ({} bytes)", body.len());
        Ok(HttpResponse { status, body })
    }

    async fn put_json(&self, url: &str, body: &str) -> Result<HttpResponse, HttpError> {
        tracing::debug!("PUT {url} (json)");
        // `Connection: close` for the same reason as `get` — a fresh connection
        // per request survives the target restarting between calls.
        let mut request = self
            .client
            .put(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::CONNECTION, "close")
            .body(body.to_string());
        if let Some((user, pass)) = &self.auth {
            request = request.basic_auth(user, Some(pass));
        }
        let response = request
            .send()
            .await
            .map_err(|e| HttpError(format!("PUT {url} failed: {e}")))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| HttpError(format!("reading response body: {e}")))?;
        tracing::debug!("PUT {url} -> {status} ({} bytes)", body.len());
        Ok(HttpResponse { status, body })
    }

    async fn post(&self, url: &str) -> Result<HttpResponse, HttpError> {
        tracing::debug!("POST {url}");
        // `Connection: close` for the same reason as `get` — a restart tears
        // processes down, so a pooled connection would go stale.
        let mut request = self
            .client
            .post(url)
            .header(reqwest::header::CONNECTION, "close");
        if let Some((user, pass)) = &self.auth {
            request = request.basic_auth(user, Some(pass));
        }
        let response = request
            .send()
            .await
            .map_err(|e| HttpError(format!("POST {url} failed: {e}")))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| HttpError(format!("reading response body: {e}")))?;
        tracing::debug!("POST {url} -> {status} ({} bytes)", body.len());
        Ok(HttpResponse { status, body })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// Port 1 is reserved and unbound, so connections are always refused.
    const UNREACHABLE_URL: &str = "http://127.0.0.1:1/api/v1/covercalibrator/0/action";

    #[tokio::test]
    async fn get_connection_refused_is_an_error() {
        let client = ReqwestHttpClient::new(None).unwrap();
        let err = client.get(UNREACHABLE_URL).await.unwrap_err();
        assert!(err.0.starts_with("GET "), "{}", err.0);
    }

    #[tokio::test]
    async fn put_form_connection_refused_is_an_error() {
        let client = ReqwestHttpClient::new(None).unwrap();
        let err = client
            .put_form(UNREACHABLE_URL, &[("Action", "config.get")])
            .await
            .unwrap_err();
        assert!(err.0.starts_with("PUT "), "{}", err.0);
    }

    #[tokio::test]
    async fn put_json_connection_refused_is_an_error() {
        let client = ReqwestHttpClient::new(None).unwrap();
        let err = client.put_json(UNREACHABLE_URL, "{}").await.unwrap_err();
        assert!(err.0.starts_with("PUT "), "{}", err.0);
    }

    #[tokio::test]
    async fn post_connection_refused_is_an_error() {
        let client = ReqwestHttpClient::new(None).unwrap();
        let err = client.post(UNREACHABLE_URL).await.unwrap_err();
        assert!(err.0.starts_with("POST "), "{}", err.0);
    }
}
