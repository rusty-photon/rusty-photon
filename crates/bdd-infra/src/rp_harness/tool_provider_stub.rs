//! Restartable in-process stub of a tool-provider plugin (rp.md § Tool
//! Provider Registration): an rmcp MCP server offering `echo` and a
//! long-running `slow_echo`, plus any extra echo-shaped tool names a
//! scenario asks for (a name colliding with a built-in, say).
//!
//! rp's tool-provider BDD scenarios need a provider they can register,
//! call through rp, take down and bring back **on the same port** — so
//! the stub holds its socket through a stop (`held_port` says why) — and
//! whose own view of events they can assert on: every call it served and
//! every request it saw cancelled by its client (rp forwarding a safety
//! stop) are recorded.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use rmcp::handler::server::router::tool::{ToolRoute, ToolRouter};
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::RoleServer;
use serde_json::{json, Value};

use super::held_port::{self, SharedListener};

/// How long `slow_echo` runs when the call names no `delay_ms`: long
/// enough that a scenario's safety flip lands in the middle of it.
const SLOW_ECHO_DEFAULT_DELAY: Duration = Duration::from_secs(30);

/// Cadence of `slow_echo`'s `notifications/progress` while it runs.
const SLOW_ECHO_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// What the stub saw, shared by every incarnation across restarts.
#[derive(Debug, Default)]
struct Log {
    /// Every `(tool, arguments)` call served, in order.
    calls: Mutex<Vec<(String, Value)>>,
    /// Every tool whose in-flight request the client cancelled, in order.
    cancelled: Mutex<Vec<String>>,
}

impl Log {
    fn record_call(&self, tool: &str, args: &Value) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((tool.to_owned(), args.clone()));
    }

    fn record_cancelled(&self, tool: &str) {
        self.cancelled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(tool.to_owned());
    }
}

/// The rmcp handler: one router of echo-shaped tools over the shared log.
#[derive(Clone)]
struct StubHandler {
    router: ToolRouter<Self>,
}

impl StubHandler {
    fn new(tools: &[String], log: &Arc<Log>) -> Self {
        let mut router = ToolRouter::new();
        for name in tools {
            let route = if name == "slow_echo" {
                slow_echo_route(Arc::clone(log))
            } else {
                echo_route(name.clone(), Arc::clone(log))
            };
            router.add_route(route);
        }
        Self { router }
    }
}

impl ServerHandler for StubHandler {
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(self.router.list_all())))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.router.get(name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.router
            .call(ToolCallContext::new(self, request, context))
            .await
    }
}

/// The schema every echo-shaped tool advertises: any object.
fn any_object_schema() -> Arc<serde_json::Map<String, Value>> {
    let mut schema = serde_json::Map::new();
    schema.insert("type".to_owned(), json!("object"));
    Arc::new(schema)
}

/// `<name>`: answers its arguments back as one JSON text block.
fn echo_route(name: String, log: Arc<Log>) -> ToolRoute<StubHandler> {
    let tool = Tool::new(
        name.clone(),
        format!("stub provider tool `{name}`: echoes its arguments"),
        any_object_schema(),
    );
    ToolRoute::new_dyn(tool, move |context: ToolCallContext<'_, StubHandler>| {
        let log = Arc::clone(&log);
        let name = name.clone();
        Box::pin(async move {
            let args = Value::Object(context.arguments.unwrap_or_default());
            log.record_call(&name, &args);
            Ok(CallToolResult::success(vec![ContentBlock::text(args.to_string())]).into())
        })
    })
}

