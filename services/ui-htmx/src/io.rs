//! HTTP client abstraction for testability.
//!
//! Mirrors `sentinel`'s `io.rs`: a small `HttpClient` trait (mocked with
//! `mockall` in tests) plus a `reqwest`-backed production implementation built
//! through `rusty-photon-tls` so it trusts the Rusty Photon CA and can present HTTP Basic
//! credentials to an auth-enabled rp.

use std::path::Path;

use async_trait::async_trait;

/// A failure issuing an HTTP request.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct HttpError(pub String);

/// HTTP response from a request.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// Abstraction over the HTTP client, so the layer above can be unit tested
/// without a live rp.
#[async_trait]
#[cfg_attr(test, mockall::automock)]
pub trait HttpClient: Send + Sync {
    /// Send a GET request to `url`.
    async fn get(&self, url: &str) -> Result<HttpResponse, HttpError>;
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
    /// request so the BFF can talk to an auth-enabled rp.
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
        // Don't reuse connections: a fresh connection per request survives
        // rp restarting between calls, which a pooled keep-alive connection
        // would not. The reads are low-frequency, so the lost pooling is
        // immaterial.
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
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// Port 1 is reserved and unbound, so connections are always refused.
    const UNREACHABLE_URL: &str = "http://127.0.0.1:1/api/equipment";

    #[tokio::test]
    async fn get_connection_refused_is_an_error() {
        let client = ReqwestHttpClient::new(None).unwrap();
        let err = client.get(UNREACHABLE_URL).await.unwrap_err();
        assert!(err.0.starts_with("GET "), "{}", err.0);
    }
}
