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
//!   failures are kept distinct from tool failures — and from rp's
//!   safety refusal, a JSON-RPC error with its own code — so consumers
//!   can map them onto their own taxonomies.
//! - **The protocol revision**: MCP [`PROTOCOL_VERSION`] (2026-07-28),
//!   bootstrapped with `server/discover` and pinned rather than
//!   inherited from rmcp's default, so the revision `rp` serves is the
//!   one our own CI exercises. There are no sessions
//!   ([ADR-021](../../../docs/decisions/021-session-less-mcp-and-the-safety-contract.md)):
//!   every request is self-contained, nothing is re-established behind
//!   a consumer's back, and "rp unreachable" surfaces at connect as a
//!   failed discovery.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::unused_async_trait_impl,
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
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation, ProtocolVersion,
};
use rmcp::service::RunningService;
use rmcp::service::ServiceError;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientHandler, ClientLifecycleMode, ClientServiceExt};
use serde_json::{Map, Value};

/// Re-exported so consumers can name the client-auth config type without a
/// direct `rp-auth` dependency.
pub use rp_auth::config::ClientAuthConfig;
use tracing::{debug, warn};

/// The JSON-RPC error code of `rp`'s safety refusal (`SafetyUnsafe`).
///
/// `rp` answers a gated tool with it while conditions are unsafe
/// (rp.md § Safety → In-Flight Tool Calls). Mirrors `rp`'s own
/// `SAFETY_UNSAFE_CODE`; the number is the wire contract between the
/// two.
pub const SAFETY_UNSAFE_CODE: i32 = -32010;

/// The MCP protocol revision this client speaks: the session-less
/// 2026-07-28 revision `rp` serves natively.
///
/// Pinned here (rather than inheriting rmcp's `ProtocolVersion::LATEST`)
/// so a Dependabot bump of rmcp cannot move first-party clients to a
/// revision `rp` has not been exercised against. Mirrors rmcp's
/// `ProtocolVersion::V_2026_07_28`; a unit test keeps the two equal.
pub const PROTOCOL_VERSION: &str = "2026-07-28";

const PINNED_VERSION: ProtocolVersion = ProtocolVersion::V_2026_07_28;

/// Failure to connect to `rp`.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The underlying HTTP client could not be built (bad CA path/PEM).
    #[error("building the HTTP client: {0}")]
    Http(#[from] rusty_photon_tls::error::TlsError),
    /// The Authorization header could not be constructed.
    #[error("building the Authorization header: {0}")]
    Header(String),
    /// The `server/discover` bootstrap failed: `rp` is unreachable, TLS
    /// rejected the connection, `rp` rejected the credential, or the
    /// server does not speak [`PROTOCOL_VERSION`].
    #[error("connecting to {url}: {message}")]
    Connect { url: String, message: String },
}

/// Failure of an individual MCP call.
#[derive(Debug, thiserror::Error)]
pub enum McpCallError {
    /// The request itself failed — transport loss or a JSON-RPC protocol
    /// error other than the safety refusal. `rp` is unreachable or
    /// unhealthy; the next call is as likely to fail.
    #[error("MCP request failed: {0}")]
    Request(String),
    /// `rp` refused the call because conditions are unsafe: a gated tool
    /// while the safety gate is closed (rp.md § Safety → In-Flight Tool
    /// Calls). On the wire this is JSON-RPC error [`SAFETY_UNSAFE_CODE`]
    /// with `data.reason = "safety"`; `monitor` is `data.monitor`, the
    /// unsafe monitor `rp` named (`None` when it could not). `rp` is
    /// healthy and ungated tools still answer — a consumer waits for
    /// safe conditions (`safety_changed`, `get_safety_status`) rather
    /// than retrying the call.
    #[error(
        "rp refused the call for safety (JSON-RPC error {SAFETY_UNSAFE_CODE}): {message}; \
         monitor: {}",
        monitor.as_deref().unwrap_or("unknown")
    )]
    SafetyStopped {
        /// `rp`'s one-line message.
        message: String,
        /// The unsafe monitor `rp` named, if any.
        monitor: Option<String>,
    },
    /// The call returned with the MCP `is_error` flag — a tool failure
    /// reported by a healthy `rp`.
    #[error("{0}")]
    Tool(String),
    /// The call returned, but the result violates the one-JSON-text-block
    /// convention (non-JSON text, non-text content, multiple blocks).
    /// `rp` answered — this is a malformed response, not a transport
    /// failure, and consumers that retry tool failures may treat it as
    /// one.
    #[error("malformed tool result: {0}")]
    Malformed(String),
}

