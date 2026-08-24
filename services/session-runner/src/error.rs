//! The service-level error type.
//!
//! Errors below the binary boundary stay `thiserror`-typed (ADR-011);
//! the engine and document layers have their own richer types
//! ([`crate::engine::WorkflowError`],
//! [`crate::document::ValidationIssue`]) — this covers service wiring:
//! configuration, the HTTP server, and the MCP connection.

#[derive(Debug, thiserror::Error)]
pub enum SessionRunnerError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("server error: {0}")]
    Server(String),
    #[error("MCP error: {0}")]
    Mcp(String),
}

pub type Result<T> = std::result::Result<T, SessionRunnerError>;
