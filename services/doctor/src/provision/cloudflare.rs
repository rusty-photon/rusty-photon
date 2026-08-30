//! Wire-level client for the Cloudflare v4 REST API, scoped to the four
//! DNS endpoints the ACME DNS-01 challenge needs: zone lookup and TXT
//! record create/list/delete.
//!
//! The calls go straight through the workspace `reqwest` client rather
//! than a vendor SDK, and the response types name only the fields the
//! DNS-01 flow consumes — everything else in a response is ignored, so
//! drift in Cloudflare's payloads cannot break deserialization.

use std::time::Duration;

use async_trait::async_trait;
use rusty_photon_tls::error::{Result, TlsError};
use serde::{Deserialize, Serialize};

use super::dns::{CloudflareApi, RecordInfo, ZoneInfo};

/// Production Cloudflare v4 API base URL.
const CLOUDFLARE_API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Every call is one small JSON exchange; a request that cannot finish
/// inside this window is dead, not slow.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// `Option::None` for any payload type: `#[serde(default)]` on a generic
/// field would put a `Default` bound on the payload, which this named
/// default avoids.
const fn none<T>() -> Option<T> {
    None
}

/// Cloudflare's v4 response envelope, reduced to what the DNS-01 flow
/// reads: the success flag, the error list, and the payload.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<ApiError>,
    #[serde(default = "none")]
    result: Option<T>,
}

/// One entry of an envelope's `errors` array.
#[derive(Debug, Deserialize)]
struct ApiError {
    code: i64,
    message: String,
}

/// A zone or DNS record, reduced to its identifier.
#[derive(Debug, Deserialize)]
struct IdOnly {
    id: String,
}

/// Request body for creating the challenge TXT record.
#[derive(Debug, Serialize)]
struct CreateTxtRecord<'a> {
    #[serde(rename = "type")]
    record_type: &'a str,
    name: &'a str,
    content: &'a str,
    ttl: u32,
    proxied: bool,
}

/// [`CloudflareApi`] implementation that talks to the Cloudflare v4 REST
/// API over HTTPS, authenticating every request with a bearer API token.
pub struct RealCloudflareApi {
    client: reqwest::Client,
    base_url: String,
    api_token: String,
}

impl RealCloudflareApi {
    /// A client authenticating with `api_token` against the production
    /// Cloudflare API.
    ///
    /// # Errors
    ///
    /// Returns [`TlsError::DnsProvider`] if the HTTP client cannot be
    /// constructed.
    pub fn new(api_token: &str) -> Result<Self> {
        Self::with_base_url(api_token, CLOUDFLARE_API_BASE)
    }

    /// A client against an arbitrary base URL; the seam the stub-server
    /// tests use to stand in for the production endpoint.
    fn with_base_url(api_token: &str, base_url: &str) -> Result<Self> {
        let client =
            Self::map_build_error(reqwest::Client::builder().timeout(HTTP_TIMEOUT).build())?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_token: api_token.to_string(),
        })
    }

    /// Shape a client-construction failure; a separate function because
    /// `reqwest` offers no portable way to make `build()` fail, so the
    /// error path is exercised with a transport error obtained elsewhere.
    fn map_build_error(result: reqwest::Result<reqwest::Client>) -> Result<reqwest::Client> {
        result
            .map_err(|e| TlsError::DnsProvider(format!("failed to create Cloudflare client: {e}")))
    }

    /// Send `request` with bearer auth and parse the response envelope,
    /// folding transport failures, unparseable bodies, and refused
    /// (`success: false`) envelopes into [`TlsError::DnsProvider`], each
    /// message prefixed with `failed to <action>`.
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        action: &str,
    ) -> Result<Option<T>> {
        let response = request
            .bearer_auth(&self.api_token)
            .send()
            .await
            .map_err(|e| TlsError::DnsProvider(format!("failed to {action}: {e}")))?;
        let status = response.status();
        let body = response.bytes().await.map_err(|e| {
            TlsError::DnsProvider(format!(
                "failed to {action}: error reading response (HTTP {status}): {e}"
            ))
        })?;
        let envelope: Envelope<T> = serde_json::from_slice(&body).map_err(|e| {
            TlsError::DnsProvider(format!(
                "failed to {action}: unparseable Cloudflare response (HTTP {status}): {e}"
            ))
        })?;
        if !envelope.success {
            let details = if envelope.errors.is_empty() {
                "no error details in response".to_string()
            } else {
                envelope
                    .errors
                    .iter()
                    .map(|e| format!("error {}: {}", e.code, e.message))
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            return Err(TlsError::DnsProvider(format!(
                "failed to {action}: Cloudflare API refused (HTTP {status}): {details}"
            )));
        }
        Ok(envelope.result)
    }

    /// Like [`Self::call`], but for endpoints whose successful envelope
    /// must carry a payload: a `success: true` response with a missing or
    /// null `result` is API drift and reported as such, never flattened
    /// into an empty answer — an "empty" zone list would blame the API
    /// token, and an "empty" record list would skip challenge cleanup.
    async fn call_expecting_result<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        action: &str,
    ) -> Result<T> {
        self.call(request, action).await?.ok_or_else(|| {
            TlsError::DnsProvider(format!(
                "failed to {action}: Cloudflare answered success with no result payload"
            ))
        })
    }
}

