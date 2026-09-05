//! The MCP server half: the three flats tools `rp` aggregates.
//!
//! docs/services/calibrator-flats.md § Tools for the contracts. Progress
//! is relayed as `notifications/progress` and cancellation honoured
//! through the request token (§ Cleanup and cancellation).
//!
//! Each tool body runs on its own task: rmcp cancels the request token
//! on `notifications/cancelled`, the workflow's active client turns that
//! into a cancelled `rp` call, and the cleanup that follows runs to
//! completion whether or not the transport is still waiting for the
//! answer.

use std::sync::Arc;

use async_trait::async_trait;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, Implementation, ProgressNotificationParam,
    ProgressToken, ServerCapabilities, ServerInfo,
};
use rmcp::service::{Peer, RequestContext};
use rmcp::{tool, tool_handler, tool_router, RoleServer};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::config::Config;
use crate::error::Result as FlatsResult;
use crate::mcp_client::McpClient;
use crate::store::FlatStore;
use crate::workflow::{self, NoProgress, Progress, Rig, TakeFlatsParams, TrainFlatsParams};

/// The rmcp handler: the config, the store and the tool router. rmcp
/// clones it per connection, so the shared parts sit behind `Arc`s.
#[derive(Clone)]
pub struct FlatsHandler {
    config: Arc<Config>,
    store: Arc<FlatStore>,
    tool_router: ToolRouter<Self>,
}

impl FlatsHandler {
    #[must_use]
    pub fn new(config: Arc<Config>, store: Arc<FlatStore>) -> Self {
        Self {
            config,
            store,
            tool_router: Self::tool_router(),
        }
    }

