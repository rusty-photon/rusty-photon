//! The MCP server half: the six focus tools `rp` aggregates.
//!
//! docs/services/focus-model.md § Tools for the contracts. Progress is
//! relayed as `notifications/progress` and cancellation honoured
//! through the request token (§ Put-back and cancellation).
//!
//! Each tool body runs on its own task: rmcp cancels the request token
//! on `notifications/cancelled`, the workflow's active client turns
//! that into a cancelled `rp` call, and the put-back that follows runs
//! to completion whether or not the transport is still waiting for the
//! answer.

use std::collections::BTreeMap;
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
use crate::error::Result as FocusResult;
use crate::mcp_client::McpClient;
use crate::store::FocusStore;
use crate::workflow::{self, FocusTrainParams, NoProgress, Progress, Rig};

/// The default number of runs `get_focus_runs` answers with.
const DEFAULT_RUN_LIMIT: usize = 20;

/// The rmcp handler: the config, the store and the tool router. rmcp
/// clones it per connection, so the shared parts sit behind `Arc`s.
#[derive(Clone)]
pub struct FocusHandler {
    config: Arc<Config>,
    store: Arc<FocusStore>,
    /// Held for the length of a `focus_train` call. The provider
    /// drives one observatory's focuser, wheel, camera and guider, so
    /// two sweeps at once would measure each other's moves and put
    /// each other's focuser back; the reads stay concurrent.
    focusing: Arc<tokio::sync::Mutex<()>>,
    tool_router: ToolRouter<Self>,
}

impl FocusHandler {
    #[must_use]
    pub fn new(config: Arc<Config>, store: Arc<FocusStore>) -> Self {
        Self {
            config,
            store,
            focusing: Arc::new(tokio::sync::Mutex::new(())),
            tool_router: Self::tool_router(),
        }
    }

