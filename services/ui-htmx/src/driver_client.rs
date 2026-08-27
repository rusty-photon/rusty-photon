//! Driver config-action client.
//!
//! Speaks the cross-driver config-action protocol (`config.get` / `config.schema`
//! / `config.apply` ASCOM actions) defined in
//! [`docs/services/config-actions.md`] and implemented by the shared
//! [`rusty_photon_config::actions`] module. `AlpacaConfigClient` shapes the
//! `PUT .../action` request over an [`HttpClient`], unwraps the Alpaca envelope,
//! and parses the inner JSON body into the shared wire types. The page handlers
//! depend on the [`ConfigClient`] trait so they can be driven by a stub in tests.
//!
//! The request/response types ([`ConfigGetResponse`], [`ConfigSchemaResponse`],
//! [`ConfigApplyResponse`], [`ApplyStatus`], [`FieldError`]) are re-exported from
//! `rusty_photon_config::actions` — the single source of truth shared with every
//! driver — so the BFF and the drivers can never drift on the wire shape.
//!
//! [`docs/services/config-actions.md`]: ../../../docs/services/config-actions.md

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::io::HttpClient;

// The wire types are defined once in the shared crate and reused here, so the
// BFF parses exactly what the drivers serialize. The BFF pulls no driver crates;
// `rusty-photon-config` is a light, driver-agnostic crate.
pub use rusty_photon_config::actions::{
    ApplyStatus, ConfigApplyResponse, ConfigGetResponse, ConfigSchemaResponse, FieldError, REDACTED,
};

/// ASCOM `ACTION_NOT_IMPLEMENTED` (`0x40C`). Returned by `action()` when the
/// target is not a config-capable driver.
pub const ACTION_NOT_IMPLEMENTED: i32 = 0x40C;

/// A failure reading or applying a driver's configuration.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ConfigClientError {
    /// Network failure, or a non-2xx HTTP status (the driver is down, or its
    /// auth/TLS layer refused us).
    #[error("could not reach the driver: {0}")]
    Transport(String),
    /// The target answered but refused the request — a REST 4xx (e.g. rp's
    /// 400 for a config body its types cannot parse). Distinct from
    /// [`ConfigClientError::Transport`] so the page doesn't claim an
    /// answering service is unreachable.
    #[error("the request was rejected: {0}")]
    Rejected(String),
    /// The action returned an ASCOM error (`ErrorNumber != 0`).
    #[error("driver returned ASCOM error {code}: {message}")]
    Ascom { code: i32, message: String },
    /// The envelope or inner body could not be decoded.
    #[error("could not decode the driver response: {0}")]
    Decode(String),
}

impl ConfigClientError {
    /// Whether this is the `ACTION_NOT_IMPLEMENTED` ASCOM error — i.e. the target
    /// driver does not expose the config actions.
    #[must_use]
    pub const fn is_action_not_implemented(&self) -> bool {
        matches!(self, Self::Ascom { code, .. } if *code == ACTION_NOT_IMPLEMENTED)
    }
}

/// Reads, describes, and applies a single driver's configuration. Handlers depend
/// on this trait so tests can inject canned responses.
#[async_trait]
pub trait ConfigClient: Send + Sync {
    /// `config.get` — the effective config (secrets redacted) + override-pinned paths.
    async fn get_config(&self) -> Result<ConfigGetResponse, ConfigClientError>;
    /// `config.schema` — the JSON Schema + editability tiers the form renders from.
    async fn get_schema(&self) -> Result<ConfigSchemaResponse, ConfigClientError>;
    /// `config.apply` — persist the submitted config; returns the classification.
    async fn apply_config(&self, config: &Value) -> Result<ConfigApplyResponse, ConfigClientError>;
}

