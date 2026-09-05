//! `CoverCalibrator` tool category: `get_cover_state`, `close_cover`,
//! `open_cover`, `calibrator_on`, `calibrator_off` (rp.md
//! § `CoverCalibrator` Tool Details).
//!
//! Every tool addresses the device as `calibrator_id` *or* `train_id`
//! — exactly one — where `train_id` resolves through the optical-train
//! model to the calibrator first in that train's list. Every result
//! carries the resolved `calibrator_id` and `trains`, the optical
//! trains containing it: a closed cover blinds every camera behind it
//! and a lit panel floods them, so the sibling trains are worth
//! knowing (the `moved_trains` precedent of `move_rotator`).

use std::time::Duration;

use ascom_alpaca::api::cover_calibrator::{CalibratorStatus, CoverStatus};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_router, RoleServer};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::super::handler::McpHandler;
use super::super::inflight::Cancel;
use super::super::{resolve_device, tool_error, tool_success};

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(extend("oneOf" = [{"required": ["calibrator_id"]}, {"required": ["train_id"]}]))]
pub struct CalibratorIdParams {
    /// Cover calibrator device ID; mutually exclusive with `train_id`.
    #[serde(default)]
    pub calibrator_id: Option<String>,
    /// Optical train whose cover calibrator (first in its device list)
    /// is addressed; mutually exclusive with `calibrator_id`.
    #[serde(default)]
    pub train_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(extend("oneOf" = [{"required": ["calibrator_id"]}, {"required": ["train_id"]}]))]
pub struct CalibratorOnParams {
    /// Cover calibrator device ID; mutually exclusive with `train_id`.
    #[serde(default)]
    pub calibrator_id: Option<String>,
    /// Optical train whose cover calibrator (first in its device list)
    /// is addressed; mutually exclusive with `calibrator_id`.
    #[serde(default)]
    pub train_id: Option<String>,
    /// Brightness `0..max_brightness`. When omitted, the device's
    /// reported `max_brightness` is used.
    #[serde(default)]
    pub brightness: Option<u32>,
}

/// Bound on every blocking wait in this category: a cover motor or a
/// panel lamp that has not reached its state after a minute is stuck.
const COVER_CALIBRATOR_DEADLINE: Duration = Duration::from_mins(1);

