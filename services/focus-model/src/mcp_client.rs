//! The provider's client half: `rp`'s built-in tools, cancellable.
//!
//! Built on the standard `rp-mcp-client` crate (ADR-017 — CA-pinned
//! TLS and the observatory credential over verified HTTPS only), with
//! every call cancellable by the tool request's token.
//!
//! A cancelled call does not merely stop waiting: `notifications/cancelled`
//! goes to `rp` for the in-flight request, so an exposure or a move in
//! progress is abandoned rather than finished into the void. The
//! put-back after a cancellation runs through
//! [`McpClient::uncancellable`], the same connection under a token
//! nothing fires.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rp_mcp_client::{ProxyCallError, RpMcpClient};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::config::Config;
use crate::error::{FocusModelError, Result};
use crate::workflow::{
    CaptureResult, FocusRig, FocuserPosition, GuideFocusResult, RefocusPlan, StarMeasurement,
    TrainInfo,
};

/// The reason sent to `rp` with `notifications/cancelled`, and carried
/// by [`FocusModelError::Cancelled`].
pub const CANCEL_REASON: &str = "the caller cancelled the focus run";

/// A connection to `rp` bound to one cancellation token.
pub struct McpClient {
    inner: Arc<RpMcpClient>,
    cancel: CancellationToken,
}

/// Result of `get_focuser_temperature`.
#[derive(Debug, Clone, Deserialize)]
struct TemperatureResult {
    #[serde(default)]
    temperature_c: Option<f64>,
}

/// Result of `get_filter`.
#[derive(Debug, Clone, Deserialize)]
struct FilterResult {
    #[serde(default)]
    filter_name: Option<String>,
}

/// Result of `get_guiding_stats`; only the loop's state is read.
#[derive(Debug, Clone, Deserialize)]
struct GuidingStats {
    #[serde(default)]
    guiding: bool,
}

impl McpClient {
    /// Connect to `rp` at the configured `mcp_server_url`, presenting
    /// `service_auth` per the ADR-017 credential policy. Calls made
    /// through the returned client are cancelled when `cancel` fires.
    ///
    /// # Errors
    ///
    /// Returns [`FocusModelError::ToolCall`] if the connection fails —
    /// the HTTP client cannot be built (bad CA path or PEM), the
    /// Authorization header cannot be constructed, or the MCP
    /// bootstrap fails.
    pub async fn connect(config: &Config, cancel: CancellationToken) -> Result<Self> {
        debug!(url = %config.mcp_server_url, "connecting to rp");
        let inner = RpMcpClient::connect(&config.mcp_server_url, config.rp_auth(), config.rp_ca())
            .await
            .map_err(|e| {
                FocusModelError::ToolCall(format!(
                    "rp at {} is unreachable: {e}",
                    config.mcp_server_url
                ))
            })?;
        Ok(Self {
            inner: Arc::new(inner),
            cancel,
        })
    }

    /// The same connection under a token that never fires — for the
    /// put-back a cancellation must not reach.
    #[must_use]
    pub fn uncancellable(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            cancel: CancellationToken::new(),
        }
    }

    /// Call an `rp` tool, apply the result convention, deserialize.
    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        tool: &str,
        arguments: Value,
    ) -> Result<T> {
        debug!(tool = %tool, "calling rp tool");
        let mut params = CallToolRequestParams::new(tool.to_owned());
        let args = arguments.as_object().cloned().unwrap_or_default();
        if !args.is_empty() {
            params.arguments = Some(args);
        }
        let cancel = self.cancel.clone();
        let cancelled = async move {
            cancel.cancelled().await;
            CANCEL_REASON.to_owned()
        };
        let result = match self
            .inner
            .call_tool_forwarding(params, None, cancelled)
            .await
        {
            Ok(result) => result,
            Err(ProxyCallError::Cancelled) => {
                return Err(FocusModelError::Cancelled(CANCEL_REASON.to_owned()));
            }
            Err(e) => return Err(FocusModelError::ToolCall(format!("{tool}: {e}"))),
        };
        let value = rp_mcp_client::tool_result_value(&result)
            .map_err(|e| FocusModelError::ToolCall(format!("{tool}: {e}")))?;
        serde_json::from_value(value)
            .map_err(|e| FocusModelError::ToolCall(format!("{tool}: failed to parse result: {e}")))
    }
}

