//! In-process stub of rp's MCP server for the bridge BDD suite.
//!
//! Serves the same rmcp streamable-HTTP transport rp does
//! (`services/rp/src/routes.rs`), so the bridge's real client path needs no
//! test seams. Every `tools/call` is recorded for the scenarios to assert
//! on; `set_reject(true)` turns each call into a tool error
//! (`CallToolResult::error`) — the "rp actively rejected it" path that must
//! never spool. rp-side *semantics* (naming, dedup, slug allocation) are
//! covered in rp's own suite against the real store, never here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ServerCapabilities,
    ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::sync::oneshot;

/// One recorded `tools/call`.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub tool: String,
    pub arguments: serde_json::Value,
}

#[derive(Clone)]
struct StubHandler {
    calls: Arc<Mutex<Vec<RecordedCall>>>,
    reject: Arc<AtomicBool>,
}

impl ServerHandler for StubHandler {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let arguments = request
            .arguments
            .clone()
            .map_or(serde_json::Value::Null, serde_json::Value::Object);
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(RecordedCall {
                tool: request.name.to_string(),
                arguments,
            });
        if self.reject.load(Ordering::SeqCst) {
            Ok(CallToolResult::error(vec![ContentBlock::text(
                "stub rp: rejected by test configuration",
            )])
            .into())
        } else {
            Ok(CallToolResult::success(vec![ContentBlock::text(
                r#"{"slug":"stub-target","created":true}"#,
            )])
            .into())
        }
    }

    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

/// A running stub rp; dropping it does NOT stop the server — call
/// [`StubRp::stop`] (the cucumber `after` hook does).
#[derive(Debug)]
pub struct StubRp {
    port: u16,
    started_at: chrono::DateTime<chrono::Utc>,
    calls: Arc<Mutex<Vec<RecordedCall>>>,
    reject: Arc<AtomicBool>,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl StubRp {
    /// Bind on an OS-assigned port.
    pub async fn start() -> Self {
        Self::bind(0).await
    }

    /// Bind on a specific port — the "rp comes back up at the same address"
    /// half of the outage scenarios.
    pub async fn start_on(port: u16) -> Self {
        Self::bind(port).await
    }

    async fn bind(port: u16) -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reject = Arc::new(AtomicBool::new(false));
        let handler = StubHandler {
            calls: Arc::clone(&calls),
            reject: Arc::clone(&reject),
        };

        // Session-less, like the real rp (rp.md § MCP Server, ADR-021).
        let mut config = StreamableHttpServerConfig::default();
        config.legacy_session_mode = false;
        config.json_response = true;
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(NeverSessionManager::default()),
            config,
        );
        let router = axum::Router::new().nest_service("/mcp", service);

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind stub rp listener");
        let port = listener.local_addr().expect("stub rp local addr").port();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let join = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("stub rp serve");
        });

        Self {
            port,
            started_at: chrono::Utc::now(),
            calls,
            reject,
            shutdown: Some(shutdown_tx),
            join: Some(join),
        }
    }

    pub fn mcp_url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// When this stub instance came up — replay-provenance assertions compare
    /// spooled `received_at` stamps against it.
    pub const fn started_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.started_at
    }

    /// Make every subsequent `tools/call` return a tool error.
    pub fn set_reject(&self, reject: bool) {
        self.reject.store(reject, Ordering::SeqCst);
    }

    /// The recorded `add_target` calls, in arrival order.
    pub fn add_target_calls(&self) -> Vec<RecordedCall> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|c| c.tool == "add_target")
            .cloned()
            .collect()
    }

    pub async fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}
