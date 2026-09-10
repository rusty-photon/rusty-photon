use thiserror::Error;

use crate::store::StoreError;

pub type Result<T> = std::result::Result<T, FocusModelError>;

#[derive(Debug, Error)]
pub enum FocusModelError {
    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// An `rp` tool call failed: the request could not be sent, `rp`
    /// answered a tool error, or the reply was not the expected shape.
    #[error("MCP tool call failed: {0}")]
    ToolCall(String),

    /// The caller cancelled the run. The in-flight `rp` call was told
    /// (`notifications/cancelled`); the put-back ran before this
    /// surfaced.
    #[error("cancelled: {0}")]
    Cancelled(String),

    /// A refusal or failure of the focus workflow itself: an unknown
    /// filter, incomplete optics, a sweep that failed after every
    /// attempt.
    #[error("workflow error: {0}")]
    Workflow(String),

    #[error("server error: {0}")]
    Server(String),

    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

impl FocusModelError {
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

    /// Whether this is the caller's cancellation rather than a failure.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled(_))
    }
}
