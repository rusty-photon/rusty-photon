//! The `start_cooldown` / `start_warmup` tools (rp.md § Camera Cooling).
//!
//! The workflow decides when to cool and when to warm, rp decides which
//! rung. Both are ungated (a cooler setpoint is indoor work; a warm-up
//! secures), return at once with the cameras they drive, and are
//! idempotent — the bodies live in [`crate::cooling::CoolingController`].

use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use tracing::debug;

use super::super::handler::McpHandler;
use super::super::{tool_error, tool_success};

#[tool_router(router = tool_router_cooling, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Cool every camera with a cooler_targets_c ladder to a dark-library \
                       setpoint, in the background: the lowest rung that stabilizes with \
                       power headroom (snapping up past tonight's floor), or cooler off \
                       when none is reachable (cooler_unreachable). Returns at once with \
                       the camera ids being driven; cooler_stabilized announces the rung. \
                       Idempotent: a running pass is left alone, a cooler already \
                       regulating at a ladder rung is adopted without re-selection, a \
                       running warm-up is cancelled and superseded"
    )]
    pub(crate) async fn start_cooldown(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(cooling) = self.cooling.as_ref() else {
            return Ok(tool_error!("camera cooling is not configured"));
        };
        let cameras = cooling.start_cooldown();
        debug!(cameras = ?cameras, "start_cooldown issued");
        Ok(tool_success!({ "cameras": cameras }))
    }

    #[tool(
        description = "Ramp every camera rp is cooling warm (+5 °C steps) and switch its \
                       cooler off, in the background; cooler_warmup_started / \
                       cooler_warmup_complete bracket the ramp. A cooldown pass still \
                       running is taken over: it is cancelled and the ramp starts from \
                       the setpoint it had commanded. Returns at once with the camera \
                       ids being warmed (empty when rp commands none). Idempotent: a \
                       running ramp is left alone"
    )]
    pub(crate) async fn start_warmup(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(cooling) = self.cooling.as_ref() else {
            return Ok(tool_error!("camera cooling is not configured"));
        };
        let cameras = cooling.start_warmup();
        debug!(cameras = ?cameras, "start_warmup issued");
        Ok(tool_success!({ "cameras": cameras }))
    }
}
