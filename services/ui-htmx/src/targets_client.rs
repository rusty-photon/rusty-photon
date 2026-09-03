//! The targets-inbox seam onto rp's MCP-only target tools
//! (`docs/services/ui-htmx.md` "Targets inbox").
//!
//! `McpTargetsClient` drives `rp-mcp-client` (ADR-017) with **per-request
//! clients**: every call connects, runs its tool, and drops the client.
//! The BFF never holds a standing MCP client — an idle rmcp client holds
//! an open POST that stalls rp's graceful stop (the transport is
//! session-less, ADR-021, so that is the only reason left). Same
//! philosophy as the config pages' `Connection: close`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use rp_mcp_client::{ClientAuthConfig, McpCallError, RpMcpClient};
use serde_json::{Map, Value};

/// A target-tool failure, split the way the pages render it.
#[derive(Debug, thiserror::Error)]
pub enum TargetsError {
    /// The client could not connect or the request itself failed — rp
    /// is down or unreachable — or rp refused the call for safety.
    /// The target tools are ungated by default (rp.md § Safety →
    /// In-Flight Tool Calls: reads and target-store writes answer while
    /// conditions are unsafe), so the refusal only arrives when an
    /// operator's `safety.gate` gated them; either way the page renders
    /// one honest unavailable card carrying the detail.
    #[error("rp's target tools are unavailable: {0}")]
    Unavailable(String),
    /// rp is healthy and rejected the call — validation surfaced to the
    /// operator (e.g. a position angle out of `[0, 360)`).
    #[error("{0}")]
    Tool(String),
    /// The call returned but violated the one-JSON-text-block convention.
    #[error("malformed rp response: {0}")]
    Malformed(String),
}

impl From<McpCallError> for TargetsError {
    fn from(err: McpCallError) -> Self {
        match err {
            McpCallError::Request(msg) => Self::Unavailable(msg),
            McpCallError::Tool(msg) => Self::Tool(msg),
            McpCallError::Malformed(msg) => Self::Malformed(msg),
            refusal @ McpCallError::SafetyStopped { .. } => Self::Unavailable(refusal.to_string()),
        }
    }
}

/// The mockable seam the targets pages render through.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait TargetsClient: Send + Sync {
    /// `list_targets` without a filter — the flattened rows (target fields
    /// plus `progress`), both pending and active.
    async fn list_targets(&self) -> Result<Vec<Value>, TargetsError>;
    /// `get_target` — the `{target, progress}` nesting's `target`.
    async fn get_target(&self, slug: &str) -> Result<Value, TargetsError>;
    /// `update_target` with `slug` plus the given field subset. A key
    /// mapped to JSON `null` rides the wire as an explicit null — how the
    /// position angle clears back to inherit.
    async fn update_target(
        &self,
        slug: &str,
        fields: Map<String, Value>,
    ) -> Result<(), TargetsError>;
    /// `set_goals` — atomic replacement of the goal list.
    async fn set_goals(&self, slug: &str, goals: Vec<Value>) -> Result<(), TargetsError>;
    /// `delete_target`.
    async fn delete_target(&self, slug: &str) -> Result<(), TargetsError>;
}

/// The production client: rp's `/mcp` endpoint, one connection per call.
pub struct McpTargetsClient {
    mcp_url: String,
    service_auth: Option<ClientAuthConfig>,
    ca_cert_path: Option<PathBuf>,
}

impl McpTargetsClient {
    /// Build from the BFF's `rp` target block: `base_url` + `/mcp`, the
    /// same credential and CA the REST legs use. The ADR-017 credential
    /// policy applies at connect time (Basic only over verified HTTPS).
    #[must_use]
    pub fn new(
        base_url: &str,
        auth: Option<&crate::config::DriverAuth>,
        ca_cert_path: Option<PathBuf>,
    ) -> Self {
        Self {
            mcp_url: format!("{}/mcp", base_url.trim_end_matches('/')),
            service_auth: auth.map(|a| ClientAuthConfig {
                username: a.username.clone(),
                password: a.password.clone(),
            }),
            ca_cert_path,
        }
    }

    async fn session(&self) -> Result<RpMcpClient, TargetsError> {
        RpMcpClient::connect(
            &self.mcp_url,
            self.service_auth.as_ref(),
            self.ca_cert_path.as_deref(),
        )
        .await
        .map_err(|e| TargetsError::Unavailable(e.to_string()))
    }

    async fn call(&self, tool: &str, args: Map<String, Value>) -> Result<Value, TargetsError> {
        let session = self.session().await?;
        Ok(session.call_tool(tool, args).await?)
    }
}

fn slug_args(slug: &str) -> Map<String, Value> {
    let mut args = Map::new();
    args.insert("slug".to_string(), Value::String(slug.to_string()));
    args
}

