//! Error type for the svbony-camera service.

use thiserror::Error;

/// Errors raised while building or running the svbony-camera server.
#[derive(Debug, Error)]
pub enum SvbonyCameraError {
    /// An error surfaced by the `svbony-rs` SDK wrapper (enumeration, open, …).
    #[error(transparent)]
    Sdk(#[from] svbony_rs::Error),

    /// The on-disk configuration could not be read or parsed.
    #[error("config error: {0}")]
    Config(String),

    /// A staged USB inventory (the simulation build's `--usb-inventory`)
    /// could not be read, or describes a state no collector could produce.
    #[error("{0}")]
    UsbInventory(String),

    /// Binding the Alpaca listener failed.
    #[error("failed to bind {addr}: {source}")]
    Bind {
        /// The address we tried to bind.
        addr: String,
        /// The underlying error from the dual-stack bind helper.
        #[source]
        source: rusty_photon_tls::error::TlsError,
    },

    /// The blocking enumeration task panicked or was cancelled.
    #[error("camera enumeration task failed: {0}")]
    Join(#[from] tokio::task::JoinError),

    /// The HTTP server stopped with an error.
    #[error("server error: {0}")]
    Server(String),

    /// Binding the Alpaca UDP discovery responder failed.
    #[error("failed to bind the Alpaca discovery responder: {0}")]
    Discovery(String),
}