/// The Alpaca envelope every ASCOM response is wrapped in. We only read the
/// fields we need; the transaction ids are ignored.
#[derive(Debug, serde::Deserialize)]
struct AlpacaEnvelope {
    #[serde(rename = "Value", default)]
    value: Value,
    #[serde(rename = "ErrorNumber", default)]
    error_number: i32,
    #[serde(rename = "ErrorMessage", default)]
    error_message: String,
}

/// `ConfigClient` backed by a driver's ASCOM Alpaca `action` endpoint.
pub struct AlpacaConfigClient {
    http: Arc<dyn HttpClient>,
    action_url: String,
}

impl AlpacaConfigClient {
    /// Target `{base_url}/api/v1/{device_type}/{device_number}/action`.
    pub fn new(
        http: Arc<dyn HttpClient>,
        base_url: &str,
        device_type: &str,
        device_number: u32,
    ) -> Self {
        let action_url = format!(
            "{}/api/v1/{}/{}/action",
            base_url.trim_end_matches('/'),
            device_type,
            device_number
        );
        Self { http, action_url }
    }

    /// Call an ASCOM action and return the parsed inner JSON body. For these
    /// vendor actions the driver returns a JSON string in `Value`, so we unwrap
    /// the envelope and then parse that string.
    async fn call_action(
        &self,
        action: &str,
        parameters: &str,
    ) -> Result<Value, ConfigClientError> {
        let response = self
            .http
            .put_form(
                &self.action_url,
                &[
                    ("Action", action),
                    ("Parameters", parameters),
                    ("ClientID", "1"),
                    ("ClientTransactionID", "1"),
                ],
            )
            .await
            .map_err(|e| ConfigClientError::Transport(e.to_string()))?;

        if !(200..300).contains(&response.status) {
            return Err(ConfigClientError::Transport(format!(
                "HTTP {} from {}",
                response.status, self.action_url
            )));
        }

        let envelope: AlpacaEnvelope = serde_json::from_str(&response.body)
            .map_err(|e| ConfigClientError::Decode(e.to_string()))?;
        if envelope.error_number != 0 {
            return Err(ConfigClientError::Ascom {
                code: envelope.error_number,
                message: envelope.error_message,
            });
        }

        let inner = envelope.value.as_str().ok_or_else(|| {
            ConfigClientError::Decode("action Value was not a JSON string".to_string())
        })?;
        serde_json::from_str(inner).map_err(|e| ConfigClientError::Decode(e.to_string()))
    }
}

#[async_trait]
impl ConfigClient for AlpacaConfigClient {
    async fn get_config(&self) -> Result<ConfigGetResponse, ConfigClientError> {
        let inner = self.call_action("config.get", "").await?;
        serde_json::from_value(inner).map_err(|e| ConfigClientError::Decode(e.to_string()))
    }

    async fn get_schema(&self) -> Result<ConfigSchemaResponse, ConfigClientError> {
        let inner = self.call_action("config.schema", "").await?;
        serde_json::from_value(inner).map_err(|e| ConfigClientError::Decode(e.to_string()))
    }

    async fn apply_config(&self, config: &Value) -> Result<ConfigApplyResponse, ConfigClientError> {
        let parameters =
            serde_json::to_string(config).map_err(|e| ConfigClientError::Decode(e.to_string()))?;
        let inner = self.call_action("config.apply", &parameters).await?;
        serde_json::from_value(inner).map_err(|e| ConfigClientError::Decode(e.to_string()))
    }
}

/// `ConfigClient` backed by rp's plain-REST config endpoints — the same
/// three operations and bodies as the ASCOM actions, without the Alpaca
/// envelope.
///
/// The endpoints (`docs/services/config-actions.md` "REST transport"):
/// `GET /api/config`, `GET /api/config/schema`, `PUT /api/config`.
pub struct RestConfigClient {
    http: Arc<dyn HttpClient>,
    base_url: String,
}

