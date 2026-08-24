//! The MCP client — `rp`'s tool catalog for layer-2 validation and the
//! engine's [`ToolClient`] seam for execution.
//!
//! Built on the standard `rp-mcp-client` crate (ADR-017): CA-pinned
//! TLS, the observatory credential over verified HTTPS only, no
//! transparent session re-establishment.
//!
//! Error mapping (pinned in `docs/services/session-runner.md` § Safety
//! Behavior): a call that *returns* with `is_error` — or with a result
//! that violates the one-JSON-text-block convention — is a tool failure,
//! retryable and catchable ([`ToolCallError::Failed`]). Any
//! **request-level failure** — transport loss or a JSON-RPC protocol
//! error — is treated as a terminated MCP session
//! ([`ToolCallError::SessionTerminated`]): `rp` tearing the session down
//! on a safety transition presents exactly this way, `rp` reports tool
//! failures via `is_error` results (so a protocol error means `rp` itself
//! is unhealthy), and the engine's response (best-effort `finally`,
//! persist, exit without completion, await re-invocation) is the safest
//! generic recovery.

use std::path::Path;

use rp_mcp_client::{ClientAuthConfig, McpCallError, RpMcpClient};
use serde_json::{Map, Value};
use tracing::debug;

use crate::document::ToolSpec;
use crate::engine::{ToolCallError, ToolClient};
use crate::error::{Result, SessionRunnerError};

/// MCP client for one `rp` session.
pub struct McpClient {
    inner: RpMcpClient,
}

impl McpClient {
    /// Connect to `rp`'s MCP server at the given URL, presenting
    /// `service_auth` per the ADR-017 credential policy.
    ///
    /// # Errors
    ///
    /// Returns [`SessionRunnerError::Mcp`] naming the URL if the
    /// connection fails — the HTTP client cannot be built (bad CA path
    /// or PEM), the Authorization header cannot be constructed, or the
    /// MCP initialize handshake fails.
    pub async fn connect(
        mcp_url: &str,
        service_auth: Option<&ClientAuthConfig>,
        ca_cert: Option<&Path>,
    ) -> Result<Self> {
        debug!(url = %mcp_url, "connecting MCP client");
        let inner = RpMcpClient::connect(mcp_url, service_auth, ca_cert)
            .await
            .map_err(|e| SessionRunnerError::Mcp(format!("connect to {mcp_url}: {e}")))?;
        Ok(Self { inner })
    }

    /// `tools/list` → the catalog for layer-2 validation.
    ///
    /// # Errors
    ///
    /// Returns [`SessionRunnerError::Mcp`] if the `tools/list` request
    /// fails (dead session, transport loss).
    pub async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        let tools = self
            .inner
            .list_tools()
            .await
            .map_err(|e| SessionRunnerError::Mcp(format!("tools/list: {e}")))?;
        Ok(tools
            .into_iter()
            .map(|tool| ToolSpec {
                name: tool.name,
                input_schema: tool.input_schema,
            })
            .collect())
    }
}

impl ToolClient for McpClient {
    async fn call(
        &self,
        tool: &str,
        args: Map<String, Value>,
    ) -> std::result::Result<Value, ToolCallError> {
        match self.inner.call_tool(tool, args).await {
            Ok(value) => Ok(value),
            Err(McpCallError::Tool(message) | McpCallError::Malformed(message)) => {
                Err(ToolCallError::Failed(message))
            }
            // The request itself failed: the session is unusable.
            Err(McpCallError::Request(message)) => Err(ToolCallError::SessionTerminated(message)),
        }
    }
}
