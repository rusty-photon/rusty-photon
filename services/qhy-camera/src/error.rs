//! Startup / library error type for the qhy-camera service.
//!
//! This is the *lib/startup* error returned to `main` (SDK open, enumeration,
//! HTTP bind). It is deliberately distinct from the per-request ASCOM error
//! mapping: that happens at the `Device`/`Camera`/`FilterWheel` trait boundary
//! (each call site picks the right `ASCOMError`), exactly as the reference
//! `qhyccd-alpaca` driver and `sky-survey-camera` do.

use thiserror::Error;

/// Errors raised while building / binding the qhy-camera server.
#[derive(Debug, Error)]
pub enum QhyCameraError {
    /// The QHYCCD SDK could not be initialized (`Sdk::new`), or could not
    /// read an enumerated camera's model.
    #[error("QHYCCD SDK initialization failed: {0}")]
    Sdk(String),

    /// The configuration breaks a rule — its `usb_devices` list, every rule
    /// named.
    #[error("config: {0}")]
    Config(String),

    /// The staged USB inventory a simulation build was handed cannot be used.
    #[error("{0}")]
    UsbInventory(String),

    /// Binding the Alpaca HTTP listener failed.
    #[error("server bind on port {port}: {source}")]
    Bind {
        port: u16,
        source: rusty_photon_tls::error::TlsError,
    },

    /// The HTTP server returned an error while serving.
    #[error("server error: {0}")]
    Server(String),
}

/// Convenience alias for fallible service operations.
pub type Result<T> = std::result::Result<T, QhyCameraError>;