/// `slow_echo`: waits `delay_ms` (default [`SLOW_ECHO_DEFAULT_DELAY`])
/// emitting progress, then echoes its arguments — unless its request is
/// cancelled first, which it records and answers with a tool error.
fn slow_echo_route(log: Arc<Log>) -> ToolRoute<StubHandler> {
    let tool = Tool::new(
        "slow_echo",
        "stub provider tool: echoes its arguments after `delay_ms`",
        any_object_schema(),
    );
    ToolRoute::new_dyn(tool, move |context: ToolCallContext<'_, StubHandler>| {
        let log = Arc::clone(&log);
        Box::pin(async move {
            let args = Value::Object(context.arguments.unwrap_or_default());
            log.record_call("slow_echo", &args);
            let delay = args
                .get("delay_ms")
                .and_then(Value::as_u64)
                .map_or(SLOW_ECHO_DEFAULT_DELAY, Duration::from_millis);
            let request = context.request_context;
            let token = request.meta.get_progress_token();
            let started = tokio::time::Instant::now();
            let deadline = tokio::time::sleep(delay);
            tokio::pin!(deadline);
            let mut ticks = tokio::time::interval(SLOW_ECHO_PROGRESS_INTERVAL);
            loop {
                tokio::select! {
                    () = &mut deadline => break,
                    () = request.ct.cancelled() => {
                        log.record_cancelled("slow_echo");
                        return Ok(CallToolResult::error(vec![ContentBlock::text(
                            "cancelled by client",
                        )])
                        .into());
                    }
                    _ = ticks.tick() => {
                        if let Some(token) = &token {
                            let mut progress = ProgressNotificationParam::new(
                                token.clone(),
                                started.elapsed().as_secs_f64(),
                            );
                            progress.total = Some(delay.as_secs_f64());
                            progress.message = Some("slow_echo".to_owned());
                            let _ = request.peer.notify_progress(progress).await;
                        }
                    }
                }
            }
            Ok(CallToolResult::success(vec![ContentBlock::text(args.to_string())]).into())
        })
    })
}

/// In-process tool-provider MCP server that can be stopped and brought
/// back on the same port. Hold the handle alive for the scenario;
/// dropping it shuts the listener down best-effort.
#[derive(Debug)]
pub struct ToolProviderStub {
    port: u16,
    tools: Vec<String>,
    log: Arc<Log>,
    /// The one socket the stub ever listens on; shared with whichever
    /// task currently owns the port — the server, or the refuser that
    /// stands in for a stopped provider.
    listener: Arc<tokio::net::TcpListener>,
    /// Ends the current task (server or refuser).
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ToolProviderStub {
    /// Spawn the stub on an OS-assigned loopback port offering `echo`
    /// and `slow_echo`.
    ///
    /// # Panics
    ///
    /// Panics if no loopback port can be bound.
    #[must_use]
    pub fn start() -> Self {
        Self::start_offering(&["echo", "slow_echo"])
    }

    /// Spawn the stub offering exactly `tools`: `slow_echo` gets the
    /// long-running body, every other name the echo body — so a
    /// scenario can offer a name that collides with an rp built-in.
    ///
    /// # Panics
    ///
    /// Panics if no loopback port can be bound.
    #[must_use]
    pub fn start_offering(tools: &[&str]) -> Self {
        let listener = held_port::bind_loopback().expect("failed to bind tool provider stub");
        let port = listener
            .local_addr()
            .expect("stub has no local addr")
            .port();
        let mut stub = Self {
            port,
            tools: tools.iter().map(|s| (*s).to_owned()).collect(),
            log: Arc::new(Log::default()),
            listener: Arc::new(listener),
            shutdown_tx: None,
            task: None,
        };
        stub.serve();
        stub
    }

    /// The `mcp_server_url` rp's registration should point at. Stable
    /// across [`Self::stop`] / [`Self::restart`].
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// The tool names this stub offers.
    #[must_use]
    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    /// Every `(tool, arguments)` call served so far, across restarts.
    #[must_use]
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.log
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Every tool whose in-flight request the client cancelled so far.
    #[must_use]
    pub fn cancelled(&self) -> Vec<String> {
        self.log
            .cancelled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Stop the server: every request from now on fails at the
    /// transport (the connection is accepted and dropped unanswered),
    /// as against a provider that is down — but the port stays bound,
    /// so nothing else can take it before [`Self::restart`].
    pub async fn stop(&mut self) {
        self.halt().await;
        let (shutdown_tx, task) = held_port::refuse(self.listener.clone());
        self.shutdown_tx = Some(shutdown_tx);
        self.task = Some(task);
    }

    /// Bring the server back on the same port, offering the same tools.
    pub async fn restart(&mut self) {
        self.halt().await;
        self.serve();
    }

    /// End whichever task holds the port (server or refuser) and wait
    /// for it, so the next task is the socket's only user.
    async fn halt(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    /// Serve the tools on the shared socket.
    fn serve(&mut self) {
        let listener = SharedListener(self.listener.clone());
        let handler = StubHandler::new(&self.tools, &self.log);
        // Session-less, like rp's own transport: every request is served
        // statelessly, which is what rp's client (MCP revision 2026-07-28,
        // `server/discover` bootstrap) speaks.
        let mut config = StreamableHttpServerConfig::default();
        config.legacy_session_mode = false;
        config.json_response = true;
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(NeverSessionManager::default()),
            config,
        );
        let app = Router::new().nest_service("/mcp", service);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("tool provider stub failed");
        });
        self.shutdown_tx = Some(shutdown_tx);
        self.task = Some(task);
    }
}