#[tool_router(router = tool_router_cover_calibrator, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Read the cover state (NotPresent | Closed | Moving | Open | Unknown | Error) without actuating anything. Address as calibrator_id or train_id (the train's cover calibrator). Returns the resolved calibrator_id plus trains, the optical trains containing it"
    )]
    pub(crate) async fn get_cover_state(
        &self,
        Parameters(params): Parameters<CalibratorIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let calibrator_id = match self.resolve_calibrator_addressing(
            "get_cover_state",
            params.calibrator_id.as_deref(),
            params.train_id.as_deref(),
        ) {
            Ok(id) => id,
            Err(e) => return Ok(*e),
        };
        let (_cc_entry, cc) =
            resolve_device!(self, find_cover_calibrator, &calibrator_id, "calibrator");

        match cc.cover_state().await {
            Ok(state) => {
                debug!(calibrator_id = %calibrator_id, cover_state = ?state, "read cover state");
                Ok(tool_success!({
                    "calibrator_id": calibrator_id,
                    "trains": self.trains_with_calibrator(&calibrator_id),
                    "cover_state": format!("{state:?}"),
                }))
            }
            Err(e) => Ok(tool_error!("failed to read cover state: {}", e)),
        }
    }

    #[tool(
        description = "Close the dust cover (blocks until closed). Address as calibrator_id or train_id (the train's cover calibrator). Returns the resolved calibrator_id plus trains, the optical trains containing it"
    )]
    pub(crate) async fn close_cover(
        &self,
        Parameters(params): Parameters<CalibratorIdParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cancel = Cancel::from_context(&ctx);
        self.close_cover_inner(params, &cancel).await
    }

    /// Body of the `close_cover` MCP tool, split out so unit tests can pass
    /// a never-cancelled handle without constructing a real rmcp
    /// `RequestContext`.
    pub(crate) async fn close_cover_inner(
        &self,
        params: CalibratorIdParams,
        cancel: &Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let calibrator_id = match self.resolve_calibrator_addressing(
            "close_cover",
            params.calibrator_id.as_deref(),
            params.train_id.as_deref(),
        ) {
            Ok(id) => id,
            Err(e) => return Ok(*e),
        };
        let (cc_entry, cc) =
            resolve_device!(self, find_cover_calibrator, &calibrator_id, "calibrator");
        let poll_interval = cc_entry.config.poll_interval;

        debug!(calibrator_id = %calibrator_id, "closing cover");
        if let Err(e) = cc.close_cover().await {
            return Ok(tool_error!("failed to close cover: {}", e));
        }

        let now = tokio::time::Instant::now();
        // Unreachable overflow degrades to an expired deadline, not a panic.
        let deadline = now.checked_add(COVER_CALIBRATOR_DEADLINE).unwrap_or(now);
        loop {
            // A cancelled close stops *waiting*, never the cover: a
            // closing cover is what the transition wants (rp.md
            // § In-Flight Tool Calls).
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(tool_error!("{}", cancel.error())),
                () = tokio::time::sleep(poll_interval) => {}
            }
            match cc.cover_state().await {
                Ok(CoverStatus::Closed) => {
                    debug!(calibrator_id = %calibrator_id, "cover closed");
                    return Ok(tool_success!({
                        "calibrator_id": calibrator_id,
                        "trains": self.trains_with_calibrator(&calibrator_id),
                        "status": "closed",
                    }));
                }
                Ok(_) if tokio::time::Instant::now() < deadline => {}
                Ok(_) => break,
                Err(e) => {
                    return Ok(tool_error!("error polling cover state: {}", e));
                }
            }
        }

        Ok(tool_error!("timeout waiting for cover to close"))
    }

    #[tool(
        description = "Open the dust cover (blocks until open). Address as calibrator_id or train_id (the train's cover calibrator). Returns the resolved calibrator_id plus trains, the optical trains containing it"
    )]
    pub(crate) async fn open_cover(
        &self,
        Parameters(params): Parameters<CalibratorIdParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cancel = Cancel::from_context(&ctx);
        self.open_cover_inner(params, &cancel).await
    }

    /// Body of the `open_cover` MCP tool, split out so unit tests can pass
    /// a never-cancelled handle without constructing a real rmcp
    /// `RequestContext`.
    pub(crate) async fn open_cover_inner(
        &self,
        params: CalibratorIdParams,
        cancel: &Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let calibrator_id = match self.resolve_calibrator_addressing(
            "open_cover",
            params.calibrator_id.as_deref(),
            params.train_id.as_deref(),
        ) {
            Ok(id) => id,
            Err(e) => return Ok(*e),
        };
        let (cc_entry, cc) =
            resolve_device!(self, find_cover_calibrator, &calibrator_id, "calibrator");
        let poll_interval = cc_entry.config.poll_interval;

        debug!(calibrator_id = %calibrator_id, "opening cover");
        if let Err(e) = cc.open_cover().await {
            return Ok(tool_error!("failed to open cover: {}", e));
        }

        let now = tokio::time::Instant::now();
        // Unreachable overflow degrades to an expired deadline, not a panic.
        let deadline = now.checked_add(COVER_CALIBRATOR_DEADLINE).unwrap_or(now);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    // Stop-class counterpart (rp.md § In-Flight Tool
                    // Calls): halt the cover where it is.
                    if let Err(e) = cc.halt_cover().await {
                        debug!(error = %e, "halt_cover after cancellation failed");
                    }
                    return Ok(tool_error!("{}", cancel.error()));
                }
                () = tokio::time::sleep(poll_interval) => {}
            }
            match cc.cover_state().await {
                Ok(CoverStatus::Open) => {
                    debug!(calibrator_id = %calibrator_id, "cover opened");
                    return Ok(tool_success!({
                        "calibrator_id": calibrator_id,
                        "trains": self.trains_with_calibrator(&calibrator_id),
                        "status": "open",
                    }));
                }
                Ok(_) if tokio::time::Instant::now() < deadline => {}
                Ok(_) => break,
                Err(e) => {
                    return Ok(tool_error!("error polling cover state: {}", e));
                }
            }
        }

        Ok(tool_error!("timeout waiting for cover to open"))
    }

    #[tool(
        description = "Turn on flat panel at brightness (default: max). Blocks until ready. Address as calibrator_id or train_id (the train's cover calibrator). Returns the resolved calibrator_id plus trains, the optical trains containing it"
    )]
    pub(crate) async fn calibrator_on(
        &self,
        Parameters(params): Parameters<CalibratorOnParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cancel = Cancel::from_context(&ctx);
        self.calibrator_on_inner(params, &cancel).await
    }

    /// Body of the `calibrator_on` MCP tool, split out so unit tests can pass
    /// a never-cancelled handle without constructing a real rmcp
    /// `RequestContext`.
    pub(crate) async fn calibrator_on_inner(
        &self,
        params: CalibratorOnParams,
        cancel: &Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let calibrator_id = match self.resolve_calibrator_addressing(
            "calibrator_on",
            params.calibrator_id.as_deref(),
            params.train_id.as_deref(),
        ) {
            Ok(id) => id,
            Err(e) => return Ok(*e),
        };
        let (cc_entry, cc) =
            resolve_device!(self, find_cover_calibrator, &calibrator_id, "calibrator");
        let poll_interval = cc_entry.config.poll_interval;

        let brightness = if let Some(b) = params.brightness {
            b
        } else {
            match cc.max_brightness().await {
                Ok(max) => max,
                Err(e) => return Ok(tool_error!("failed to read max_brightness: {}", e)),
            }
        };

        debug!(calibrator_id = %calibrator_id, brightness = brightness, "turning calibrator on");
        if let Err(e) = cc.calibrator_on(brightness).await {
            return Ok(tool_error!("failed to turn calibrator on: {}", e));
        }

        let now = tokio::time::Instant::now();
        // Unreachable overflow degrades to an expired deadline, not a panic.
        let deadline = now.checked_add(COVER_CALIBRATOR_DEADLINE).unwrap_or(now);
        loop {
            // No stop-class counterpart for a panel lamp: the wait
            // ends, the lamp finishes on its own.
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(tool_error!("{}", cancel.error())),
                () = tokio::time::sleep(poll_interval) => {}
            }
            match cc.calibrator_state().await {
                Ok(CalibratorStatus::Ready) => {
                    debug!(calibrator_id = %calibrator_id, "calibrator ready");
                    return Ok(tool_success!({
                        "calibrator_id": calibrator_id,
                        "trains": self.trains_with_calibrator(&calibrator_id),
                        "status": "ready",
                        "brightness": brightness,
                    }));
                }
                Ok(_) if tokio::time::Instant::now() < deadline => {}
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

    #[tool(
        description = "Turn off flat panel. Blocks until off. Address as calibrator_id or train_id (the train's cover calibrator). Returns the resolved calibrator_id plus trains, the optical trains containing it"
    )]
    pub(crate) async fn calibrator_off(
        &self,
        Parameters(params): Parameters<CalibratorIdParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cancel = Cancel::from_context(&ctx);
        self.calibrator_off_inner(params, &cancel).await
    }

    /// Body of the `calibrator_off` MCP tool, split out so unit tests can pass
    /// a never-cancelled handle without constructing a real rmcp
    /// `RequestContext`.
    pub(crate) async fn calibrator_off_inner(
        &self,
        params: CalibratorIdParams,
        cancel: &Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let calibrator_id = match self.resolve_calibrator_addressing(
            "calibrator_off",
            params.calibrator_id.as_deref(),
            params.train_id.as_deref(),
        ) {
            Ok(id) => id,
            Err(e) => return Ok(*e),
        };
        let (cc_entry, cc) =
            resolve_device!(self, find_cover_calibrator, &calibrator_id, "calibrator");
        let poll_interval = cc_entry.config.poll_interval;

        debug!(calibrator_id = %calibrator_id, "turning calibrator off");
        if let Err(e) = cc.calibrator_off().await {
            return Ok(tool_error!("failed to turn calibrator off: {}", e));
        }

        let now = tokio::time::Instant::now();
        // Unreachable overflow degrades to an expired deadline, not a panic.
        let deadline = now.checked_add(COVER_CALIBRATOR_DEADLINE).unwrap_or(now);
        loop {
            // No stop-class counterpart for a panel lamp: the wait
            // ends, the lamp finishes on its own.
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(tool_error!("{}", cancel.error())),
                () = tokio::time::sleep(poll_interval) => {}
            }
            match cc.calibrator_state().await {
                Ok(CalibratorStatus::Off) => {
                    debug!(calibrator_id = %calibrator_id, "calibrator off");
                    return Ok(tool_success!({
                        "calibrator_id": calibrator_id,
                        "trains": self.trains_with_calibrator(&calibrator_id),
                        "status": "off",
                    }));
                }
                Ok(_) if tokio::time::Instant::now() < deadline => {}
                Ok(_) => break,
                Err(e) => {
                    return Ok(tool_error!("error polling calibrator state: {}", e));
                }
            }
        }

        Ok(tool_error!("timeout waiting for calibrator to turn off"))
    }
}