#[async_trait]
impl CloudflareApi for RealCloudflareApi {
    async fn list_zones(&self, domain: String) -> Result<Vec<ZoneInfo>> {
        let request = self
            .client
            .get(format!("{}/zones", self.base_url))
            .query(&[("name", domain.as_str())]);
        let zones: Vec<IdOnly> = self.call_expecting_result(request, "list zones").await?;
        Ok(zones.into_iter().map(|z| ZoneInfo { id: z.id }).collect())
    }

    async fn create_txt_record_api(
        &self,
        zone_id: String,
        name: String,
        content: String,
    ) -> Result<()> {
        let request = self
            .client
            .post(format!("{}/zones/{zone_id}/dns_records", self.base_url))
            .json(&CreateTxtRecord {
                record_type: "TXT",
                name: &name,
                content: &content,
                ttl: 60,
                proxied: false,
            });
        self.call::<serde::de::IgnoredAny>(request, "create TXT record")
            .await?;
        Ok(())
    }

    async fn list_txt_records(&self, zone_id: String, name: String) -> Result<Vec<RecordInfo>> {
        let request = self
            .client
            .get(format!("{}/zones/{zone_id}/dns_records", self.base_url))
            .query(&[("type", "TXT"), ("name", name.as_str())]);
        let records: Vec<IdOnly> = self
            .call_expecting_result(request, "list TXT records")
            .await?;
        Ok(records
            .into_iter()
            .map(|r| RecordInfo { id: r.id })
            .collect())
    }