impl RestConfigClient {
    pub fn new(http: Arc<dyn HttpClient>, base_url: &str) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Map a REST response to its parsed body. Unlike the action transport
    /// there is no envelope: a 2xx body IS the payload; a 4xx is a rejection
    /// carrying rp's message (e.g. the 400 for a malformed apply body); any
    /// other status is a transport error.
    fn parse<T: serde::de::DeserializeOwned>(
        url: &str,
        response: &crate::io::HttpResponse,
    ) -> Result<T, ConfigClientError> {
        if !(200..300).contains(&response.status) {
            let detail = response.body.chars().take(200).collect::<String>();
            let message = format!("HTTP {} from {url}: {detail}", response.status);
            return Err(if (400..500).contains(&response.status) {
                ConfigClientError::Rejected(message)
            } else {
                ConfigClientError::Transport(message)
            });
        }
        serde_json::from_str(&response.body).map_err(|e| ConfigClientError::Decode(e.to_string()))
    }
}

#[async_trait]
impl ConfigClient for RestConfigClient {
    async fn get_config(&self) -> Result<ConfigGetResponse, ConfigClientError> {
        let url = self.url("/api/config");
        let response = self
            .http
            .get(&url)
            .await
            .map_err(|e| ConfigClientError::Transport(e.to_string()))?;
        Self::parse(&url, &response)
    }

    async fn get_schema(&self) -> Result<ConfigSchemaResponse, ConfigClientError> {
        let url = self.url("/api/config/schema");
        let response = self
            .http
            .get(&url)
            .await
            .map_err(|e| ConfigClientError::Transport(e.to_string()))?;
        Self::parse(&url, &response)
    }

