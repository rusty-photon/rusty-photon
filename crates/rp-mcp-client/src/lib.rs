#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! The standard authenticated MCP client for `rp`'s `/mcp` endpoint
//! ([ADR-017](../../../docs/decisions/017-standard-mcp-client-construction.md)).
//!
//! Every first-party MCP consumer connects through this crate. It owns the
//! three things that must not drift between consumers:
//!
//! - **Transport construction**: rmcp streamable HTTP over a reqwest client
//!   built by `rusty-photon-tls` (CA-pinned when a CA is configured).
//! - **The credential policy**: the observatory credential is presented as
//!   HTTP Basic **only over verified HTTPS** — a configured credential
//!   without a configured CA, or on a non-HTTPS URL, is not sent; the
//!   client connects unauthenticated and logs a loud warning.
//! - **The result convention**: rp returns tool results as one JSON text
//!   content block; anything else is a loud error, and request-level
//!   failures are kept distinct from tool failures so consumers can map
//!   them onto their own taxonomies.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

use std::path::Path;

use base64::Engine as _;
use reqwest::header::{HeaderName, HeaderValue, AUTHORIZATION};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde_json::{Map, Value};

/// Re-exported so consumers can name the client-auth config type without a
/// direct `rp-auth` dependency.
pub use rp_auth::config::ClientAuthConfig;
use tracing::{debug, warn};

/// Failure to establish the MCP session.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The underlying HTTP client could not be built (bad CA path/PEM).
    #[error("building the HTTP client: {0}")]
    Http(#[from] rusty_photon_tls::error::TlsError),
    /// The Authorization header could not be constructed.
    #[error("building the Authorization header: {0}")]
    Header(String),
    /// The MCP initialize handshake failed (unreachable, TLS rejection,
    /// or an auth rejection surfacing as a failed handshake).
    #[error("connecting to {url}: {message}")]
    Connect { url: String, message: String },
}

/// Failure of an individual MCP call on an established session.
#[derive(Debug, thiserror::Error)]
pub enum McpCallError {
    /// The request itself failed — transport loss or a JSON-RPC protocol
    /// error. The session is unusable; `rp` tearing a session down (e.g.
    /// on a safety transition) presents exactly this way.
    #[error("MCP request failed: {0}")]
    Request(String),
    /// The call returned with the MCP `is_error` flag — a tool failure
    /// reported by a healthy `rp`.
    #[error("{0}")]
    Tool(String),
    /// The call returned, but the result violates the one-JSON-text-block
    /// convention (non-JSON text, non-text content, multiple blocks). The
    /// session is still alive — this is a malformed response, not a
    /// transport failure, and consumers that retry tool failures may
    /// treat it as one.
    #[error("malformed tool result: {0}")]
    Malformed(String),
}

/// One entry of the tool catalog.
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub input_schema: Value,
}

/// An established MCP session with `rp`.
///
/// Sessions are deliberately **not** re-established transparently
/// (`reinit_on_expired_session` stays off): `rp` terminates MCP sessions
/// on safety transitions, and consumers treat a dead session as the
/// signal to stop acting. Reconnecting is an explicit consumer decision.
pub struct RpMcpClient {
    peer: rmcp::Peer<rmcp::RoleClient>,
    // Keep the running service alive so the connection isn't dropped.
    _service: RunningService<rmcp::RoleClient, ()>,
}

impl RpMcpClient {
    /// Connect to `rp`'s MCP endpoint, presenting `service_auth` per the
    /// credential policy (see the crate docs).
    pub async fn connect(
        mcp_url: &str,
        service_auth: Option<&ClientAuthConfig>,
        ca_cert: Option<&Path>,
    ) -> Result<Self, ConnectError> {
        let http_client = rusty_photon_tls::client::build_reqwest_client(ca_cert)?;

        let mut config = StreamableHttpClientTransportConfig::with_uri(mcp_url.to_owned());
        // Hard invariant (ADR-017): rp terminates MCP sessions on safety
        // transitions and consumers must observe a dead session — the
        // transport must never re-establish one transparently, whatever
        // rmcp's default becomes.
        config.reinit_on_expired_session = false;
        if let Some(header) = basic_authorization(mcp_url, service_auth, ca_cert)? {
            config =
                config.custom_headers(std::collections::HashMap::<HeaderName, HeaderValue>::from(
                    [(AUTHORIZATION, header)],
                ));
        }

        debug!(url = %mcp_url, "connecting MCP client");
        let transport = StreamableHttpClientTransport::with_client(http_client, config);
        let service =
            ().serve(transport)
                .await
                .map_err(|e| ConnectError::Connect {
                    url: mcp_url.to_owned(),
                    message: e.to_string(),
                })?;
        let peer = service.peer().clone();
        Ok(Self {
            peer,
            _service: service,
        })
    }

    /// Call a tool and parse the result per the rp convention: no content
    /// is `null`, one JSON text block is the parsed value, anything else
    /// is a loud failure.
    pub async fn call_tool(
        &self,
        tool: &str,
        args: Map<String, Value>,
    ) -> Result<Value, McpCallError> {
        let mut params = CallToolRequestParams::new(tool.to_string());
        if !args.is_empty() {
            params.arguments = Some(args);
        }
        let result = self
            .peer
            .call_tool(params)
            .await
            .map_err(|e| McpCallError::Request(e.to_string()))?;

        if result.is_error.unwrap_or(false) {
            let message = result
                .content
                .first()
                .and_then(|content| content.as_text())
                .map_or_else(|| "unknown error".to_owned(), |text| text.text.clone());
            return Err(McpCallError::Tool(message));
        }

        parse_content(&result.content)
    }