    async fn delete_record(&self, zone_id: String, record_id: String) -> Result<()> {
        let request = self.client.delete(format!(
            "{}/zones/{zone_id}/dns_records/{record_id}",
            self.base_url
        ));
        self.call::<serde::de::IgnoredAny>(request, &format!("delete TXT record {record_id}"))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::{Request, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::Router;

    use super::*;

    // -----------------------------------------------------------------------
    // Recording stub server (ADR-004 Tier 2: in-test axum stub)
    // -----------------------------------------------------------------------

    #[derive(Debug)]
    struct RecordedRequest {
        method: String,
        path: String,
        query: String,
        authorization: Option<String>,
        body: Option<serde_json::Value>,
    }

    struct StubState {
        status: StatusCode,
        body: String,
        seen: Mutex<Vec<RecordedRequest>>,
    }

    async fn record_and_respond(
        State(state): State<Arc<StubState>>,
        request: Request,
    ) -> impl IntoResponse {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
        let recorded = RecordedRequest {
            method: parts.method.to_string(),
            path: parts.uri.path().to_string(),
            query: parts.uri.query().unwrap_or_default().to_string(),
            authorization: parts
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            body: if bytes.is_empty() {
                None
            } else {
                Some(serde_json::from_slice(&bytes).unwrap())
            },
        };
        state.seen.lock().unwrap().push(recorded);
        (
            state.status,
            [("content-type", "application/json")],
            state.body.clone(),
        )
    }

    struct Stub {
        base_url: String,
        state: Arc<StubState>,
    }

    impl Stub {
        /// The single request the test drove through the stub.
        fn only_request(&self) -> RecordedRequest {
            let mut seen = self.state.seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "expected exactly one request: {seen:?}");
            seen.remove(0)
        }
    }

    async fn spawn_stub(status: u16, body: &str) -> Stub {
        let state = Arc::new(StubState {
            status: StatusCode::from_u16(status).unwrap(),
            body: body.to_string(),
            seen: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .fallback(record_and_respond)
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Stub { base_url, state }
    }

    fn api_for(stub: &Stub) -> RealCloudflareApi {
        RealCloudflareApi::with_base_url("test-token", &stub.base_url).unwrap()
    }

    // -----------------------------------------------------------------------
    // Construction tests
    // -----------------------------------------------------------------------

    #[test]
    fn new_constructs_a_client_for_the_production_endpoint() {
        let api = RealCloudflareApi::new("test-token").unwrap();
        assert_eq!(api.base_url, CLOUDFLARE_API_BASE);
    }

    // -----------------------------------------------------------------------
    // Request-shape tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_zones_queries_by_exact_name_with_bearer_auth() {
        let stub = spawn_stub(
            200,
            r#"{"success":true,"errors":[],"result":[{"id":"zone-1","name":"example.com","status":"active"}]}"#,
        )
        .await;

        let zones = api_for(&stub)
            .list_zones("example.com".to_string())
            .await
            .unwrap();

        assert_eq!(zones.len(), 1);
        assert_eq!(zones.first().unwrap().id, "zone-1");
        let request = stub.only_request();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/zones");
        assert_eq!(request.query, "name=example.com");
        assert_eq!(request.authorization.as_deref(), Some("Bearer test-token"));
    }

    #[tokio::test]
    async fn create_txt_record_posts_the_record_shape() {
        let stub = spawn_stub(
            200,
            r#"{"success":true,"errors":[],"result":{"id":"rec-1"}}"#,
        )
        .await;

        api_for(&stub)
            .create_txt_record_api(
                "zone-9".to_string(),
                "_acme-challenge.rig.example.com".to_string(),
                "challenge-value".to_string(),
            )
            .await
            .unwrap();

        let request = stub.only_request();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/zones/zone-9/dns_records");
        assert_eq!(
            request.body.unwrap(),
            serde_json::json!({
                "type": "TXT",
                "name": "_acme-challenge.rig.example.com",
                "content": "challenge-value",
                "ttl": 60,
                "proxied": false,
            })
        );
    }

    #[tokio::test]
    async fn list_txt_records_filters_by_type_and_name_server_side() {
        let stub = spawn_stub(
            200,
            r#"{"success":true,"errors":[],"result":[{"id":"rec-1"},{"id":"rec-2"}]}"#,
        )
        .await;

        let records = api_for(&stub)
            .list_txt_records(
                "zone-9".to_string(),
                "_acme-challenge.rig.example.com".to_string(),
            )
            .await
            .unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records.first().unwrap().id, "rec-1");
        let request = stub.only_request();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/zones/zone-9/dns_records");
        assert_eq!(
            request.query,
            "type=TXT&name=_acme-challenge.rig.example.com"
        );
    }

    #[tokio::test]
    async fn delete_record_targets_the_record_path() {
        let stub = spawn_stub(
            200,
            r#"{"success":true,"errors":[],"result":{"id":"rec-1"}}"#,
        )
        .await;

        api_for(&stub)
            .delete_record("zone-9".to_string(), "rec-1".to_string())
            .await
            .unwrap();

        let request = stub.only_request();
        assert_eq!(request.method, "DELETE");
        assert_eq!(request.path, "/zones/zone-9/dns_records/rec-1");
    }

    #[tokio::test]
    async fn base_url_trailing_slash_is_tolerated() {
        let stub = spawn_stub(200, r#"{"success":true,"errors":[],"result":[]}"#).await;
        let api =
            RealCloudflareApi::with_base_url("test-token", &format!("{}/", stub.base_url)).unwrap();

        api.list_zones("example.com".to_string()).await.unwrap();

        assert_eq!(stub.only_request().path, "/zones");
    }

    // -----------------------------------------------------------------------
    // Response-handling tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn null_result_on_a_successful_list_is_api_drift_not_empty() {
        let stub = spawn_stub(200, r#"{"success":true,"errors":[],"result":null}"#).await;

        let err = api_for(&stub)
            .list_zones("example.com".to_string())
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("failed to list zones"), "error: {msg}");
        assert!(
            msg.contains("no result payload"),
            "a null result on a list must surface as drift, not read as empty: {msg}"
        );
    }

    #[tokio::test]
    async fn missing_result_key_on_a_successful_list_is_api_drift_not_empty() {
        let stub = spawn_stub(200, r#"{"success":true,"errors":[]}"#).await;

        let err = api_for(&stub)
            .list_zones("example.com".to_string())
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("no result payload"), "error: {msg}");
    }

    #[tokio::test]
    async fn a_failed_client_build_maps_to_a_dns_provider_error() {
        // `reqwest::ClientBuilder::build` cannot be made to fail on demand,
        // so feed the mapper a real transport error instead.
        let transport_error = reqwest::Client::builder()
            .build()
            .unwrap()
            .get("http://127.0.0.1:1")
            .send()
            .await
            .unwrap_err();

        let err = RealCloudflareApi::map_build_error(Err(transport_error)).unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("failed to create Cloudflare client"),
            "error: {msg}"
        );
    }

