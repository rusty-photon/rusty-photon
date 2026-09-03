//! Persistent MCP test client, backed by the standard `rp-mcp-client`
//! crate (ADR-017).
//!
//! [`McpTestClient`] wraps a single MCP session. It is created once per
//! scenario (typically in an "MCP client connected to rp" Given step) and
//! reused for all tool calls within that scenario — mirroring how a real
//! MCP client (e.g. calibrator-flats) works. [`McpTestClient::connect_authed`]
//! is the TLS + Basic-auth variant for the scenarios that prove rp's /mcp
//! honors the server-wide `server.tls` / `server.auth`.

use std::path::Path;
use std::time::Duration;

use rp_mcp_client::{ClientAuthConfig, McpCallError, RpMcpClient};
use serde_json::Value;

/// Upper bound on a single MCP request (`call_tool` / `list_tools`).
///
/// rmcp's `Peer::call_tool` has no built-in client timeout: if an rp tool
/// handler never returns, the `await` here hangs *forever*. Two centering-BDD
/// hangs traced to that: `do_capture` looping on a failed `sky-survey-camera`
/// exposure (fixed in `services/rp/src/mcp/internals.rs::do_capture`), and —
/// issue #319 — a per-iteration mount read stalling because rp's Alpaca client
/// had no per-request timeout (fixed in rp's `equipment::alpaca`). This timeout
/// is a defense-in-depth backstop: any *future* handler hang fails the scenario
/// fast and legibly here, rather than hanging the BDD binary until the CI
/// job's `timeout-minutes` kills it.
///
/// 360 s sits comfortably above rp's worst-case single blocking operation
/// (the 300 s slew/park deadlines), so a genuinely slow-but-progressing call
/// still completes and a *server-produced* error still propagates with its
/// real message; only a true hang trips this bound.
const MCP_CALL_TIMEOUT: Duration = Duration::from_mins(6);

/// A persistent MCP client backed by a single session.
pub struct McpTestClient {
    client: RpMcpClient,
}

impl McpTestClient {
    /// Connect to an MCP server over plain HTTP and perform the initialize
    /// handshake.
    ///
    /// # Errors
    ///
    /// Returns an `MCP connect:` message if the HTTP client cannot be built
    /// or the initialize handshake fails (server unreachable, or it refuses
    /// the session).
    pub async fn connect(mcp_url: &str) -> Result<Self, String> {
        let client = RpMcpClient::connect(mcp_url, None, None)
            .await
            .map_err(|e| format!("MCP connect: {e}"))?;
        Ok(Self { client })
    }

    /// Connect over TLS trusting the scenario CA but presenting no
    /// credentials — the client for proving an auth-enabled rp refuses an
    /// unauthenticated MCP session (as opposed to a TLS trust failure).
    ///
    /// # Errors
    ///
    /// Returns an `MCP connect:` message if `ca_cert` cannot be read or
    /// parsed, or if the initialize handshake fails — which is how an
    /// auth-enabled rp's refusal surfaces.
    pub async fn connect_tls(mcp_url: &str, ca_cert: &Path) -> Result<Self, String> {
        let client = RpMcpClient::connect(mcp_url, None, Some(ca_cert))
            .await
            .map_err(|e| format!("MCP connect: {e}"))?;
        Ok(Self { client })
    }

    /// Connect over TLS presenting HTTP Basic credentials — the
    /// authenticated path the `mcp_auth` scenarios exercise. `ca_cert` is the
    /// scenario CA (e.g. `PkiFixture::ca_path`); per the ADR-017 policy the
    /// credential is only sent because the CA is given and the URL is https.
    ///
    /// # Errors
    ///
    /// Returns an `MCP connect:` message if `ca_cert` cannot be read or
    /// parsed, if the credentials cannot form an `Authorization` header, or
    /// if the initialize handshake fails (unreachable, TLS rejection, or a
    /// wrong password surfacing as a failed handshake).
    pub async fn connect_authed(
        mcp_url: &str,
        username: &str,
        password: &str,
        ca_cert: &Path,
    ) -> Result<Self, String> {
        let auth = ClientAuthConfig {
            username: username.to_owned(),
            password: password.to_owned(),
        };
        let client = RpMcpClient::connect(mcp_url, Some(&auth), Some(ca_cert))
            .await
            .map_err(|e| format!("MCP connect: {e}"))?;
        Ok(Self { client })
    }

    /// Call an MCP tool and return the parsed JSON result or error message.
    ///
    /// # Errors
    ///
    /// Returns the tool's own message if rp reports the call as failed,
    /// rp-mcp-client's rendering of a safety refusal (naming the JSON-RPC
    /// code and the monitor) if rp refused the call for safety, a
    /// `<tool>:`-prefixed message if the request fails (dead session,
    /// transport loss) or the result is malformed, and a timeout message if
    /// no response arrives within `MCP_CALL_TIMEOUT`.
    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value, String> {
        let args = arguments.as_object().cloned().unwrap_or_default();
        let call = self.client.call_tool(tool_name, args);
        match tokio::time::timeout(MCP_CALL_TIMEOUT, call).await {
            Err(_) => Err(format!(
                "{}: MCP call timed out after {}s with no response — the rp MCP \
                 transport was almost certainly torn down mid-request (see MCP_CALL_TIMEOUT)",
                tool_name,
                MCP_CALL_TIMEOUT.as_secs()
            )),
            Ok(Err(McpCallError::Tool(message))) => Err(message),
            Ok(Err(refusal @ McpCallError::SafetyStopped { .. })) => Err(refusal.to_string()),
            Ok(Err(e @ (McpCallError::Request(_) | McpCallError::Malformed(_)))) => {
                Err(format!("{tool_name}: {e}"))
            }
            Ok(Ok(value)) => Ok(value),
        }
    }

    /// List all available MCP tools and return their names.
    ///
    /// # Errors
    ///
    /// Returns a message if the listing request fails (dead session,
    /// transport loss) or no response arrives within `MCP_CALL_TIMEOUT`.
    pub async fn list_tools(&self) -> Result<Vec<String>, String> {
        let tools = tokio::time::timeout(MCP_CALL_TIMEOUT, self.client.list_tools())
            .await
            .map_err(|_| {
                format!(
                    "list_tools: MCP call timed out after {}s with no response (see \
                     MCP_CALL_TIMEOUT)",
                    MCP_CALL_TIMEOUT.as_secs()
                )
            })?
            .map_err(|e| format!("list_tools: {e}"))?;

        Ok(tools.into_iter().map(|t| t.name).collect())
    }
}