/// One entry of the tool catalog.
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub input_schema: Value,
}

/// The identity this crate presents in every request's `_meta`: the
/// pinned [`PROTOCOL_VERSION`], no client capabilities (rp asks nothing
/// of its clients — no sampling, no roots, no elicitation), and this
/// crate's name and version as the `clientInfo`.
#[derive(Clone, Copy, Debug, Default)]
struct RpClientHandler;

impl ClientHandler for RpClientHandler {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        );
        info.protocol_version = PINNED_VERSION;
        info
    }
}

/// A connected MCP client for `rp`.
///
/// There is no session behind it (ADR-021): each request is
/// self-contained, so nothing expires and nothing is re-established
/// behind the consumer's back. That is a statement about protocol
/// state, not about cost: a standing client can keep a long-lived HTTP
/// connection open into `rp`, which stalls `rp`'s graceful stop — the
/// reason `ui-htmx` and `planetarium-bridge` connect per request or
/// per burst and drop the client when idle. A consumer that holds one
/// for the length of a run (`session-runner`, `calibrator-flats`)
/// drops it when the run ends. A consumer that loses `rp` (every call
/// answers [`McpCallError::Request`]) reconnects on its own terms; one
/// that is refused for safety ([`McpCallError::SafetyStopped`]) waits
/// for safe conditions rather than reconnecting.
pub struct RpMcpClient {
    peer: rmcp::Peer<rmcp::RoleClient>,
    // Keep the running service alive so the transport isn't dropped.
    _service: RunningService<rmcp::RoleClient, RpClientHandler>,
}