    #[tokio::test]
    async fn truncated_response_body_maps_to_a_read_error_naming_the_status() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // An axum handler cannot understate a body against its declared
        // Content-Length, so this stub speaks raw HTTP: full headers, a
        // truncated body, then a closed connection — send() succeeds at
        // the header boundary and only the body read fails.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                      content-length: 999\r\n\r\n{\"success\":true",
                )
                .await
                .unwrap();
            socket.shutdown().await.ok();
        });

        let api = RealCloudflareApi::with_base_url("test-token", &base_url).unwrap();
        let err = api.list_zones("example.com".to_string()).await.unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("error reading response"), "error: {msg}");
        assert!(msg.contains("200"), "error should carry the status: {msg}");
    }

    #[tokio::test]
    async fn null_result_on_a_successful_delete_is_tolerated() {
        let stub = spawn_stub(200, r#"{"success":true,"errors":[],"result":null}"#).await;

        api_for(&stub)
            .delete_record("zone-9".to_string(), "rec-1".to_string())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn refused_envelope_error_names_the_action_status_and_details() {
        let stub = spawn_stub(
            403,
            r#"{"success":false,"errors":[{"code":9109,"message":"Invalid access token"}],"result":null}"#,
        )
        .await;

        let err = api_for(&stub)
            .list_zones("example.com".to_string())
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("failed to list zones"), "error: {msg}");
        assert!(msg.contains("403"), "error should name the status: {msg}");
        assert!(msg.contains("9109"), "error should name the code: {msg}");
        assert!(
            msg.contains("Invalid access token"),
            "error should carry the message: {msg}"
        );
    }

    #[tokio::test]
    async fn refused_envelope_without_details_still_errors() {
        let stub = spawn_stub(400, r#"{"success":false,"errors":[],"result":null}"#).await;

        let err = api_for(&stub)
            .delete_record("zone-9".to_string(), "rec-1".to_string())
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("delete TXT record rec-1"),
            "error should name the record: {msg}"
        );
        assert!(msg.contains("no error details"), "error: {msg}");
    }

    #[tokio::test]
    async fn non_json_response_error_names_the_status() {
        let stub = spawn_stub(502, "<html>bad gateway</html>").await;

        let err = api_for(&stub)
            .list_zones("example.com".to_string())
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("failed to list zones"), "error: {msg}");
        assert!(msg.contains("502"), "error should name the status: {msg}");
    }

    #[tokio::test]
    async fn unreachable_server_maps_to_a_dns_provider_error() {
        let api = RealCloudflareApi::with_base_url("test-token", "http://127.0.0.1:1").unwrap();

        let err = api.list_zones("example.com".to_string()).await.unwrap_err();

        assert!(
            err.to_string().contains("failed to list zones"),
            "error: {err}"
        );
    }
}