    /// The tool names this provider offers, in catalog order.
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrainFlatsArgs {
    /// An `equipment.optical_trains[]` id; its cover calibrator must be
    /// the first device.
    pub train_id: String,
    /// Wheel filter names to train, in this order. Default: every name
    /// the wheel reports. Must be absent on a filterless train.
    #[serde(default)]
    pub filters: Option<Vec<String>>,
    /// The brightness ladder's starting level. Default: the device
    /// maximum.
    #[serde(default)]
    pub brightness: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TakeFlatsArgs {
    /// An `equipment.optical_trains[]` id; its cover calibrator must be
    /// the first device.
    pub train_id: String,
    /// Flats per filter; at least 1.
    pub count: u32,
    /// Wheel filter names to take, in this order. Default: every name
    /// the wheel reports. Must be absent on a filterless train.
    #[serde(default)]
    pub filters: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFlatTrainingArgs {
    /// An `equipment.optical_trains[]` id.
    pub train_id: String,
    /// One wheel filter name; default: every record of the train.
    #[serde(default)]
    pub filter: Option<String>,
}

macro_rules! tool_success {
    ($value:expr) => {
        CallToolResult::success(vec![ContentBlock::text($value.to_string())])
    };
}

macro_rules! tool_error {
    ($($arg:tt)+) => {
        CallToolResult::error(vec![ContentBlock::text(format!($($arg)+))])
    };
}

/// Serialize a workflow outcome into the one-JSON-text-block result, or
/// the error into a tool error carrying its message.
fn finish<T: serde::Serialize>(tool: &str, outcome: FlatsResult<T>) -> CallToolResult {
    match outcome {
        Ok(value) => match serde_json::to_value(value) {
            Ok(json) => tool_success!(json),
            Err(e) => tool_error!("{tool}: failed to encode the result: {e}"),
        },
        Err(e) => {
            debug!(tool, error = %e, "tool failed");
            tool_error!("{}", e.tool_message())
        }
    }
}

/// `notifications/progress` for one request: the caller's token and the
/// peer to send through. `None` when the caller sent no `progressToken`.
struct RmcpProgress {
    peer: Peer<RoleServer>,
    token: ProgressToken,
}

impl RmcpProgress {
    fn from_context(ctx: &RequestContext<RoleServer>) -> Arc<dyn Progress> {
        match ctx.meta.get_progress_token() {
            Some(token) => Arc::new(Self {
                peer: ctx.peer.clone(),
                token,
            }),
            None => Arc::new(NoProgress),
        }
    }
}

#[async_trait]
impl Progress for RmcpProgress {
    async fn tick(&self, progress: f64, total: Option<f64>, message: String) {
        let mut param = ProgressNotificationParam::new(self.token.clone(), progress);
        param.total = total;
        param.message = Some(message);
        if let Err(e) = self.peer.notify_progress(param).await {
            debug!(error = %e, "notifications/progress could not be sent");
        }
    }
}

/// What a tool body needs, owned, so it can run on its own task.
struct Run {
    config: Arc<Config>,
    store: Arc<FlatStore>,
    cancel: CancellationToken,
    progress: Arc<dyn Progress>,
}

impl Run {
    fn new(handler: &FlatsHandler, ctx: &RequestContext<RoleServer>) -> Self {
        Self {
            config: Arc::clone(&handler.config),
            store: Arc::clone(&handler.store),
            cancel: ctx.ct.clone(),
            progress: RmcpProgress::from_context(ctx),
        }
    }

    /// Connect to `rp` for this run: the active client under the
    /// request token, and the cleanup client under one nothing fires.
    async fn connect(&self) -> FlatsResult<(McpClient, McpClient)> {
        let active = McpClient::connect(&self.config, self.cancel.clone()).await?;
        let cleanup = active.uncancellable();
        Ok((active, cleanup))
    }
}

/// Run `body` on its own task and hand its result back; a panic in the
/// body is a tool error rather than a dropped request. The panic's
/// text stays in this service's log — the caller, on the far side of
/// rp's proxy, gets a generic message.
async fn detached<F>(tool: &str, body: F) -> std::result::Result<CallToolResult, ErrorData>
where
    F: std::future::Future<Output = CallToolResult> + Send + 'static,
{
    match tokio::spawn(body).await {
        Ok(result) => Ok(result),
        Err(e) => {
            tracing::error!(tool, error = %e, "the run task failed");
            Ok(tool_error!(
                "{tool}: internal error in the provider; see the calibrator-flats log"
            ))
        }
    }
}

#[tool_router]
impl FlatsHandler {
    #[tool(
        description = "Learn the flat timing for an optical train: closes the cover, lights the panel and, per filter, runs the proportional exposure search (with the brightness ladder) until the median hits 50 % of max_adu, writing a record per converged filter keyed by train and filter. Reports `trained`, `unconverged` (a normal result, nothing written for those), `cover_restored` and `warnings`. Restores the cover only if it started open. Ungated: the optics are never exposed by this tool."
    )]
    async fn train_flats(
        &self,
        Parameters(args): Parameters<TrainFlatsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let run = Run::new(self, &ctx);
        let params = TrainFlatsParams {
            train_id: args.train_id,
            filters: args.filters,
            brightness: args.brightness,
        };
        detached("train_flats", async move {
            let (active, cleanup) = match run.connect().await {
                Ok(pair) => pair,
                Err(e) => return tool_error!("{}", e.tool_message()),
            };
            let rig = Rig {
                active: &active,
                cleanup: &cleanup,
            };
            finish(
                "train_flats",
                workflow::train_flats(rig, &run.store, &run.config, &params, run.progress.as_ref())
                    .await,
            )
        })
        .await
    }

    #[tool(
        description = "Capture `count` flats per filter of an optical train at the timing train_flats learned: checks every requested filter against the store before touching anything (an untrained or stale record — camera_id, max_adu, binning, gain or offset changed — fails the call naming the filter and the field, and nothing moves), then closes the cover, lights the panel at the trained brightness and captures as frame_type Flat. Every frame's median is verified after the fact; one outside 50 % ± flat_warn_tolerance is listed in `out_of_range` and `warnings`, never a failure. Restores the cover only if it started open. Ungated."
    )]
    async fn take_flats(
        &self,
        Parameters(args): Parameters<TakeFlatsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let run = Run::new(self, &ctx);
        let params = TakeFlatsParams {
            train_id: args.train_id,
            count: args.count,
            filters: args.filters,
        };
        detached("take_flats", async move {
            let (active, cleanup) = match run.connect().await {
                Ok(pair) => pair,
                Err(e) => return tool_error!("{}", e.tool_message()),
            };
            let rig = Rig {
                active: &active,
                cleanup: &cleanup,
            };
            finish(
                "take_flats",
                workflow::take_flats(rig, &run.store, &run.config, &params, run.progress.as_ref())
                    .await,
            )
        })
        .await
    }

    #[tool(
        description = "The flat-timing records of an optical train (or of one filter), each judged against the train's live camera the way take_flats judges it: `trained`, or `stale` with every changed field named. Touches no device. Ungated."
    )]
    async fn get_flat_training(
        &self,
        Parameters(args): Parameters<GetFlatTrainingArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let run = Run::new(self, &ctx);
        detached("get_flat_training", async move {
            let (active, _cleanup) = match run.connect().await {
                Ok(pair) => pair,
                Err(e) => return tool_error!("{}", e.tool_message()),
            };
            finish(
                "get_flat_training",
                workflow::get_flat_training(
                    &active,
                    &run.store,
                    &args.train_id,
                    args.filter.as_deref(),
                )
                .await,
            )
        })
        .await
    }
}