    /// Claim the one focus run this provider runs at a time; `None`
    /// while another call holds it.
    fn claim_focus(&self) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        Arc::clone(&self.focusing).try_lock_owned().ok()
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
pub struct FocusTrainArgs {
    /// An `equipment.optical_trains[]` id.
    pub train_id: String,
    /// The filter to focus through; default the one in the path. Must
    /// be absent on a train without a wheel.
    #[serde(default)]
    pub filter: Option<String>,
    /// Walk the train's whole refocus plan — its shared focusers
    /// upstream-first, the guiding step last — instead of its own
    /// focuser alone.
    #[serde(default)]
    pub shared: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrainAndFilterArgs {
    /// An `equipment.optical_trains[]` id.
    pub train_id: String,
    /// One wheel filter name; default the one in the path.
    #[serde(default)]
    pub filter: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrainArgs {
    /// An `equipment.optical_trains[]` id.
    pub train_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFocusRunsArgs {
    /// An `equipment.optical_trains[]` id.
    pub train_id: String,
    /// How many runs to answer with, newest first; default 20.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Restrict the history to one filter's runs.
    #[serde(default)]
    pub filter: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetFocusOffsetsArgs {
    /// An `equipment.optical_trains[]` id.
    pub train_id: String,
    /// The filter every offset is measured from; it maps to 0.
    pub reference: String,
    /// Filter name to steps relative to the reference.
    pub offsets: BTreeMap<String, i32>,
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
fn finish<T: serde::Serialize>(tool: &str, outcome: FocusResult<T>) -> CallToolResult {
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
    store: Arc<FocusStore>,
    cancel: CancellationToken,
    progress: Arc<dyn Progress>,
}

impl Run {
    fn new(handler: &FocusHandler, ctx: &RequestContext<RoleServer>) -> Self {
        Self {
            config: Arc::clone(&handler.config),
            store: Arc::clone(&handler.store),
            cancel: ctx.ct.clone(),
            progress: RmcpProgress::from_context(ctx),
        }
    }

    /// Connect to `rp` for this run: the active client under the
    /// request token, and the put-back client under one nothing fires.
    async fn connect(&self) -> FocusResult<(McpClient, McpClient)> {
        let active = McpClient::connect(&self.config, self.cancel.clone()).await?;
        let cleanup = active.uncancellable();
        Ok((active, cleanup))
    }
}

/// Run `body` on its own task and hand its result back; a panic in the
/// body is a tool error rather than a dropped request. The panic's text
/// stays in this service's log — the caller, on the far side of rp's
/// proxy, gets a generic message.
async fn detached<F>(tool: &str, body: F) -> std::result::Result<CallToolResult, ErrorData>
where
    F: std::future::Future<Output = CallToolResult> + Send + 'static,
{
    match tokio::spawn(body).await {
        Ok(result) => Ok(result),
        Err(e) => {
            tracing::error!(tool, error = %e, "the run task failed");
            Ok(tool_error!(
                "{tool}: internal error in the provider; see the focus-model log"
            ))
        }
    }
}

#[tool_router]
impl FocusHandler {
    #[tool(
        description = "Focus an optical train: sizes the V-curve sweep from the train's optics and the filter's wavelength, predicts the start from the remembered focus, filter offset and temperature model, moves there, walks the sweep through rp's move_focuser / capture / measure_stars, gates sparse samples, fits, confirms the vertex with a fresh frame and retries a failed fit with the grid shifted toward the lowest sample. Pauses guide corrections around a guide-coupled focuser and resumes after. A sweep that fails after every attempt puts the focuser back where the call found it. Every run — confirmed, fallback or failed — is recorded with its curve points. With shared true it walks rp's whole refocus plan for the train, the guiding step last. Ungated: nothing here moves the mount or exposes the optics."
    )]
    async fn focus_train(
        &self,
        Parameters(args): Parameters<FocusTrainArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let Some(busy) = self.claim_focus() else {
            return Ok(tool_error!(
                "a focus run is already in progress; wait for it to finish or cancel it"
            ));
        };
        let run = Run::new(self, &ctx);
        let params = FocusTrainParams {
            train_id: args.train_id,
            filter: args.filter,
            shared: args.shared.unwrap_or(false),
        };
        detached("focus_train", async move {
            // Dropped with the task, so the next call waits for the
            // put-back too, not only for the last frame.
            let _busy = busy;
            let (active, cleanup) = match run.connect().await {
                Ok(pair) => pair,
                Err(e) => return tool_error!("{}", e.tool_message()),
            };
            let rig = Rig {
                active: &active,
                cleanup: &cleanup,
            };
            let outcome = if params.shared {
                workflow::focus_shared(rig, &run.store, &run.config, &params, run.progress.as_ref())
                    .await
            } else {
                workflow::focus_train(rig, &run.store, &run.config, &params, run.progress.as_ref())
                    .await
            };
            finish("focus_train", outcome)
        })
        .await
    }

    #[tool(
        description = "The sweep focus_train would run for a train and filter, without running it: step_size, half_width, points, end_ratio and source (derived, configured or mixed), the optics the derivation used, the critical focus zone in steps, the focused HFR it was sized from, and the predicted and last measured wing slopes side by side, both in pixels per 100 steps. Writes nothing, moves nothing. Ungated."
    )]
    async fn get_sweep_plan(
        &self,
        Parameters(args): Parameters<TrainAndFilterArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let run = Run::new(self, &ctx);
        detached("get_sweep_plan", async move {
            let (active, _cleanup) = match run.connect().await {
                Ok(pair) => pair,
                Err(e) => return tool_error!("{}", e.tool_message()),
            };
            finish(
                "get_sweep_plan",
                workflow::get_sweep_plan(
                    &active,
                    &run.store,
                    &run.config,
                    &args.train_id,
                    args.filter.as_deref(),
                )
                .await,
            )
        })
        .await
    }

    #[tool(
        description = "What the provider remembers about a train: the identity the record is valid at, whether it is fresh, stale (naming every changed field) or empty, the reference filter and per-filter offsets, the temperature coefficient with its run count and span, the last good focus per filter, how many runs are recorded and the most recent one. The history itself is get_focus_runs. Touches no device. Ungated."
    )]
    async fn get_focus_model(
        &self,
        Parameters(args): Parameters<TrainArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let run = Run::new(self, &ctx);
        detached("get_focus_model", async move {
            let (active, _cleanup) = match run.connect().await {
                Ok(pair) => pair,
                Err(e) => return tool_error!("{}", e.tool_message()),
            };
            finish(
                "get_focus_model",
                workflow::get_focus_model(&active, &run.store, &args.train_id).await,
            )
        })
        .await
    }