impl McpHandler {
    /// Resolve the `calibrator_id` / `train_id` addressing shared by
    /// the five calibrator tools: exactly one must be present, and
    /// `train_id` resolves the calibrator first in the train's list. A
    /// train without one is an error naming the train. Returns the
    /// resolved roster id, or the ready-to-return error
    /// `CallToolResult` (boxed — `clippy::result_large_err`).
    pub(crate) fn resolve_calibrator_addressing(
        &self,
        tool: &str,
        calibrator_id: Option<&str>,
        train_id: Option<&str>,
    ) -> Result<String, Box<CallToolResult>> {
        match (calibrator_id, train_id) {
            (Some(_), Some(_)) => Err(Box::new(tool_error!(
                "{}: train_id is mutually exclusive with calibrator_id",
                tool
            ))),
            (None, None) => Err(Box::new(tool_error!(
                "{}: pass exactly one of calibrator_id or train_id",
                tool
            ))),
            (Some(id), None) => Ok(id.to_string()),
            (None, Some(train_id)) => {
                let Some(train) = self.trains.train(train_id) else {
                    return Err(Box::new(tool_error!("train not found: {}", train_id)));
                };
                train.calibrator_id().map_or_else(
                    || {
                        Err(Box::new(tool_error!(
                            "train '{}' has no cover calibrator",
                            train_id
                        )))
                    },
                    |id| Ok(id.to_string()),
                )
            }
        }
    }

    /// The ids of every optical train containing `calibrator_id` — the
    /// trains a closed cover blinds and a lit panel floods. Empty for a
    /// calibrator outside every train.
    pub(crate) fn trains_with_calibrator(&self, calibrator_id: &str) -> Vec<String> {
        self.trains
            .trains_with_device(calibrator_id)
            .iter()
            .map(|t| t.id.clone())
            .collect()
    }
}