impl Drop for ToolProviderStub {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::rp_harness::McpTestClient;

    /// The stub speaks what rp's standard client speaks: discover, list,
    /// call — and echoes what it was given.
    #[tokio::test]
    async fn echo_answers_its_arguments_through_the_standard_client() {
        let stub = ToolProviderStub::start();
        let client = McpTestClient::connect(&stub.url()).await.unwrap();
        let mut tools = client.list_tools().await.unwrap();
        tools.sort();
        assert_eq!(tools, ["echo", "slow_echo"]);
        let result = client
            .call_tool("echo", json!({"message": "hello"}))
            .await
            .unwrap();
        assert_eq!(result["message"], "hello");
        assert_eq!(
            stub.calls(),
            vec![("echo".to_owned(), json!({"message": "hello"}))]
        );
    }

    /// A stop and a restart keep the port and the tool set, and the log
    /// spans both incarnations.
    #[tokio::test]
    async fn restart_keeps_the_port_and_the_log() {
        let mut stub = ToolProviderStub::start_offering(&["echo", "capture"]);
        let url = stub.url();
        let client = McpTestClient::connect(&url).await.unwrap();
        client.call_tool("capture", json!({})).await.unwrap();
        stub.stop().await;
        assert!(McpTestClient::connect(&url).await.is_err());
        stub.restart().await;
        assert_eq!(stub.url(), url);
        let client = McpTestClient::connect(&url).await.unwrap();
        client.call_tool("echo", json!({"n": 1})).await.unwrap();
        let calls: Vec<String> = stub.calls().into_iter().map(|(tool, _)| tool).collect();
        assert_eq!(calls, ["capture", "echo"]);
    }

    /// The port is never released: while the stub is stopped nothing
    /// else can bind it, so a concurrent shard's server can never sit
    /// where rp expects the stopped provider.
    #[tokio::test]
    async fn the_port_stays_bound_while_stopped() {
        let mut stub = ToolProviderStub::start();
        let url = stub.url();
        stub.stop().await;

        let taken = std::net::TcpListener::bind(("127.0.0.1", stub.port)).map(|_| ());
        assert!(
            taken.is_err(),
            "a stopped stub must keep its port bound, but a second bind succeeded"
        );

        stub.restart().await;
        let client = McpTestClient::connect(&url).await.unwrap();
        client.call_tool("echo", json!({})).await.unwrap();
    }

    /// `slow_echo` with a short delay finishes on its own; nothing is
    /// recorded as cancelled.
    #[tokio::test]
    async fn slow_echo_finishes_after_its_delay() {
        let stub = ToolProviderStub::start();
        let client = McpTestClient::connect(&stub.url()).await.unwrap();
        let result = client
            .call_tool("slow_echo", json!({"delay_ms": 50}))
            .await
            .unwrap();
        assert_eq!(result["delay_ms"], 50);
        let cancelled = stub.cancelled();
        assert!(cancelled.is_empty(), "{cancelled:?}");
    }
}
