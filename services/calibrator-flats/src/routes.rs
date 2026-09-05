//! HTTP routes: `GET /health` and the MCP endpoint `/mcp`.
//!
//! There is no run surface (docs/services/calibrator-flats.md § Overview):
//! MCP is the only way in, and `/health` stays for systemd, sentinel and
//! `doctor`. Both sit in one router so the `server.auth` layer `lib.rs`
//! wraps around it guards them alike.

use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::tools::FlatsHandler;

/// Build the router. `mcp_extra_allowed_hosts` extends rmcp's loopback
/// `Host` allowlist (see [`crate::additional_allowed_hosts`]).
pub fn build_router(handler: FlatsHandler, mcp_extra_allowed_hosts: Vec<String>) -> Router {
    // Session-less, like rp's own transport (ADR-021): every request is
    // served statelessly, which is what rp's client speaks.
    let mut mcp_config = StreamableHttpServerConfig::default();
    mcp_config.legacy_session_mode = false;
    // A `tools/call` answers as plain JSON unless the body emits a
    // notification first (progress), in which case rmcp upgrades to SSE.
    mcp_config.json_response = true;
    // rmcp's DNS-rebinding protection answers 403 to any `Host` outside
    // this list, whose defaults cover loopback only. Extend it, never
    // replace it: an empty list switches the protection off entirely.
    mcp_config.allowed_hosts.extend(mcp_extra_allowed_hosts);
    let mcp_service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(NeverSessionManager::default()),
        mcp_config,
    );

    Router::new()
        .route("/health", get(health))
        .nest_service("/mcp", mcp_service)
}

async fn health() -> &'static str {
    "calibrator-flats healthy"
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::net::SocketAddr;

    use super::*;
    use crate::store::FlatStore;

    async fn serve() -> (SocketAddr, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::parse_config(
            r#"{ "mcp_server_url": "http://127.0.0.1:1/mcp" }"#,
            "test",
        )
        .unwrap();
        let store = FlatStore::open(dir.path().join("flats.redb"))
            .await
            .unwrap();
        let handler = FlatsHandler::new(Arc::new(config), Arc::new(store));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, build_router(handler, Vec::new()))
                .await
                .unwrap();
        });
        (addr, dir)
    }

    #[tokio::test]
    async fn health_answers_without_rp() {
        let (addr, _dir) = serve().await;
        let body = reqwest::get(format!("http://{addr}/health"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "calibrator-flats healthy");
    }

    /// The provider answers `tools/list` with no rp in sight (plan D10):
    /// rp dials it at startup and there is no cycle.
    #[tokio::test]
    async fn tools_list_answers_without_rp() {
        let (addr, _dir) = serve().await;
        let client = rp_mcp_client::RpMcpClient::connect(&format!("http://{addr}/mcp"), None, None)
            .await
            .unwrap();
        let mut names: Vec<String> = client
            .list_tools()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        names.sort();
        assert_eq!(names, ["get_flat_training", "take_flats", "train_flats"]);
    }

    /// A tool call with no rp to reach is a tool error naming the URL —
    /// after the argument parse, so a malformed call is still a JSON-RPC
    /// error from rmcp.
    #[tokio::test]
    async fn a_tool_call_without_rp_is_a_tool_error_naming_the_url() {
        let (addr, _dir) = serve().await;
        let client = rp_mcp_client::RpMcpClient::connect(&format!("http://{addr}/mcp"), None, None)
            .await
            .unwrap();
        let mut args = serde_json::Map::new();
        args.insert("train_id".into(), serde_json::json!("main"));
        let err = client
            .call_tool("get_flat_training", args)
            .await
            .unwrap_err();
        let rp_mcp_client::McpCallError::Tool(message) = err else {
            panic!("expected a tool error, got {err:?}");
        };
        assert!(
            message.starts_with("rp at http://127.0.0.1:1/mcp is unreachable"),
            "{message}"
        );
    }
}
