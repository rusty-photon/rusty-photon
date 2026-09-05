use thiserror::Error;

use crate::store::StoreError;

pub type Result<T> = std::result::Result<T, CalibratorFlatsError>;

#[derive(Debug, Error)]
pub enum CalibratorFlatsError {
    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// An `rp` tool call failed: the request could not be sent, `rp`
    /// answered a tool error, or the reply was not the expected shape.
    #[error("MCP tool call failed: {0}")]
    ToolCall(String),

    /// `rp` refused an `rp` tool for safety (JSON-RPC error `-32010`):
    /// the gate is closed. Distinct from [`Self::ToolCall`] so the
    /// cleanup guard can report a refused `open_cover` as the warning it
    /// is (docs/services/calibrator-flats.md § Cleanup and cancellation).
    #[error("rp refused the call for safety: {0}")]
    SafetyRefused(String),

    /// The caller cancelled the run. The in-flight `rp` call was told
    /// (`notifications/cancelled`); cleanup ran before this surfaced.
    #[error("cancelled: {0}")]
    Cancelled(String),

    #[error("workflow error: {0}")]
    Workflow(String),

    #[error("server error: {0}")]
    Server(String),

    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

impl CalibratorFlatsError {
    /// The text a tool error carries back to the caller: the bare cause
    /// for the variants whose prefix would only repeat what `isError`
    /// already says, the full `Display` for the rest.
    #[must_use]
    pub fn tool_message(&self) -> String {
        match self {
            Self::ToolCall(message) | Self::Workflow(message) => message.clone(),
            other => other.to_string(),
        }
    }
}
