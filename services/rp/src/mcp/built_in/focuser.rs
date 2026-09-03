use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_router, RoleServer};
use schemars::JsonSchema;
use serde::Deserialize;

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
            .do_move_focuser_blocking(&params.focuser_id, params.position, progress, cancel)
            .await
        {
            Ok(actual_position) => Ok(tool_success!({
                "focuser_id": params.focuser_id,
                "actual_position": actual_position,
            })),
            Err(e) => Ok(tool_error!("{}", e)),
        }
    }

    #[tool(description = "Read the current absolute position of the focuser")]
    pub(crate) async fn get_focuser_position(
        &self,
        Parameters(params): Parameters<FocuserIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (_entry, foc) = resolve_device!(self, find_focuser, &params.focuser_id, "focuser");

        let position = match foc.position().await {
            Ok(p) => p,
            Err(e) => return Ok(tool_error!("failed to read focuser position: {}", e)),
        };

        Ok(tool_success!({
            "focuser_id": params.focuser_id,
            "position": position,
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
