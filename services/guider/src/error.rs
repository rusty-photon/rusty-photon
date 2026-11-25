use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, GuiderError>;

/// Why the service cannot start or stopped serving.
#[derive(Debug, Error)]
pub enum GuiderError {
    #[error("config error: {0}")]
    Config(String),

    /// `ca_cert` cannot be read or holds no certificate.
    #[error("ca_cert {}: {message}", path.display())]
    CaCert { path: PathBuf, message: String },

    /// The state directory cannot be created or written.
    #[error("state directory {}: {message}", path.display())]
    StateDir { path: PathBuf, message: String },

    /// A device client cannot be built from its address.
    #[error("device client for {url}: {message}")]
    DeviceClient { url: String, message: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("server error: {0}")]
    Server(String),

    /// The camera loop's task ended without a stop: the process exits so
    /// that its supervisor starts it again (guider.md § Failure domains).
    #[error("the camera loop's task ended without a stop")]
    LoopEnded,
}
