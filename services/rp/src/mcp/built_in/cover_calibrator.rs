use std::time::Duration;

use ascom_alpaca::api::cover_calibrator::{CalibratorStatus, CoverStatus};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::super::handler::McpHandler;
use super::super::{resolve_device, tool_error, tool_success};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalibratorIdParams {
    pub calibrator_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalibratorOnParams {
    pub calibrator_id: String,
    /// Brightness `0..max_brightness`. When omitted, the device's
    /// reported `max_brightness` is used.
    #[serde(default)]
    pub brightness: Option<u32>,
}

#[tool_router(router = tool_router_cover_calibrator, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Read the cover state (NotPresent | Closed | Moving | Open | Unknown | Error) without actuating anything"
    )]
    pub(crate) async fn get_cover_state(
        &self,
        Parameters(params): Parameters<CalibratorIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (_cc_entry, cc) = resolve_device!(
            self,
            find_cover_calibrator,
            &params.calibrator_id,
            "calibrator"
        );

        match cc.cover_state().await {
            Ok(state) => {
                debug!(calibrator_id = %params.calibrator_id, cover_state = ?state, "read cover state");
                Ok(tool_success!({"cover_state": format!("{state:?}")}))
            }
            Err(e) => Ok(tool_error!("failed to read cover state: {}", e)),
        }
    }

    #[tool(description = "Close the dust cover (blocks until closed)")]
    pub(crate) async fn close_cover(
        &self,
        Parameters(params): Parameters<CalibratorIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (cc_entry, cc) = resolve_device!(
            self,
            find_cover_calibrator,
            &params.calibrator_id,
            "calibrator"
        );
        let poll_interval = cc_entry.config.poll_interval;

        debug!(calibrator_id = %params.calibrator_id, "closing cover");
        if let Err(e) = cc.close_cover().await {
            return Ok(tool_error!("failed to close cover: {}", e));
        }

        let now = tokio::time::Instant::now();
        // Unreachable overflow degrades to an expired deadline, not a panic.
        let deadline = now.checked_add(Duration::from_mins(1)).unwrap_or(now);
        loop {
            tokio::time::sleep(poll_interval).await;
            match cc.cover_state().await {
                Ok(CoverStatus::Closed) => {
                    debug!(calibrator_id = %params.calibrator_id, "cover closed");
                    return Ok(tool_success!({"status": "closed"}));
                }
                Ok(_) if tokio::time::Instant::now() < deadline => continue,
                Ok(_) => break,
                Err(e) => {
                    return Ok(tool_error!("error polling cover state: {}", e));
                }
            }
        }

        Ok(tool_error!("timeout waiting for cover to close"))
    }

    #[tool(description = "Open the dust cover (blocks until open)")]
    pub(crate) async fn open_cover(
        &self,
        Parameters(params): Parameters<CalibratorIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (cc_entry, cc) = resolve_device!(
            self,
            find_cover_calibrator,
            &params.calibrator_id,
            "calibrator"
        );
        let poll_interval = cc_entry.config.poll_interval;

        debug!(calibrator_id = %params.calibrator_id, "opening cover");
        if let Err(e) = cc.open_cover().await {
            return Ok(tool_error!("failed to open cover: {}", e));
        }

        let now = tokio::time::Instant::now();
        // Unreachable overflow degrades to an expired deadline, not a panic.
        let deadline = now.checked_add(Duration::from_mins(1)).unwrap_or(now);
        loop {
            tokio::time::sleep(poll_interval).await;
            match cc.cover_state().await {
                Ok(CoverStatus::Open) => {
                    debug!(calibrator_id = %params.calibrator_id, "cover opened");
                    return Ok(tool_success!({"status": "open"}));
                }
                Ok(_) if tokio::time::Instant::now() < deadline => continue,
                Ok(_) => break,
                Err(e) => {
                    return Ok(tool_error!("error polling cover state: {}", e));
                }
            }
        }

        Ok(tool_error!("timeout waiting for cover to open"))
    }

    #[tool(description = "Turn on flat panel at brightness (default: max). Blocks until ready")]
    pub(crate) async fn calibrator_on(
        &self,
        Parameters(params): Parameters<CalibratorOnParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (cc_entry, cc) = resolve_device!(
            self,
            find_cover_calibrator,
            &params.calibrator_id,
            "calibrator"
        );
        let poll_interval = cc_entry.config.poll_interval;

        let brightness = if let Some(b) = params.brightness {
            b
        } else {
            match cc.max_brightness().await {
                Ok(max) => max,
                Err(e) => return Ok(tool_error!("failed to read max_brightness: {}", e)),
            }
        };

        debug!(calibrator_id = %params.calibrator_id, brightness = brightness, "turning calibrator on");
        if let Err(e) = cc.calibrator_on(brightness).await {
            return Ok(tool_error!("failed to turn calibrator on: {}", e));
        }

        let now = tokio::time::Instant::now();
        // Unreachable overflow degrades to an expired deadline, not a panic.
        let deadline = now.checked_add(Duration::from_mins(1)).unwrap_or(now);
        loop {
            tokio::time::sleep(poll_interval).await;
            match cc.calibrator_state().await {
                Ok(CalibratorStatus::Ready) => {
                    debug!(calibrator_id = %params.calibrator_id, "calibrator ready");
                    return Ok(tool_success!({"status": "ready", "brightness": brightness}));
                }
                Ok(_) if tokio::time::Instant::now() < deadline => continue,
                Ok(_) => break,
                Err(e) => {
                    return Ok(tool_error!("error polling calibrator state: {}", e));
                }
            }
        }

        Ok(tool_error!(
            "timeout waiting for calibrator to become ready"
        ))
    }

    #[tool(description = "Turn off flat panel. Blocks until off")]
    pub(crate) async fn calibrator_off(
        &self,
        Parameters(params): Parameters<CalibratorIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (cc_entry, cc) = resolve_device!(
            self,
            find_cover_calibrator,
            &params.calibrator_id,
            "calibrator"
        );
        let poll_interval = cc_entry.config.poll_interval;

        debug!(calibrator_id = %params.calibrator_id, "turning calibrator off");
        if let Err(e) = cc.calibrator_off().await {
            return Ok(tool_error!("failed to turn calibrator off: {}", e));
        }

        let now = tokio::time::Instant::now();
        // Unreachable overflow degrades to an expired deadline, not a panic.
        let deadline = now.checked_add(Duration::from_mins(1)).unwrap_or(now);
        loop {
            tokio::time::sleep(poll_interval).await;
            match cc.calibrator_state().await {
                Ok(CalibratorStatus::Off) => {
                    debug!(calibrator_id = %params.calibrator_id, "calibrator off");
                    return Ok(tool_success!({"status": "off"}));
                }
                Ok(_) if tokio::time::Instant::now() < deadline => continue,
                Ok(_) => break,
                Err(e) => {
                    return Ok(tool_error!("error polling calibrator state: {}", e));
                }
            }
        }

        Ok(tool_error!("timeout waiting for calibrator to turn off"))
    }
}
