//! MCP client for calling rp's built-in tools, built on the standard
//! `rp-mcp-client` crate (ADR-017): CA-pinned TLS and the observatory
//! credential over verified HTTPS only.

use std::path::Path;
use std::time::Duration;

use rp_mcp_client::{ClientAuthConfig, RpMcpClient};
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use crate::error::{CalibratorFlatsError, Result};

/// MCP client for one `rp` session.
pub struct McpClient {
    inner: RpMcpClient,
}

/// Result from the `capture` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct CaptureResult {
    pub image_path: String,
    pub document_id: String,
}

/// Result from the `get_camera_info` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct CameraInfo {
    pub max_adu: u32,
    #[serde(with = "humantime_serde")]
    pub exposure_min: Duration,
    #[serde(with = "humantime_serde")]
    pub exposure_max: Duration,
}

/// Result from the `compute_image_stats` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct ImageStats {
    pub median_adu: u32,
    pub mean_adu: f64,
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
    /// Connect to an MCP server at the given URL, presenting
    /// `service_auth` per the ADR-017 credential policy.
    ///
    /// # Errors
    ///
    /// Returns [`CalibratorFlatsError::ToolCall`] if the connection
    /// fails — the HTTP client cannot be built (bad CA path or PEM),
    /// the Authorization header cannot be constructed, or the MCP
    /// initialize handshake fails.
    pub async fn new(
        mcp_url: &str,
        service_auth: Option<&ClientAuthConfig>,
        ca_cert: Option<&Path>,
    ) -> Result<Self> {
        debug!(url = %mcp_url, "connecting MCP client");
        let inner = RpMcpClient::connect(mcp_url, service_auth, ca_cert)
            .await
            .map_err(|e| CalibratorFlatsError::ToolCall(format!("MCP connect: {e}")))?;
        Ok(Self { inner })
    }

    /// Capture one frame of `duration` on `camera_id` via rp's
    /// `capture` tool, returning the stored image path and document id.
    ///
    /// # Errors
    ///
    /// Returns [`CalibratorFlatsError::ToolCall`] if the `capture` call
    /// fails — the request cannot be sent, rp reports a tool error, or
    /// the reply is not a [`CaptureResult`].
    pub async fn capture(&self, camera_id: &str, duration: Duration) -> Result<CaptureResult> {
        self.call_tool(
            "capture",
            serde_json::json!({
                "camera_id": camera_id,
                "duration": humantime::format_duration(duration).to_string(),
            }),
        )
        .await
    }

    /// Query `camera_id`'s ADU ceiling and exposure range via rp's
    /// `get_camera_info` tool.
    ///
    /// # Errors
    ///
    /// Returns [`CalibratorFlatsError::ToolCall`] if the
    /// `get_camera_info` call fails — the request cannot be sent, rp
    /// reports a tool error, or the reply is not a [`CameraInfo`].
    pub async fn get_camera_info(&self, camera_id: &str) -> Result<CameraInfo> {
        self.call_tool(
            "get_camera_info",
            serde_json::json!({"camera_id": camera_id}),
        )
        .await
    }

    /// Compute the median and mean ADU of a captured image via rp's
    /// `compute_image_stats` tool.
    ///
    /// # Errors
    ///
    /// Returns [`CalibratorFlatsError::ToolCall`] if the
    /// `compute_image_stats` call fails — the request cannot be sent,
    /// rp reports a tool error, or the reply is not an [`ImageStats`].
    pub async fn compute_image_stats(
        &self,
        image_path: &str,
        document_id: Option<&str>,
    ) -> Result<ImageStats> {
        let mut args = serde_json::json!({"image_path": image_path});
        if let Some(doc_id) = document_id {
            let map = args.as_object_mut();
            debug_assert!(map.is_some(), "args is built as an object literal above");
            if let Some(map) = map {
                map.insert("document_id".to_owned(), serde_json::json!(doc_id));
            }
        }
        self.call_tool("compute_image_stats", args).await
    }

    /// Move the filter wheel to `filter_name` via rp's `set_filter`
    /// tool.
    ///
    /// # Errors
    ///
    /// Returns [`CalibratorFlatsError::ToolCall`] if the `set_filter`
    /// call fails — the request cannot be sent, rp reports a tool
    /// error, or the reply is not the expected JSON shape.
    pub async fn set_filter(&self, filter_wheel_id: &str, filter_name: &str) -> Result<()> {
        let _: Value = self
            .call_tool(
                "set_filter",
                serde_json::json!({"filter_wheel_id": filter_wheel_id, "filter_name": filter_name}),
            )
            .await?;
        Ok(())
    }

    /// Read the cover's state without actuating anything. Returns the
    /// state name as rp reports it (`NotPresent` | `Closed` | `Moving` |
    /// `Open` | `Unknown` | `Error`).
    ///
    /// # Errors
    ///
    /// Returns [`CalibratorFlatsError::ToolCall`] if the
    /// `get_cover_state` call fails — the request cannot be sent, rp
    /// reports a tool error, or the reply is not the expected JSON
    /// shape.
    pub async fn get_cover_state(&self, calibrator_id: &str) -> Result<String> {
        let result: CoverStateResult = self
            .call_tool(
                "get_cover_state",
                serde_json::json!({"calibrator_id": calibrator_id}),
            )
            .await?;
        Ok(result.cover_state)
    }

    /// Close the calibrator's cover via rp's `close_cover` tool.
    ///
    /// # Errors
    ///
    /// Returns [`CalibratorFlatsError::ToolCall`] if the `close_cover`
    /// call fails — the request cannot be sent, rp reports a tool
    /// error, or the reply is not the expected JSON shape.
    pub async fn close_cover(&self, calibrator_id: &str) -> Result<()> {
        let _: Value = self
            .call_tool(
                "close_cover",
                serde_json::json!({"calibrator_id": calibrator_id}),
            )
            .await?;
        Ok(())
    }

    /// Open the calibrator's cover via rp's `open_cover` tool.
    ///
    /// # Errors
    ///
    /// Returns [`CalibratorFlatsError::ToolCall`] if the `open_cover`
    /// call fails — the request cannot be sent, rp reports a tool
    /// error, or the reply is not the expected JSON shape.
    pub async fn open_cover(&self, calibrator_id: &str) -> Result<()> {
        let _: Value = self
            .call_tool(
                "open_cover",
                serde_json::json!({"calibrator_id": calibrator_id}),
            )
            .await?;
        Ok(())
    }

    /// Turn the panel on and return the brightness rp actually applied
    /// (the device maximum when `brightness` is `None`) — the brightness
    /// ladder's starting point.
    ///
    /// # Errors
    ///
    /// Returns [`CalibratorFlatsError::ToolCall`] if the
    /// `calibrator_on` call fails — the request cannot be sent, rp
    /// reports a tool error, or the reply is not the expected JSON
    /// shape.
    pub async fn calibrator_on(&self, calibrator_id: &str, brightness: Option<u32>) -> Result<u32> {
        let mut args = serde_json::json!({"calibrator_id": calibrator_id});
        if let Some(b) = brightness {
            let map = args.as_object_mut();
            debug_assert!(map.is_some(), "args is built as an object literal above");
            if let Some(map) = map {
                map.insert("brightness".to_owned(), serde_json::json!(b));
            }
        }
        let result: CalibratorOnResult = self.call_tool("calibrator_on", args).await?;
        Ok(result.brightness)
    }

    /// Turn the calibrator panel off via rp's `calibrator_off` tool.
    ///
    /// # Errors
    ///
    /// Returns [`CalibratorFlatsError::ToolCall`] if the
    /// `calibrator_off` call fails — the request cannot be sent, rp
    /// reports a tool error, or the reply is not the expected JSON
    /// shape.
    pub async fn calibrator_off(&self, calibrator_id: &str) -> Result<()> {
        let _: Value = self
            .call_tool(
                "calibrator_off",
                serde_json::json!({"calibrator_id": calibrator_id}),
            )
            .await?;
        Ok(())
    }

    /// Generic helper: call tool, check for errors, deserialize result.
    async fn call_tool<T: serde::de::DeserializeOwned>(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<T> {
        debug!(tool = %tool_name, "calling MCP tool");

        let args = arguments.as_object().cloned().unwrap_or_default();
        let value = self
            .inner
            .call_tool(tool_name, args)
            .await
            .map_err(|e| CalibratorFlatsError::ToolCall(format!("{tool_name}: {e}")))?;

        serde_json::from_value(value).map_err(|e| {
            CalibratorFlatsError::ToolCall(format!("{tool_name}: failed to parse result: {e}"))
        })
    }
}