    #[tool(
        description = "A train's focus runs, newest first, each with its outcome, prediction, fit and every curve point as the sweep measured it — including the runs that failed. limit defaults to 20 and filter restricts the list to one filter's runs; total counts them all. Touches no device. Ungated."
    )]
    async fn get_focus_runs(
        &self,
        Parameters(args): Parameters<GetFocusRunsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let run = Run::new(self, &ctx);
        detached("get_focus_runs", async move {
            let (active, _cleanup) = match run.connect().await {
                Ok(pair) => pair,
                Err(e) => return tool_error!("{}", e.tool_message()),
            };
            finish(
                "get_focus_runs",
                workflow::get_focus_runs(
                    &active,
                    &run.store,
                    &args.train_id,
                    args.limit.unwrap_or(DEFAULT_RUN_LIMIT),
                    args.filter.as_deref(),
                )
                .await,
            )
        })
        .await
    }

    #[tool(
        description = "Write a train's reference filter and per-filter focus offsets by hand, in focuser steps relative to the reference, which maps to 0. Every name is validated against the train's filter wheel before anything is written; a record whose identity no longer matches the train is replaced. Returns the model. Touches no device. Ungated."
    )]
    async fn set_focus_offsets(
        &self,
        Parameters(args): Parameters<SetFocusOffsetsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let run = Run::new(self, &ctx);
        detached("set_focus_offsets", async move {
            let (active, _cleanup) = match run.connect().await {
                Ok(pair) => pair,
                Err(e) => return tool_error!("{}", e.tool_message()),
            };
            finish(
                "set_focus_offsets",
                workflow::set_focus_offsets(
                    &active,
                    &run.store,
                    &args.train_id,
                    &args.reference,
                    args.offsets,
                )
                .await,
            )
        })
        .await
    }

    #[tool(
        description = "Forget what a re-homed or re-seated focuser invalidated: drops a train's run history, its last good focus per filter and its temperature coefficient, and keeps the reference filter and the offsets, which are differences between filters. Returns what was dropped, what was kept and the model. Touches no device. Ungated."
    )]
    async fn reset_focus_model(
        &self,
        Parameters(args): Parameters<TrainArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let run = Run::new(self, &ctx);
        detached("reset_focus_model", async move {
            let (active, _cleanup) = match run.connect().await {
                Ok(pair) => pair,
                Err(e) => return tool_error!("{}", e.tool_message()),
            };
            finish(
                "reset_focus_model",
                workflow::reset_focus_model(&active, &run.store, &args.train_id).await,
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
impl rmcp::handler::server::ServerHandler for FocusHandler {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        // The literal, not `CARGO_PKG_NAME`: rp logs providers by this
        // name, and Bazel builds the library under its crate name.
        info.server_info = Implementation::new("focus-model", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "Focus tool provider for rp: focus_train sizes a V-curve sweep from an optical \
             train's optics, predicts its start from the remembered focus and runs it through \
             rp's primitives; get_sweep_plan shows the sweep without running it; \
             get_focus_model and get_focus_runs read what the provider remembers; \
             set_focus_offsets and reset_focus_model write it. Address every tool by rp's \
             train_id."
                .to_owned(),
        );
        info
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::error::FocusModelError;

    async fn handler() -> (FocusHandler, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::parse_config(
            r#"{ "mcp_server_url": "http://127.0.0.1:1/mcp" }"#,
            "test",
        )
        .unwrap();
        let store = FocusStore::open(dir.path().join("focus.redb"))
            .await
            .unwrap();
        (FocusHandler::new(Arc::new(config), Arc::new(store)), dir)
    }

    #[tokio::test]
    async fn the_catalog_is_the_six_focus_tools() {
        let (handler, _dir) = handler().await;
        let mut names = handler.tool_names();
        names.sort();
        assert_eq!(
            names,
            [
                "focus_train",
                "get_focus_model",
                "get_focus_runs",
                "get_sweep_plan",
                "reset_focus_model",
                "set_focus_offsets",
            ]
        );
    }

    /// Two sweeps at once would measure each other's moves, so the
    /// second call is refused while the first holds the claim.
    #[tokio::test]
    async fn only_one_focus_run_is_claimed_at_a_time() {
        let (handler, _dir) = handler().await;
        let first = handler.claim_focus().expect("the first claim");
        assert!(handler.claim_focus().is_none(), "a second run is refused");
        drop(first);
        assert!(handler.claim_focus().is_some(), "the claim is released");
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

    #[tokio::test]
    async fn set_focus_offsets_requires_the_reference_and_the_offsets() {
        let (handler, _dir) = handler().await;
        let tool = handler
            .tool_router
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "set_focus_offsets")
            .unwrap();
        let required: Vec<String> = tool.input_schema["required"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();
        assert!(required.contains(&"reference".to_owned()), "{required:?}");
        assert!(required.contains(&"offsets".to_owned()), "{required:?}");
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
            Err(FocusModelError::Workflow(
                "train 'x' has no terminal focuser".into(),
            )),
        );
        assert_eq!(failed.is_error, Some(true));
        assert_eq!(
            failed.content[0].as_text().unwrap().text,
            "train 'x' has no terminal focuser"
        );
    }

    #[tokio::test]
    async fn get_info_advertises_tools() {
        let (handler, _dir) = handler().await;
        let info = rmcp::handler::server::ServerHandler::get_info(&handler);
        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.server_info.name, "focus-model");
    }
}
