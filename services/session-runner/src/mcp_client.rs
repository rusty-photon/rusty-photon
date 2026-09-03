//! The MCP client — `rp`'s tool catalog for layer-2 validation and the
//! engine's [`ToolClient`] seam for execution.
//!
//! Built on the standard `rp-mcp-client` crate (ADR-017): CA-pinned
//! TLS, the observatory credential over verified HTTPS only, the
//! session-less 2026-07-28 revision (ADR-021).
//!
//! Error mapping — the three-way posture pinned in
//! `docs/services/session-runner.md` § Safety Behavior: a call that
//! *returns* with `is_error` — or with a result that violates the
//! one-JSON-text-block convention — is a tool failure, retryable and
//! catchable ([`ToolCallError::Failed`]), with one exception: the exact
//! text `cancelled: safety`, which is how `rp` answers a call its safety
//! enforcer cancelled (rp.md § Safety → In-Flight Tool Calls). That
//! cancellation and `rp`'s structured safety refusal of a gated call
//! while conditions stay unsafe (`SafetyUnsafe`, surfaced as
//! `McpCallError::SafetyStopped`) are the safety pause
//! ([`ToolCallError::SafetyStopped`]). Any **request-level failure** —
//! transport loss or a JSON-RPC protocol error — is an `rp` outage
//! ([`ToolCallError::Unavailable`]): `rp` reports ordinary tool failures
//! via `is_error` results, so a protocol error means `rp` itself is
//! unreachable or unhealthy, and the engine pauses until it is back.

use std::path::Path;

use rp_mcp_client::{ClientAuthConfig, McpCallError, RpMcpClient};
use serde_json::{Map, Value};
use tracing::debug;

use crate::document::ToolSpec;
use crate::engine::{ToolCallError, ToolClient};
use crate::error::{Result, SessionRunnerError};

/// The tool-error text `rp` answers with when its safety enforcer
/// cancelled the call (rp.md § Safety → In-Flight Tool Calls).
const SAFETY_CANCELLED: &str = "cancelled: safety";

/// The [`ToolClient`] error for one failed `rp` call — see the module
/// doc for the mapping.
pub(crate) fn map_call_error(err: McpCallError) -> ToolCallError {
    match err {
        McpCallError::Tool(message) if message == SAFETY_CANCELLED => {
            ToolCallError::SafetyStopped(message)
        }
        McpCallError::Tool(message) | McpCallError::Malformed(message) => {
            ToolCallError::Failed(message)
        }
        // The request itself failed: rp is unreachable or unhealthy.
        McpCallError::Request(message) => ToolCallError::Unavailable(message),
        // rp refused a gated call while conditions are unsafe: the run
        // pauses until they clear — never retried, never caught.
        refusal @ McpCallError::SafetyStopped { .. } => {
            ToolCallError::SafetyStopped(refusal.to_string())
        }
    }
}

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
    /// `server/discover` bootstrap fails.
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
    /// fails (`rp` unreachable, transport loss).
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
        self.inner
            .call_tool(tool, args)
            .await
            .map_err(map_call_error)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_tool_error_is_a_catchable_failure() {
        let err = map_call_error(McpCallError::Tool("filter not found: Ha".to_owned()));
        assert!(matches!(err, ToolCallError::Failed(m) if m == "filter not found: Ha"));
    }

    #[test]
    fn a_malformed_result_is_a_catchable_failure() {
        let err = map_call_error(McpCallError::Malformed("two content blocks".to_owned()));
        assert!(matches!(err, ToolCallError::Failed(_)));
    }

    #[test]
    fn a_request_failure_is_an_rp_outage() {
        let err = map_call_error(McpCallError::Request("connection reset".to_owned()));
        assert!(matches!(err, ToolCallError::Unavailable(m) if m == "connection reset"));
    }

    /// The safety enforcer's cancellation answers as a tool error with a
    /// fixed text; it takes the pause path, not `catch`.
    #[test]
    fn a_safety_cancellation_is_a_safety_pause() {
        let err = map_call_error(McpCallError::Tool(SAFETY_CANCELLED.to_owned()));
        assert!(matches!(err, ToolCallError::SafetyStopped(m) if m == SAFETY_CANCELLED));
    }

    /// rp's structured refusal of a gated call while unsafe takes the
    /// same path as the cancellation: pause, keep the blackboard.
    #[test]
    fn a_safety_refusal_is_a_safety_pause() {
        let err = map_call_error(McpCallError::SafetyStopped {
            message: "safety: conditions are unsafe".to_owned(),
            monitor: Some("weather-watcher".to_owned()),
        });
        let ToolCallError::SafetyStopped(message) = err else {
            panic!("expected SafetyStopped, got: {err:?}");
        };
        assert!(message.contains("weather-watcher"), "got: {message}");
    }

    /// A client's own disconnect is not safety; the engine never sees
    /// its own cancellation, but the mapping must not over-match.
    #[test]
    fn another_cancellation_reason_stays_a_tool_failure() {
        let err = map_call_error(McpCallError::Tool(
            "cancelled: client disconnected".to_owned(),
        ));
        assert!(matches!(err, ToolCallError::Failed(_)));
    }
}