#[async_trait]
impl FocusRig for McpClient {
    async fn get_train_info(&self, train_id: &str) -> Result<TrainInfo> {
        self.call(
            "get_train_info",
            serde_json::json!({ "train_id": train_id }),
        )
        .await
    }

    async fn get_refocus_plan(&self, train_id: &str) -> Result<RefocusPlan> {
        self.call(
            "get_refocus_plan",
            serde_json::json!({ "train_id": train_id }),
        )
        .await
    }

    async fn get_focuser_position(&self, focuser_id: &str) -> Result<FocuserPosition> {
        self.call(
            "get_focuser_position",
            serde_json::json!({ "focuser_id": focuser_id }),
        )
        .await
    }

    async fn get_focuser_temperature(&self, focuser_id: &str) -> Result<Option<f64>> {
        let result: TemperatureResult = self
            .call(
                "get_focuser_temperature",
                serde_json::json!({ "focuser_id": focuser_id }),
            )
            .await?;
        Ok(result.temperature_c)
    }

    async fn move_focuser(&self, focuser_id: &str, position: i32) -> Result<i32> {
        #[derive(Deserialize)]
        struct MoveResult {
            actual_position: i32,
        }
        let result: MoveResult = self
            .call(
                "move_focuser",
                serde_json::json!({ "focuser_id": focuser_id, "position": position }),
            )
            .await?;
        Ok(result.actual_position)
    }

    async fn get_filter(&self, filter_wheel_id: &str) -> Result<Option<String>> {
        let result: FilterResult = self
            .call(
                "get_filter",
                serde_json::json!({ "filter_wheel_id": filter_wheel_id }),
            )
            .await?;
        Ok(result.filter_name)
    }

    async fn set_filter(&self, filter_wheel_id: &str, filter: &str) -> Result<()> {
        let _: Value = self
            .call(
                "set_filter",
                serde_json::json!({ "filter_wheel_id": filter_wheel_id, "filter_name": filter }),
            )
            .await?;
        Ok(())
    }

    async fn capture(&self, train_id: &str, duration: Duration) -> Result<CaptureResult> {
        self.call(
            "capture",
            serde_json::json!({
                "train_id": train_id,
                "duration": humantime::format_duration(duration).to_string(),
            }),
        )
        .await
    }

    async fn measure_stars(
        &self,
        document_id: &str,
        min_area: usize,
        max_area: usize,
        threshold_sigma: Option<f64>,
    ) -> Result<StarMeasurement> {
        let mut args = serde_json::json!({
            "document_id": document_id,
            "min_area": min_area,
            "max_area": max_area,
        });
        if let (Some(sigma), Some(map)) = (threshold_sigma, args.as_object_mut()) {
            map.insert("threshold_sigma".to_owned(), serde_json::json!(sigma));
        }
        self.call("measure_stars", args).await
    }

    async fn guiding_active(&self) -> Result<bool> {
        let stats: GuidingStats = self.call("get_guiding_stats", Value::Null).await?;
        Ok(stats.guiding)
    }

    async fn pause_guiding(&self) -> Result<()> {
        let _: Value = self
            .call("pause_guiding", serde_json::json!({ "full": false }))
            .await?;
        Ok(())
    }

    async fn resume_guiding(&self) -> Result<()> {
        let _: Value = self.call("resume_guiding", Value::Null).await?;
        Ok(())
    }

    async fn auto_focus_guide_train(&self, train_id: &str) -> Result<GuideFocusResult> {
        self.call("auto_focus", serde_json::json!({ "train_id": train_id }))
            .await
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}
