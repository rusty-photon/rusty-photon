use thiserror::Error;

#[derive(Debug, Error)]
pub enum SkySurveyCameraError {
    #[error("config I/O: {0}")]
    ConfigIo(#[from] std::io::Error),
    #[error("config parse: {0}")]
    ConfigParse(#[from] serde_json::Error),
    #[error("invalid config: {0}")]
    ConfigInvalid(String),
    #[error("device identity: {0}")]
    Identity(#[from] rusty_photon_config::ConfigError),
    #[error("server bind: {0}")]
    Bind(String),
    #[error("server: {0}")]
    Server(String),
    #[error("mount client: {0}")]
    MountClient(String),
    #[error("rotator client: {0}")]
    RotatorClient(String),
}

/// Outcome of a single Telescope read in follow mode. Surfaced via the
/// camera's `last_error` and ASCOM `UNSPECIFIED_ERROR` per F2.
#[derive(Debug, Error)]
pub enum MountReadError {
    #[error("mount read timed out after {0:?}")]
    Timeout(std::time::Duration),
    #[error("mount transport error: {0}")]
    Transport(String),
    #[error("mount ASCOM error: {0}")]
    Ascom(String),
    #[error("mount device {device_number} not found on Alpaca server")]
    DeviceNotFound { device_number: u32 },
}

/// Outcome of a single Rotator read in follow mode. Mirrors
/// [`MountReadError`]; surfaced via the camera's `last_error` and ASCOM
/// `UNSPECIFIED_ERROR` per F8 (same path as F2).
#[derive(Debug, Error)]
pub enum RotatorReadError {
    #[error("rotator read timed out after {0:?}")]
    Timeout(std::time::Duration),
    #[error("rotator transport error: {0}")]
    Transport(String),
    #[error("rotator ASCOM error: {0}")]
    Ascom(String),
    #[error("rotator device {device_number} not found on Alpaca server")]
    DeviceNotFound { device_number: u32 },
}

/// Failure of a follow-mode pointing snapshot.
///
/// A snapshot reads RA/Dec from the mount and — when `pointing.rotator`
/// is configured — the position angle from the rotator; either read can
/// fail. Both surface through the same `UNSPECIFIED_ERROR` exposure
/// path (F2/F8), so the exposure pipeline only needs the `Display`
/// text.
#[derive(Debug, Error)]
pub enum PointingReadError {
    #[error(transparent)]
    Mount(#[from] MountReadError),
    #[error(transparent)]
    Rotator(#[from] RotatorReadError),
}