#[tool_handler]
#[expect(
    clippy::unused_async_trait_impl,
    reason = "the tool_handler expansion writes async trait methods whose bodies have no awaits"
)]
impl rmcp::handler::server::ServerHandler for FlatsHandler {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        // The literal, not `CARGO_PKG_NAME`: rp logs providers by this
        // name, and Bazel builds the library under its crate name.
        info.server_info = Implementation::new("calibrator-flats", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "Flat-field tool provider for rp: train_flats learns the exposure and panel \
             brightness per optical train and filter, take_flats captures at that timing, \
             get_flat_training reads the records. Address every tool by rp's train_id."
                .to_owned(),
        );
        info
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::error::CalibratorFlatsError;

    async fn handler() -> (FlatsHandler, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::parse_config(
            r#"{ "mcp_server_url": "http://127.0.0.1:1/mcp" }"#,
            "test",
        )
        .unwrap();
        let store = FlatStore::open(dir.path().join("flats.redb"))
            .await
            .unwrap();
        (FlatsHandler::new(Arc::new(config), Arc::new(store)), dir)
    }

    #[tokio::test]
    async fn the_catalog_is_the_three_flats_tools() {
        let (handler, _dir) = handler().await;
        let mut names = handler.tool_names();
        names.sort();
        assert_eq!(names, ["get_flat_training", "take_flats", "train_flats"]);
    }

    #[tokio::test]
    async fn every_tool_schema_requires_train_id() {
        let (handler, _dir) = handler().await;
        for tool in handler.tool_router.list_all() {
            let required = tool.input_schema["required"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            assert!(
                required.iter().any(|r| r == "train_id"),
                "{}: {required:?}",
                tool.name
            );
        }
    }

    #[test]
    fn finish_encodes_an_outcome_as_one_json_block_and_an_error_as_its_message() {
        let ok = finish("t", Ok(serde_json::json!({ "n": 1 })));
        assert_ne!(ok.is_error, Some(true));
        let text = ok.content[0].as_text().unwrap().text.clone();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["n"],
            1
        );

        let failed = finish::<()>(
            "t",
            Err(CalibratorFlatsError::Workflow(
                "train 'x' has no camera".into(),
            )),
        );
        assert_eq!(failed.is_error, Some(true));
        assert_eq!(
            failed.content[0].as_text().unwrap().text,
            "train 'x' has no camera"
        );
    }

    #[tokio::test]
    async fn get_info_advertises_tools() {
        let (handler, _dir) = handler().await;
        let info = rmcp::handler::server::ServerHandler::get_info(&handler);
        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.server_info.name, "calibrator-flats");
    }
}
