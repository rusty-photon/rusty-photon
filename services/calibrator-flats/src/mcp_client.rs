//! The provider's client half: `rp`'s built-in tools, cancellable.
//!
//! Built on the standard `rp-mcp-client` crate (ADR-017 — CA-pinned
//! TLS and the observatory credential over verified HTTPS only), with
//! every call cancellable by the tool request's token.
//!
//! A cancelled call does not merely stop waiting: `notifications/cancelled`
//! goes to `rp` for the in-flight request (`call_tool_forwarding`), so an
//! exposure in progress is aborted rather than finished into the void.
//! Cleanup after a cancellation runs through [`McpClient::uncancellable`],
//! the same connection under a token nothing fires.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rp_mcp_client::{ProxyCallError, RpMcpClient, SAFETY_UNSAFE_CODE};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::config::Config;
use crate::error::{CalibratorFlatsError, Result};
use crate::workflow::{CameraInfo, CaptureResult, FlatsRig, Frame, ImageStats, TrainInfo};

/// The reason sent to `rp` with `notifications/cancelled`, and carried
/// by [`CalibratorFlatsError::Cancelled`].
pub const CANCEL_REASON: &str = "the caller cancelled the flats run";

/// A connection to `rp` bound to one cancellation token.
pub struct McpClient {
    inner: Arc<RpMcpClient>,
    cancel: CancellationToken,
}

/// Result from the `get_cover_state` tool.
#[derive(Debug, Clone, Deserialize)]
struct CoverStateResult {
    cover_state: String,
}

/// Result from the `calibrator_on` tool.
#[derive(Debug, Clone, Deserialize)]
struct CalibratorOnResult {
    brightness: u32,
}

impl McpClient {
    /// Connect to `rp` at the configured `mcp_server_url`, presenting
    /// `service_auth` per the ADR-017 credential policy. Calls made
    /// through the returned client are cancelled when `cancel` fires.
    ///
    /// # Errors
    ///
    /// Returns [`CalibratorFlatsError::ToolCall`] if the connection
    /// fails — the HTTP client cannot be built (bad CA path or PEM), the
    /// Authorization header cannot be constructed, or the MCP
    /// `server/discover` bootstrap fails.
    pub async fn connect(config: &Config, cancel: CancellationToken) -> Result<Self> {
        debug!(url = %config.mcp_server_url, "connecting to rp");
        let inner = RpMcpClient::connect(&config.mcp_server_url, config.rp_auth(), config.rp_ca())
            .await
            .map_err(|e| {
                CalibratorFlatsError::ToolCall(format!(
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
    /// cleanup a cancellation must not reach.
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
                return Err(CalibratorFlatsError::Cancelled(CANCEL_REASON.to_owned()));
            }
            Err(ProxyCallError::Protocol(data)) if data.code.0 == SAFETY_UNSAFE_CODE => {
                return Err(CalibratorFlatsError::SafetyRefused(format!(
                    "{tool}: {}",
                    data.message
                )));
            }
            Err(e) => return Err(CalibratorFlatsError::ToolCall(format!("{tool}: {e}"))),
        };
        let value = rp_mcp_client::tool_result_value(&result)
            .map_err(|e| CalibratorFlatsError::ToolCall(format!("{tool}: {e}")))?;
        serde_json::from_value(value).map_err(|e| {
            CalibratorFlatsError::ToolCall(format!("{tool}: failed to parse result: {e}"))
        })
    }
}

#[async_trait]
impl FlatsRig for McpClient {
    async fn get_train_info(&self, train_id: &str) -> Result<TrainInfo> {
        self.call(
            "get_train_info",
            serde_json::json!({ "train_id": train_id }),
        )
        .await
    }

    async fn get_camera_info(&self, camera_id: &str) -> Result<CameraInfo> {
        self.call(
            "get_camera_info",
            serde_json::json!({ "camera_id": camera_id }),
        )
        .await
    }

    async fn get_cover_state(&self, calibrator_id: &str) -> Result<String> {
        let result: CoverStateResult = self
            .call(
                "get_cover_state",
                serde_json::json!({ "calibrator_id": calibrator_id }),
            )
            .await?;
        Ok(result.cover_state)
    }

    async fn close_cover(&self, calibrator_id: &str) -> Result<()> {
        let _: Value = self
            .call(
                "close_cover",
                serde_json::json!({ "calibrator_id": calibrator_id }),
            )
            .await?;
        Ok(())
    }

    async fn open_cover(&self, calibrator_id: &str) -> Result<()> {
        let _: Value = self
            .call(
                "open_cover",
                serde_json::json!({ "calibrator_id": calibrator_id }),
            )
            .await?;
        Ok(())
    }

    async fn calibrator_on(&self, calibrator_id: &str, brightness: Option<u32>) -> Result<u32> {
        let mut args = serde_json::json!({ "calibrator_id": calibrator_id });
        if let (Some(b), Some(map)) = (brightness, args.as_object_mut()) {
            map.insert("brightness".to_owned(), serde_json::json!(b));
        }
        let result: CalibratorOnResult = self.call("calibrator_on", args).await?;
        Ok(result.brightness)
    }

    async fn calibrator_off(&self, calibrator_id: &str) -> Result<()> {
        let _: Value = self
            .call(
                "calibrator_off",
                serde_json::json!({ "calibrator_id": calibrator_id }),
            )
            .await?;
        Ok(())
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

    async fn capture(
        &self,
        camera_id: &str,
        duration: Duration,
        frame: Frame,
    ) -> Result<CaptureResult> {
        let mut args = serde_json::json!({
            "camera_id": camera_id,
            "duration": humantime::format_duration(duration).to_string(),
        });
        if let (Frame::Flat, Some(map)) = (frame, args.as_object_mut()) {
            // rp files the frame under the flats directory and stamps
            // the document (rp.md § Capture Tool Details); a probe
            // exposure of the search stays a plain, unfiled capture.
            map.insert("frame_type".to_owned(), serde_json::json!("Flat"));
        }
        self.call("capture", args).await
    }

    async fn compute_image_stats(&self, image_path: &str, document_id: &str) -> Result<ImageStats> {
        self.call(
            "compute_image_stats",
            serde_json::json!({ "image_path": image_path, "document_id": document_id }),
        )
        .await
    }
}
