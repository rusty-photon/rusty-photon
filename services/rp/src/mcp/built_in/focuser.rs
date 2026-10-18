use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_router, RoleServer};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::super::handler::McpHandler;
use super::super::inflight::Cancel;
use super::super::progress::{ProgressEmitter, ProgressSink};
use super::super::{resolve_device, tool_error, tool_success};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FocuserIdParams {
    pub focuser_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoveFocuserParams {
    pub focuser_id: String,
    pub position: i32,
    /// A held train lease to move under (docs/services/rp.md § Train
    /// Lease): admitted only for the focuser the lease moves. Omitted,
    /// a move of a focuser a lease reserves waits for the lease to end.
    #[serde(default)]
    pub lease_id: Option<String>,
}

#[tool_router(router = tool_router_focuser, vis = "pub")]
impl McpHandler {
    #[tool(description = "Move the focuser to an absolute position (blocks until idle)")]
    pub(crate) async fn move_focuser(
        &self,
        Parameters(params): Parameters<MoveFocuserParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let sink = ProgressSink::from_request_context(&ctx);
        let emitter = sink.as_ref().map(ProgressSink::as_emitter);
        let cancel = Cancel::from_context(&ctx);
        self.move_focuser_inner(params, emitter, &cancel).await
    }

    /// Body of the `move_focuser` MCP tool, split out so unit tests
    /// can pass `None` for the progress emitter without constructing
    /// a real rmcp `Peer`.
    pub(crate) async fn move_focuser_inner(
        &self,
        params: MoveFocuserParams,
        progress: Option<&dyn ProgressEmitter>,
        cancel: &Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match self
            .do_move_focuser_under(
                &params.focuser_id,
                params.position,
                params.lease_id.as_deref(),
                progress,
                cancel,
            )
            .await
        {
            Ok(outcome) => Ok(tool_success!({
                "focuser_id": params.focuser_id,
                "actual_position": outcome.position,
                "backlash_compensated": outcome.backlash_compensated,
            })),
            Err(e) => Ok(tool_error!("{}", e)),
        }
    }

    #[tool(
        description = "Read the current absolute position of the focuser, its travel bounds (min_position / max_position: the configured bounds narrowed to the driver's 0..MaxStep, which stands in where the config sets none; max_position is null only when no ceiling is known) and its configured backlash block (approach and steps; null when the config sets none)"
    )]
    pub(crate) async fn get_focuser_position(
        &self,
        Parameters(params): Parameters<FocuserIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(entry) = self.equipment.find_focuser(&params.focuser_id) else {
            return Ok(tool_error!("focuser not found: {}", params.focuser_id));
        };
        let Some((foc, (min_position, max_position))) = entry.device_with_travel_bounds() else {
            return Ok(tool_error!("focuser not connected: {}", params.focuser_id));
        };

        let position = match foc.position().await {
            Ok(p) => p,
            Err(e) => return Ok(tool_error!("failed to read focuser position: {}", e)),
        };
        let backlash = entry.config.backlash;
        debug!(
            focuser_id = %params.focuser_id,
            position,
            ?min_position,
            ?max_position,
            ?backlash,
            "get_focuser_position"
        );

        Ok(tool_success!({
            "focuser_id": params.focuser_id,
            "position": position,
            "min_position": min_position,
            "max_position": max_position,
            "backlash": backlash,
        }))
    }

    #[tool(description = "Read the focuser temperature sensor (null if not implemented)")]
    pub(crate) async fn get_focuser_temperature(
        &self,
        Parameters(params): Parameters<FocuserIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (_entry, foc) = resolve_device!(self, find_focuser, &params.focuser_id, "focuser");

        // ASCOM `Temperature` and `TempCompAvailable` are independent: a
        // focuser may expose a temperature reading while reporting
        // `TempCompAvailable=false` (qhy-focuser is the canonical local
        // example). Try the temperature read directly and only translate
        // a `NOT_IMPLEMENTED` rejection to `null`; surface every other
        // error to the caller.
        let temperature_c: Option<f64> = match foc.temperature().await {
            Ok(t) => Some(t),
            Err(e) if e.code == ascom_alpaca::ASCOMErrorCode::NOT_IMPLEMENTED => None,
            Err(e) => return Ok(tool_error!("failed to read focuser temperature: {}", e)),
        };

        Ok(tool_success!({
            "focuser_id": params.focuser_id,
            "temperature_c": temperature_c,
        }))
    }
}