    /// `tools/list` — the full catalog.
    pub async fn list_tools(&self) -> Result<Vec<ToolInfo>, McpCallError> {
        let tools = self
            .peer
            .list_all_tools()
            .await
            .map_err(|e| McpCallError::Request(format!("tools/list: {e}")))?;
        Ok(tools
            .into_iter()
            .map(|tool| ToolInfo {
                name: tool.name.into_owned(),
                input_schema: Value::Object((*tool.input_schema).clone()),
            })
            .collect())
    }
}

/// The credential policy: an `Authorization: Basic …` header is produced
/// only when a credential **and** a CA are configured **and** the URL is
/// HTTPS. Any other combination with a credential present warns loudly
/// and produces `None` — plaintext credentials never travel over
/// cleartext or unverified channels.
///
/// Public so a consumer's *other* connections to the same rp (an SSE
/// subscription, a completion POST) apply the identical policy instead of
/// re-deriving it. The returned header is marked sensitive.
pub fn basic_authorization(
    url: &str,
    service_auth: Option<&ClientAuthConfig>,
    ca_cert: Option<&Path>,
) -> Result<Option<HeaderValue>, ConnectError> {
    let Some(auth) = service_auth else {
        return Ok(None);
    };
    if ca_cert.is_none() {
        warn!(
            url = %url,
            "service_auth is configured without ca_cert; connecting UNAUTHENTICATED \
             (credentials only ride verified HTTPS — configure ca_cert to send them)"
        );
        return Ok(None);
    }
    if !url.starts_with("https://") {
        warn!(
            url = %url,
            "service_auth is configured but the URL is not https; connecting \
             UNAUTHENTICATED (credentials only ride verified HTTPS)"
        );
        return Ok(None);
    }

    let encoded = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", auth.username, auth.password));
    let mut header = HeaderValue::from_str(&format!("Basic {encoded}"))
        .map_err(|e| ConnectError::Header(e.to_string()))?;
    header.set_sensitive(true);
    Ok(Some(header))
}

/// The one-JSON-text-block result convention.
fn parse_content(content: &[rmcp::model::ContentBlock]) -> Result<Value, McpCallError> {
    match content {
        [] => Ok(Value::Null),
        [block] => block.as_text().map_or_else(
            || {
                Err(McpCallError::Malformed(
                    "tool returned non-text content; expected one JSON text block".to_owned(),
                ))
            },
            |text| {
                serde_json::from_str(&text.text).map_err(|e| {
                    McpCallError::Malformed(format!("tool returned non-JSON content: {e}"))
                })
            },
        ),
        blocks => Err(McpCallError::Malformed(format!(
            "tool returned {} content blocks; expected one JSON text block",
            blocks.len()
        ))),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn auth() -> ClientAuthConfig {
        ClientAuthConfig {
            username: "observatory".to_owned(),
            password: "secret".to_owned(),
        }
    }

    #[test]
    fn no_credential_produces_no_header() {
        let header = basic_authorization("https://localhost:1/mcp", None, None).unwrap();
        assert!(header.is_none());
    }

    #[test]
    fn credential_without_ca_is_not_sent() {
        let header = basic_authorization("https://localhost:1/mcp", Some(&auth()), None).unwrap();
        assert!(header.is_none());
    }

    #[test]
    fn credential_on_plain_http_is_not_sent() {
        let ca = std::path::PathBuf::from("/does/not/matter/ca.pem");
        let header =
            basic_authorization("http://localhost:1/mcp", Some(&auth()), Some(&ca)).unwrap();
        assert!(header.is_none());
    }

    #[test]
    fn credential_with_ca_over_https_produces_basic_header() {
        let ca = std::path::PathBuf::from("/does/not/matter/ca.pem");
        let header = basic_authorization("https://localhost:1/mcp", Some(&auth()), Some(&ca))
            .unwrap()
            .expect("header expected");
        // base64("observatory:secret")
        assert_eq!(header.to_str().unwrap(), "Basic b2JzZXJ2YXRvcnk6c2VjcmV0");
        assert!(header.is_sensitive());
    }

    #[test]
    fn empty_content_parses_to_null() {
        assert_eq!(parse_content(&[]).unwrap(), Value::Null);
    }

    #[test]
    fn single_json_text_block_parses() {
        let content = vec![rmcp::model::ContentBlock::text(r#"{"position": 42}"#)];
        let value = parse_content(&content).unwrap();
        assert_eq!(value["position"], 42);
    }

    #[test]
    fn non_json_text_is_a_malformed_error() {
        let content = vec![rmcp::model::ContentBlock::text("not json")];
        let err = parse_content(&content).unwrap_err();
        assert!(matches!(err, McpCallError::Malformed(_)), "got: {err:?}");
    }

    #[test]
    fn multiple_blocks_are_a_malformed_error() {
        let content = vec![
            rmcp::model::ContentBlock::text("{}"),
            rmcp::model::ContentBlock::text("{}"),
        ];
        let err = parse_content(&content).unwrap_err();
        let McpCallError::Malformed(message) = err else {
            panic!("expected Malformed error");
        };
        assert!(message.contains("2 content blocks"), "got: {message}");
    }

    #[tokio::test]
    async fn connect_to_unreachable_url_is_a_connect_error() {
        // Port 1 on loopback is closed: the connection is refused
        // immediately rather than timing out.
        let result = RpMcpClient::connect("http://127.0.0.1:1/mcp", None, None).await;
        let err = result.err().expect("connect must fail");
        assert!(matches!(err, ConnectError::Connect { .. }), "got: {err:?}");
    }
}