impl RpMcpClient {
    /// Connect to `rp`'s MCP endpoint, presenting `service_auth` per the
    /// credential policy (see the crate docs). The bootstrap is one
    /// `server/discover` negotiating [`PROTOCOL_VERSION`]; there is no
    /// `initialize` and no fallback to it.
    ///
    /// # Errors
    ///
    /// Returns a [`ConnectError`]: the HTTP client could not be built
    /// (bad CA path/PEM), the Authorization header could not be
    /// constructed, or the `server/discover` bootstrap failed
    /// (unreachable, TLS or credential rejection, or a server that does
    /// not speak [`PROTOCOL_VERSION`]).
    pub async fn connect(
        mcp_url: &str,
        service_auth: Option<&ClientAuthConfig>,
        ca_cert: Option<&Path>,
    ) -> Result<Self, ConnectError> {
        let http_client = rusty_photon_tls::client::build_reqwest_client(ca_cert)?;

        let mut config = StreamableHttpClientTransportConfig::with_uri(mcp_url.to_owned());
        if let Some(header) = basic_authorization(mcp_url, service_auth, ca_cert)? {
            config =
                config.custom_headers(std::collections::HashMap::<HeaderName, HeaderValue>::from(
                    [(AUTHORIZATION, header)],
                ));
        }

        debug!(url = %mcp_url, protocol = PROTOCOL_VERSION, "connecting MCP client");
        let transport = StreamableHttpClientTransport::with_client(http_client, config);
        let service = RpClientHandler
            .serve_with_lifecycle(
                transport,
                ClientLifecycleMode::Discover {
                    preferred_versions: vec![PINNED_VERSION],
                },
            )
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

    /// The protocol revision negotiated with `rp` — [`PROTOCOL_VERSION`]
    /// for a connected client. `None` only if discovery left no peer
    /// info behind, which a successful [`connect`](Self::connect) never
    /// does.
    #[must_use]
    pub fn protocol_version(&self) -> Option<String> {
        self.peer
            .peer_info()
            .map(|info| info.protocol_version.as_str().to_owned())
    }

    /// Call a tool and parse the result per the rp convention: no content
    /// is `null`, one JSON text block is the parsed value, anything else
    /// is a loud failure.
    ///
    /// # Errors
    ///
    /// Returns [`McpCallError::SafetyStopped`] if `rp` refused the call
    /// because conditions are unsafe, [`McpCallError::Request`] if the
    /// request itself fails otherwise (`rp` unreachable, transport
    /// loss), [`McpCallError::Tool`] if the tool reports an error, and
    /// [`McpCallError::Malformed`] if the result violates the
    /// one-JSON-text-block convention.
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
            .map_err(map_service_error)?;

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
    ///
    /// # Errors
    ///
    /// Returns [`McpCallError::Request`] if the listing request fails
    /// (`rp` unreachable, transport loss).
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

/// The credential policy, shared by every connection to an rp.
///
/// An `Authorization: Basic …` header is produced only when a credential
/// **and** a CA are configured **and** the URL is HTTPS. Any other
/// combination with a credential present warns loudly and produces
/// `None` — plaintext credentials never travel over cleartext or
/// unverified channels.
///
/// Public so a consumer's *other* connections to the same rp (an SSE
/// subscription, a completion POST) apply the identical policy instead of
/// re-deriving it. The returned header is marked sensitive.
///
/// # Errors
///
/// Returns [`ConnectError::Header`] if the assembled value is rejected
/// as an HTTP header — a defensive arm: the value is built from base64
/// output, so in practice it does not trigger.
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

/// A failed request: `rp`'s safety refusal (JSON-RPC error
/// [`SAFETY_UNSAFE_CODE`]) is the one request-level failure that does
/// not mean `rp` is unreachable or unhealthy, so it gets its own
/// variant; everything else is [`McpCallError::Request`].
fn map_service_error(err: ServiceError) -> McpCallError {
    match err {
        ServiceError::McpError(data) if data.code.0 == SAFETY_UNSAFE_CODE => {
            let monitor = data
                .data
                .as_ref()
                .and_then(|d| d.get("monitor"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            McpCallError::SafetyStopped {
                message: data.message.into_owned(),
                monitor,
            }
        }
        other => McpCallError::Request(other.to_string()),
    }
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

    /// The wire contract: rp's `-32010` with `data.monitor` becomes the
    /// dedicated variant, naming the monitor.
    #[test]
    fn the_safety_unsafe_error_code_maps_to_safety_stopped() {
        let err = map_service_error(ServiceError::McpError(rmcp::model::ErrorData::new(
            rmcp::model::ErrorCode(SAFETY_UNSAFE_CODE),
            "safety: conditions are unsafe",
            Some(serde_json::json!({"reason": "safety", "monitor": "weather-watcher"})),
        )));
        let McpCallError::SafetyStopped { message, monitor } = err else {
            panic!("expected SafetyStopped, got: {err:?}");
        };
        assert_eq!(message, "safety: conditions are unsafe");
        assert_eq!(monitor.as_deref(), Some("weather-watcher"));
    }

    #[test]
    fn a_safety_refusal_without_a_monitor_names_none() {
        let err = map_service_error(ServiceError::McpError(rmcp::model::ErrorData::new(
            rmcp::model::ErrorCode(SAFETY_UNSAFE_CODE),
            "safety: conditions are unsafe",
            Some(serde_json::json!({"reason": "safety", "monitor": null})),
        )));
        assert!(
            matches!(err, McpCallError::SafetyStopped { monitor: None, .. }),
            "got: {err:?}"
        );
        assert!(err.to_string().contains("-32010"), "got: {err}");
        assert!(err.to_string().contains("monitor: unknown"), "got: {err}");
    }

    /// The public constant and the pinned rmcp version are one value:
    /// consumers and probes name the revision by the string, the
    /// transport by the type.
    #[test]
    fn the_pinned_protocol_version_is_2026_07_28() {
        assert_eq!(PROTOCOL_VERSION, PINNED_VERSION.as_str());
        assert_eq!(PROTOCOL_VERSION, "2026-07-28");
    }

    /// The identity every request carries: the pinned revision, no
    /// capabilities, this crate as the client.
    #[test]
    fn the_client_info_pins_the_protocol_version_and_names_the_crate() {
        let info = RpClientHandler.get_info();
        assert_eq!(info.protocol_version, PINNED_VERSION);
        assert_eq!(info.client_info.name, env!("CARGO_PKG_NAME"));
        assert_eq!(info.client_info.version, env!("CARGO_PKG_VERSION"));
    }

    /// Every other JSON-RPC error is still a request failure.
    #[test]
    fn another_json_rpc_error_is_a_request_failure() {
        let err = map_service_error(ServiceError::McpError(rmcp::model::ErrorData::new(
            rmcp::model::ErrorCode::METHOD_NOT_FOUND,
            "no such method",
            None,
        )));
        assert!(matches!(err, McpCallError::Request(_)), "got: {err:?}");
    }

    #[test]
    fn a_closed_transport_is_a_request_failure() {
        let err = map_service_error(ServiceError::TransportClosed);
        assert!(matches!(err, McpCallError::Request(_)), "got: {err:?}");
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