    async fn apply_config(&self, config: &Value) -> Result<ConfigApplyResponse, ConfigClientError> {
        let url = self.url("/api/config");
        let body =
            serde_json::to_string(config).map_err(|e| ConfigClientError::Decode(e.to_string()))?;
        let response = self
            .http
            .put_json(&url, &body)
            .await
            .map_err(|e| ConfigClientError::Transport(e.to_string()))?;
        Self::parse(&url, &response)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::io::{HttpResponse, MockHttpClient};
    use serde_json::json;

    /// Wrap an inner JSON body the way the driver does: serialize it to a string
    /// and place it in the Alpaca envelope's `Value`.
    fn ok_envelope(inner: Value) -> String {
        let value_string = serde_json::to_string(&inner).unwrap();
        json!({
            "Value": value_string,
            "ClientTransactionID": 1,
            "ServerTransactionID": 7,
            "ErrorNumber": 0,
            "ErrorMessage": ""
        })
        .to_string()
    }

    fn client_returning(body: HttpResponse) -> AlpacaConfigClient {
        let mut http = MockHttpClient::new();
        http.expect_put_form().returning(move |_, _| {
            let body = body.clone();
            Box::pin(async move { Ok(body) })
        });
        AlpacaConfigClient::new(Arc::new(http), "http://driver:11119", "covercalibrator", 0)
    }

    #[tokio::test]
    async fn get_config_parses_the_inner_body() {
        let inner = json!({
            "config": { "serial": { "port": "/dev/ttyACM0" } },
            "overrides": ["serial.port"]
        });
        let client = client_returning(HttpResponse {
            status: 200,
            body: ok_envelope(inner),
        });

        let resp = client.get_config().await.unwrap();
        assert_eq!(
            resp.config.pointer("/serial/port").and_then(Value::as_str),
            Some("/dev/ttyACM0")
        );
        assert_eq!(resp.overrides, vec!["serial.port".to_string()]);
    }

    #[tokio::test]
    async fn get_schema_parses_schema_and_tiers() {
        let inner = json!({
            "schema": { "type": "object", "properties": { "serial": { "type": "object" } } },
            "locked_fields": ["cover_calibrator.unique_id"],
            "read_only_fields": ["server.port"]
        });
        let client = client_returning(HttpResponse {
            status: 200,
            body: ok_envelope(inner),
        });

        let resp = client.get_schema().await.unwrap();
        assert_eq!(
            resp.locked_fields,
            vec!["cover_calibrator.unique_id".to_string()]
        );
        assert_eq!(resp.read_only_fields, vec!["server.port".to_string()]);
        assert!(resp.schema.pointer("/properties/serial").is_some());
    }

    #[tokio::test]
    async fn apply_config_parses_status_and_reload() {
        let inner = json!({
            "status": "applying",
            "applied": [],
            "reload": ["cover_calibrator.max_brightness"],
            "restart_required": [],
            "skipped_override": [],
            "persisted_to": "/tmp/dsd-fp2.json"
        });
        let client = client_returning(HttpResponse {
            status: 200,
            body: ok_envelope(inner),
        });

        let resp = client.apply_config(&json!({})).await.unwrap();
        assert_eq!(resp.status, ApplyStatus::Applying);
        assert_eq!(
            resp.reload,
            vec!["cover_calibrator.max_brightness".to_string()]
        );
    }

    #[tokio::test]
    async fn apply_config_surfaces_invalid_field_errors() {
        let inner = json!({
            "status": "invalid",
            "applied": [],
            "reload": [],
            "restart_required": [],
            "skipped_override": [],
            "errors": [{ "path": "serial.baud_rate", "msg": "must be greater than 0" }]
        });
        let client = client_returning(HttpResponse {
            status: 200,
            body: ok_envelope(inner),
        });

        let resp = client.apply_config(&json!({})).await.unwrap();
        assert_eq!(resp.status, ApplyStatus::Invalid);
        assert_eq!(resp.errors.len(), 1);
        assert_eq!(resp.errors[0].path, "serial.baud_rate");
    }

    #[tokio::test]
    async fn ascom_error_number_maps_to_action_not_implemented() {
        let body = json!({
            "Value": "",
            "ErrorNumber": 0x40C,
            "ErrorMessage": "unknown action \"config.get\""
        })
        .to_string();
        let client = client_returning(HttpResponse { status: 200, body });

        let err = client.get_config().await.unwrap_err();
        assert!(err.is_action_not_implemented(), "{err:?}");
    }

    #[tokio::test]
    async fn http_non_2xx_is_a_transport_error() {
        let client = client_returning(HttpResponse {
            status: 401,
            body: "unauthorized".to_string(),
        });
        let err = client.get_config().await.unwrap_err();
        assert!(matches!(err, ConfigClientError::Transport(_)), "{err:?}");
    }

    #[tokio::test]
    async fn transport_failure_is_a_transport_error() {
        let mut http = MockHttpClient::new();
        http.expect_put_form().returning(|_, _| {
            Box::pin(async {
                Err::<HttpResponse, _>(crate::io::HttpError("connection refused".to_string()))
            })
        });
        let client =
            AlpacaConfigClient::new(Arc::new(http), "http://driver:11119", "covercalibrator", 0);
        let err = client.get_config().await.unwrap_err();
        assert!(matches!(err, ConfigClientError::Transport(_)), "{err:?}");
    }

    #[test]
    fn action_url_is_well_formed_and_trims_trailing_slash() {
        let http = Arc::new(MockHttpClient::new());
        let client = AlpacaConfigClient::new(http, "http://driver:11119/", "covercalibrator", 0);
        assert_eq!(
            client.action_url,
            "http://driver:11119/api/v1/covercalibrator/0/action"
        );
    }

    // --- RestConfigClient (rp's plain-REST transport) ---------------------------

    #[tokio::test]
    async fn rest_get_config_hits_api_config_and_parses_the_body() {
        let mut http = MockHttpClient::new();
        http.expect_get()
            .withf(|url| url == "http://rp:11115/api/config")
            .returning(|_| {
                Box::pin(async {
                    Ok(HttpResponse {
                        status: 200,
                        body: json!({
                            "config": { "server": { "port": 11115 } },
                            "overrides": []
                        })
                        .to_string(),
                    })
                })
            });
        let client = RestConfigClient::new(Arc::new(http), "http://rp:11115/");
        let resp = client.get_config().await.unwrap();
        assert_eq!(
            resp.config.pointer("/server/port").and_then(Value::as_u64),
            Some(11115)
        );
        assert_eq!(resp.overrides, Vec::<String>::new());
    }

    #[tokio::test]
    async fn rest_get_schema_hits_api_config_schema() {
        let mut http = MockHttpClient::new();
        http.expect_get()
            .withf(|url| url == "http://rp:11115/api/config/schema")
            .returning(|_| {
                Box::pin(async {
                    Ok(HttpResponse {
                        status: 200,
                        body: json!({
                            "schema": { "type": "object" },
                            "locked_fields": [],
                            "read_only_fields": ["server.port"]
                        })
                        .to_string(),
                    })
                })
            });
        let client = RestConfigClient::new(Arc::new(http), "http://rp:11115");
        let resp = client.get_schema().await.unwrap();
        assert_eq!(resp.read_only_fields, vec!["server.port".to_string()]);
    }

    #[tokio::test]
    async fn rest_apply_puts_the_config_json_and_parses_restart_required() {
        let mut http = MockHttpClient::new();
        http.expect_put_json()
            .withf(|url, body| url == "http://rp:11115/api/config" && body.contains("\"site\""))
            .returning(|_, _| {
                Box::pin(async {
                    Ok(HttpResponse {
                        status: 200,
                        body: json!({
                            "status": "ok",
                            "applied": [],
                            "reload": [],
                            "restart_required": ["site.latitude_degrees"],
                            "skipped_override": [],
                            "persisted_to": "/tmp/rp.json"
                        })
                        .to_string(),
                    })
                })
            });
        let client = RestConfigClient::new(Arc::new(http), "http://rp:11115");
        let resp = client
            .apply_config(&json!({ "site": { "latitude_degrees": 47.6 } }))
            .await
            .unwrap();
        assert_eq!(resp.status, ApplyStatus::Ok);
        assert_eq!(
            resp.restart_required,
            vec!["site.latitude_degrees".to_string()]
        );
    }

    #[tokio::test]
    async fn rest_4xx_is_a_rejection_carrying_the_body() {
        let mut http = MockHttpClient::new();
        http.expect_put_json().returning(|_, _| {
            Box::pin(async {
                Ok(HttpResponse {
                    status: 400,
                    body: "invalid config JSON: expected value at line 1".to_string(),
                })
            })
        });
        let client = RestConfigClient::new(Arc::new(http), "http://rp:11115");
        let err = client.apply_config(&json!({})).await.unwrap_err();
        match err {
            ConfigClientError::Rejected(msg) => {
                assert!(msg.contains("HTTP 400"), "{msg}");
                assert!(msg.contains("invalid config JSON"), "{msg}");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rest_5xx_is_a_transport_error() {
        let mut http = MockHttpClient::new();
        http.expect_get().returning(|_| {
            Box::pin(async {
                Ok(HttpResponse {
                    status: 502,
                    body: "bad gateway".to_string(),
                })
            })
        });
        let client = RestConfigClient::new(Arc::new(http), "http://rp:11115");
        let err = client.get_config().await.unwrap_err();
        assert!(matches!(err, ConfigClientError::Transport(_)), "{err:?}");
    }

    #[tokio::test]
    async fn rest_connection_failure_is_a_transport_error() {
        let mut http = MockHttpClient::new();
        http.expect_get().returning(|_| {
            Box::pin(async {
                Err::<HttpResponse, _>(crate::io::HttpError("connection refused".to_string()))
            })
        });
        let client = RestConfigClient::new(Arc::new(http), "http://rp:11115");
        let err = client.get_config().await.unwrap_err();
        assert!(matches!(err, ConfigClientError::Transport(_)), "{err:?}");
    }
}