#[async_trait]
impl TargetsClient for McpTargetsClient {
    async fn list_targets(&self) -> Result<Vec<Value>, TargetsError> {
        let result = self.call("list_targets", Map::new()).await?;
        result
            .get("targets")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| {
                TargetsError::Malformed(format!("list_targets without a targets array: {result}"))
            })
    }

    async fn get_target(&self, slug: &str) -> Result<Value, TargetsError> {
        let result = self.call("get_target", slug_args(slug)).await?;
        result.get("target").cloned().ok_or_else(|| {
            TargetsError::Malformed(format!("get_target without a target object: {result}"))
        })
    }

    async fn update_target(
        &self,
        slug: &str,
        fields: Map<String, Value>,
    ) -> Result<(), TargetsError> {
        let mut args = slug_args(slug);
        args.extend(fields);
        self.call("update_target", args).await.map(|_| ())
    }

    async fn set_goals(&self, slug: &str, goals: Vec<Value>) -> Result<(), TargetsError> {
        let mut args = slug_args(slug);
        args.insert("goals".to_string(), Value::Array(goals));
        self.call("set_goals", args).await.map(|_| ())
    }

    async fn delete_target(&self, slug: &str) -> Result<(), TargetsError> {
        self.call("delete_target", slug_args(slug))
            .await
            .map(|_| ())
    }
}

/// Test-state default: every call reports rp's target tools unwired. The
/// `with_rp_parts` test constructor installs this so unit tests that never
/// touch `/targets` need no stub; targets-page unit tests inject a mock via
/// `AppState::with_targets_client`.
pub(crate) struct UnwiredTargets;

#[async_trait]
impl TargetsClient for UnwiredTargets {
    async fn list_targets(&self) -> Result<Vec<Value>, TargetsError> {
        Err(TargetsError::Unavailable("no targets client wired".into()))
    }

    async fn get_target(&self, _slug: &str) -> Result<Value, TargetsError> {
        Err(TargetsError::Unavailable("no targets client wired".into()))
    }

    async fn update_target(
        &self,
        _slug: &str,
        _fields: Map<String, Value>,
    ) -> Result<(), TargetsError> {
        Err(TargetsError::Unavailable("no targets client wired".into()))
    }

    async fn set_goals(&self, _slug: &str, _goals: Vec<Value>) -> Result<(), TargetsError> {
        Err(TargetsError::Unavailable("no targets client wired".into()))
    }

    async fn delete_target(&self, _slug: &str) -> Result<(), TargetsError> {
        Err(TargetsError::Unavailable("no targets client wired".into()))
    }
}

/// Shared by the page handlers: an `Arc`'d client, cheap to clone into
/// handler futures.
pub(crate) type SharedTargetsClient = Arc<dyn TargetsClient>;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::DriverAuth;

    #[test]
    fn the_client_shapes_the_mcp_url_and_maps_the_credential() {
        let auth = DriverAuth {
            username: "svc".to_string(),
            password: "pw".to_string(),
        };
        let client = McpTargetsClient::new("https://rp.example:11115/", Some(&auth), None);
        assert_eq!(client.mcp_url, "https://rp.example:11115/mcp");
        let mapped = client.service_auth.unwrap();
        assert_eq!(mapped.username, "svc");
        assert_eq!(mapped.password, "pw");

        let bare = McpTargetsClient::new("http://127.0.0.1:11115", None, None);
        assert_eq!(bare.mcp_url, "http://127.0.0.1:11115/mcp");
        assert!(bare.service_auth.is_none());
    }

    #[test]
    fn call_errors_map_onto_the_three_page_states() {
        assert!(matches!(
            TargetsError::from(McpCallError::Request("gone".to_string())),
            TargetsError::Unavailable(_)
        ));
        assert!(matches!(
            TargetsError::from(McpCallError::Tool("rejected".to_string())),
            TargetsError::Tool(_)
        ));
        assert!(matches!(
            TargetsError::from(McpCallError::Malformed("two blocks".to_string())),
            TargetsError::Malformed(_)
        ));
        // rp's safety refusal is the unavailable card too, carrying the
        // monitor in its detail.
        assert!(matches!(
            TargetsError::from(McpCallError::SafetyStopped {
                message: "safety: conditions are unsafe".to_string(),
                monitor: Some("weather-watcher".to_string()),
            }),
            TargetsError::Unavailable(detail) if detail.contains("weather-watcher")
        ));
    }

    #[tokio::test]
    async fn the_unwired_default_reports_every_call_unavailable() {
        // `with_rp_parts` installs this so a test state that never touches
        // /targets needs no stub; each method must present as the honest
        // unavailable card, never a panic.
        let unwired = UnwiredTargets;
        assert!(matches!(
            unwired.list_targets().await,
            Err(TargetsError::Unavailable(_))
        ));
        assert!(matches!(
            unwired.get_target("x").await,
            Err(TargetsError::Unavailable(_))
        ));
        assert!(matches!(
            unwired.update_target("x", Map::new()).await,
            Err(TargetsError::Unavailable(_))
        ));
        assert!(matches!(
            unwired.set_goals("x", Vec::new()).await,
            Err(TargetsError::Unavailable(_))
        ));
        assert!(matches!(
            unwired.delete_target("x").await,
            Err(TargetsError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn an_unreachable_rp_maps_the_connect_failure_to_unavailable() {
        // Port 1 refuses deterministically (the repo's unreachable-target
        // convention) — the production connect path must surface it as the
        // Unavailable page state, exercising `session()`'s error mapping.
        let client = McpTargetsClient::new("http://127.0.0.1:1", None, None);
        assert!(matches!(
            client.list_targets().await,
            Err(TargetsError::Unavailable(_))
        ));
    }
}
