//! The `get_safety_status` tool (rp.md § Safety → In-Flight Tool
//! Calls → `get_safety_status`): the state the safety gate acts on,
//! for a client that does not consume the `safety_changed` SSE
//! stream.

use chrono::{DateTime, SecondsFormat, Utc};
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use tracing::debug;

use super::super::handler::McpHandler;
use super::super::tool_success;

const fn state_name(safe: bool) -> &'static str {
    if safe {
        "safe"
    } else {
        "unsafe"
    }
}

fn stamp(at: &DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[tool_router(router = tool_router_safety, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Read the safety state the gate acts on: overall safe/unsafe with the \
                       time it last changed, every configured safety monitor's last reading \
                       (a failed read counts as unsafe), and the effective gated tool list \
                       after safety.gate overrides. Read-only; safety_changed is the push signal"
    )]
    pub(crate) async fn get_safety_status(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let snapshot = self.safety.snapshot();
        debug!(
            overall = state_name(snapshot.overall),
            monitors = snapshot.monitors.len(),
            "read safety status"
        );
        let monitors: Vec<serde_json::Value> = snapshot
            .monitors
            .iter()
            .map(|monitor| {
                serde_json::json!({
                    "id": monitor.id,
                    "state": state_name(monitor.safe),
                    "since": stamp(&monitor.since),
                })
            })
            .collect();
        Ok(tool_success!({
            "overall": state_name(snapshot.overall),
            "since": stamp(&snapshot.since),
            "monitors": monitors,
            "gated": self.classes.gated(),
        }))
    }
}
