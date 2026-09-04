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

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use reqwest::header::{HeaderName, HeaderValue, AUTHORIZATION};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo,
    ClientRequest, ErrorData, Implementation, ProgressNotificationParam, ProgressToken,
    ProtocolVersion, ServerResult,
};
use rmcp::service::ServiceError;
use rmcp::service::{NotificationContext, PeerRequestOptions, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientHandler, ClientLifecycleMode, ClientServiceExt, RoleClient};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

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

/// Failure of a forwarded call ([`RpMcpClient::call_tool_forwarding`]).
///
/// Unlike [`McpCallError`], a tool failure is not a variant: the
/// `CallToolResult` comes back verbatim, `is_error` and all, because a
/// proxy relays what the server said rather than interpreting it.
#[derive(Debug, thiserror::Error)]
pub enum ProxyCallError {
    /// The caller's cancellation future resolved before the server
    /// answered; `notifications/cancelled` was sent for the request.
    #[error("cancelled")]
    Cancelled,
    /// The request itself failed — transport loss, or the server
    /// answered something other than a tool result.
    #[error("MCP request failed: {0}")]
    Request(String),
    /// The server answered with a JSON-RPC error (invalid params, an
    /// unknown tool, its own safety refusal, ...): relayed as-is so the
    /// proxy can hand it to its own caller unchanged.
    #[error("MCP error {}: {}", .0.code.0, .0.message)]
    Protocol(ErrorData),
}

/// One entry of the tool catalog.
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub input_schema: Value,
}

/// Where a `notifications/progress` from the server goes: the sender
/// registered for its `progressToken` by an in-flight
/// [`RpMcpClient::call_tool_forwarding`], or nowhere.
///
/// rmcp mints a fresh progress token for every request the client
/// sends (it overwrites whatever `_meta` carried), so a token names
/// exactly one in-flight request and the map needs no further key.
#[derive(Default)]
struct ProgressRoutes {
    routes: Mutex<HashMap<ProgressToken, mpsc::UnboundedSender<ProgressNotificationParam>>>,
}

impl ProgressRoutes {
    /// Route notifications carrying `token` to `sender` until the
    /// returned guard drops.
    fn register(
        self: &Arc<Self>,
        token: ProgressToken,
        sender: mpsc::UnboundedSender<ProgressNotificationParam>,
    ) -> ProgressRoute {
        self.lock().insert(token.clone(), sender);
        ProgressRoute {
            routes: Arc::clone(self),
            token,
        }
    }

    /// Deliver one notification to its route; a token nobody registered
    /// (the request already finished, or never asked for progress) is
    /// dropped after a debug log.
    fn dispatch(&self, notification: ProgressNotificationParam) {
        let token = notification.progress_token.clone();
        let sender = self.lock().get(&token).cloned();
        let delivered = sender.is_some_and(|sender| sender.send(notification).is_ok());
        if !delivered {
            debug!(
                ?token,
                "progress notification for no in-flight forwarded call; dropped"
            );
        }
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<
        '_,
        HashMap<ProgressToken, mpsc::UnboundedSender<ProgressNotificationParam>>,
    > {
        // A poisoned lock only means a panic elsewhere while holding it;
        // the map itself is still consistent.
        self.routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Unregisters its progress route on drop.
struct ProgressRoute {
    routes: Arc<ProgressRoutes>,
    token: ProgressToken,
}

impl Drop for ProgressRoute {
    fn drop(&mut self) {
        self.routes.lock().remove(&self.token);
    }
}

/// The identity this crate presents in every request's `_meta`: the
/// pinned [`PROTOCOL_VERSION`], no client capabilities (rp asks nothing
/// of its clients — no sampling, no roots, no elicitation), and this
/// crate's name and version as the `clientInfo`. Also the receiver of
/// the server's `notifications/progress`, routed to the forwarded call
/// they belong to.
#[derive(Clone, Default)]
struct RpClientHandler {
    progress: Arc<ProgressRoutes>,
}

impl ClientHandler for RpClientHandler {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        );
        info.protocol_version = PINNED_VERSION;
        info
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.progress.dispatch(params);
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
    /// The progress routes the handler dispatches into; a forwarded
    /// call registers its token here for its lifetime.
    progress: Arc<ProgressRoutes>,
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
        let handler = RpClientHandler::default();
        let progress = Arc::clone(&handler.progress);
        let service = handler
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
            progress,
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

    /// Forward a `tools/call` on behalf of another caller — the proxy
    /// half of rp's tool-provider aggregation (rp.md § Plugin-Provided
    /// Tools).
    ///
    /// `params` goes out as given: arguments and `_meta` alike (rmcp
    /// then overlays the protocol fields this client always sends and
    /// its own `progressToken`, so a caller's token is never forwarded
    /// as-is). The result comes back **verbatim** — a tool error stays
    /// a `CallToolResult` with `is_error` set, and a JSON-RPC error is
    /// [`ProxyCallError::Protocol`] — because a proxy relays what the
    /// server said. Every `notifications/progress` the server emits for
    /// this request is sent to `progress` while the call is in flight;
    /// `None` drops them.
    ///
    /// `cancel` is the caller's own cancellation: when it resolves
    /// before the server answers, `notifications/cancelled` is sent for
    /// the request with the string it resolved to as the reason, and
    /// the call returns [`ProxyCallError::Cancelled`].
    ///
    /// # Errors
    ///
    /// Returns [`ProxyCallError::Cancelled`] if `cancel` resolved first,
    /// [`ProxyCallError::Protocol`] if the server answered with a
    /// JSON-RPC error, and [`ProxyCallError::Request`] if the request
    /// failed otherwise (transport loss, or a response that is not a
    /// tool result).
    pub async fn call_tool_forwarding(
        &self,
        params: CallToolRequestParams,
        progress: Option<mpsc::UnboundedSender<ProgressNotificationParam>>,
        cancel: impl Future<Output = String>,
    ) -> Result<CallToolResult, ProxyCallError> {
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let mut handle = self
            .peer
            .send_cancellable_request(request, PeerRequestOptions::no_options())
            .await
            .map_err(|e| ProxyCallError::Request(e.to_string()))?;
        // Registered after the send so the token is rmcp's; the request
        // has only been queued for the transport at this point, so no
        // notification can have arrived yet.
        let _route = progress.map(|sender| {
            self.progress
                .register(handle.progress_token.clone(), sender)
        });
        tokio::select! {
            response = &mut handle.rx => match response {
                Ok(Ok(ServerResult::CallToolResult(result))) => Ok(result),
                Ok(Ok(other)) => Err(ProxyCallError::Request(format!(
                    "unexpected response to tools/call: {other:?}"
                ))),
                Ok(Err(ServiceError::McpError(data))) => Err(ProxyCallError::Protocol(data)),
                Ok(Err(e)) => Err(ProxyCallError::Request(e.to_string())),
                Err(_) => Err(ProxyCallError::Request("transport closed".to_owned())),
            },
            reason = cancel => {
                debug!(reason = %reason, "cancelling forwarded tools/call");
                if let Err(e) = handle.cancel(Some(reason)).await {
                    debug!(error = %e, "notifications/cancelled could not be sent");
                }
                Err(ProxyCallError::Cancelled)
            }
        }
    }

    /// `tools/list` — the full catalog, as rmcp's own `Tool` records
    /// (name, description, schemas, annotations), for a consumer that
    /// re-advertises them.
    ///
    /// # Errors
    ///
    /// Returns [`McpCallError::Request`] if the listing request fails
    /// (the server unreachable, transport loss).
    pub async fn list_tool_records(&self) -> Result<Vec<rmcp::model::Tool>, McpCallError> {
        self.peer
            .list_all_tools()
            .await
            .map_err(|e| McpCallError::Request(format!("tools/list: {e}")))
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
        let info = RpClientHandler::default().get_info();
        assert_eq!(info.protocol_version, PINNED_VERSION);
        assert_eq!(info.client_info.name, env!("CARGO_PKG_NAME"));
        assert_eq!(info.client_info.version, env!("CARGO_PKG_VERSION"));
    }

    fn token(n: i64) -> ProgressToken {
        ProgressToken(rmcp::model::NumberOrString::Number(n))
    }

    fn progress(token: ProgressToken, progress: f64) -> ProgressNotificationParam {
        ProgressNotificationParam::new(token, progress)
    }

    /// A notification reaches the sender registered for its token and
    /// no other; a token nobody registered is dropped, not an error.
    #[test]
    fn progress_is_routed_by_token() {
        let routes = Arc::new(ProgressRoutes::default());
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        let _a = routes.register(token(1), tx_a);
        let _b = routes.register(token(2), tx_b);

        routes.dispatch(progress(token(1), 0.5));
        routes.dispatch(progress(token(3), 0.9));

        assert_eq!(rx_a.try_recv().unwrap().progress, 0.5);
        assert!(rx_a.try_recv().is_err(), "only its own notification");
        assert!(rx_b.try_recv().is_err(), "nothing for the other token");
    }

    /// Dropping the route guard ends the routing — the forwarded call
    /// returned, and a late notification must not reach a closed call.
    #[test]
    fn a_dropped_route_receives_nothing_more() {
        let routes = Arc::new(ProgressRoutes::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let route = routes.register(token(7), tx);
        routes.dispatch(progress(token(7), 0.1));
        drop(route);
        routes.dispatch(progress(token(7), 0.2));
        assert_eq!(rx.try_recv().unwrap().progress, 0.1);
        assert!(
            rx.try_recv().is_err(),
            "the second notification was routed after unregister"
        );
        assert!(routes.lock().is_empty());
    }

    /// A forwarded call against nothing fails as a request failure,
    /// not a panic, and the error renders the cause.
    #[test]
    fn proxy_call_errors_render() {
        assert_eq!(ProxyCallError::Cancelled.to_string(), "cancelled");
        assert_eq!(
            ProxyCallError::Request("gone".to_owned()).to_string(),
            "MCP request failed: gone"
        );
        let protocol = ProxyCallError::Protocol(ErrorData::new(
            rmcp::model::ErrorCode::INVALID_PARAMS,
            "bad args",
            None,
        ));
        assert_eq!(protocol.to_string(), "MCP error -32602: bad args");
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
