//! ASCOM Alpaca rotator client used by `PointingSource::Telescope`
//! when `pointing.rotator` is configured.
//!
//! The camera only needs the rotator's `position` (the synced sky
//! position angle, in degrees). Wrapping the giant
//! `ascom_alpaca::api::Rotator` trait in the narrower `RotatorReader`
//! trait (defined in `pointing.rs`) keeps unit-test mocks tiny — the
//! exact mirror of `mount.rs` / `MountReader`.
//!
//! Connection-state policy: this client **never** calls
//! `set_connected(true)` on the rotator. Whoever owns the rotator —
//! `rp`, a guiding stack, etc. — is responsible for that. A read
//! against a disconnected rotator surfaces the standard ASCOM error
//! via `RotatorReadError`.

use ascom_alpaca::api::{Rotator, TypedDevice};
use ascom_alpaca::Client;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::debug;

use crate::alpaca::build_alpaca_client;
use crate::config::RotatorFollowConfig;
use crate::error::{RotatorReadError, SkySurveyCameraError};
use crate::pointing::RotatorReader;

/// One-shot deadline for resolving the Rotator device on the Alpaca
/// server. Discovery is cached after success, so this only matters on
/// the first exposure (and on retry after a failure). Independent of
/// the configurable per-read timeout that bounds steady-state latency.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Production `RotatorReader` impl. Builds the Alpaca client at
/// construction (cheap, no network) and resolves the Rotator device
/// lazily on the first `position_angle` call. The resolved handle is
/// cached on success; on failure the cache is left empty so the next
/// call retries discovery.
#[derive(Debug)]
pub struct AlpacaRotatorReader {
    client: Client,
    device_number: u32,
    request_timeout: Duration,
    cached: RwLock<Option<Arc<dyn Rotator>>>,
}

impl AlpacaRotatorReader {
    pub fn from_config(config: &RotatorFollowConfig) -> Result<Self, SkySurveyCameraError> {
        let client = build_alpaca_client(&config.alpaca_url, config.auth.as_ref())
            .map_err(|e| SkySurveyCameraError::RotatorClient(e.to_string()))?;
        Ok(Self {
            client,
            device_number: config.device_number,
            request_timeout: config.request_timeout,
            cached: RwLock::new(None),
        })
    }

    async fn resolve_rotator(&self) -> Result<Arc<dyn Rotator>, RotatorReadError> {
        if let Some(r) = self.cached.read().await.as_ref() {
            return Ok(Arc::clone(r));
        }

        let devices = match tokio::time::timeout(DISCOVERY_TIMEOUT, self.client.get_devices()).await
        {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => return Err(RotatorReadError::Transport(e.to_string())),
            Err(_) => return Err(RotatorReadError::Timeout(DISCOVERY_TIMEOUT)),
        };

        let mut idx = 0u32;
        let mut found: Option<Arc<dyn Rotator>> = None;
        for device in devices {
            if let TypedDevice::Rotator(r) = device {
                if idx == self.device_number {
                    found = Some(r);
                    break;
                }
                idx = idx.saturating_add(1);
            }
        }

        let rotator = found.ok_or(RotatorReadError::DeviceNotFound {
            device_number: self.device_number,
        })?;

        *self.cached.write().await = Some(Arc::clone(&rotator));
        Ok(rotator)
    }
}

#[async_trait]
impl RotatorReader for AlpacaRotatorReader {
    async fn position_angle(&self) -> Result<f64, RotatorReadError> {
        let rotator = self.resolve_rotator().await?;
        let timeout = self.request_timeout;
        let read = async move {
            rotator
                .position()
                .await
                .map_err(|e| RotatorReadError::Ascom(e.to_string()))
        };
        match tokio::time::timeout(timeout, read).await {
            Ok(Ok(angle)) => {
                debug!(position_angle = angle, "rotator read");
                Ok(angle)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(RotatorReadError::Timeout(timeout)),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn cfg(url: &str) -> RotatorFollowConfig {
        RotatorFollowConfig {
            alpaca_url: url.into(),
            device_number: 0,
            request_timeout: Duration::from_secs(2),
            auth: None,
        }
    }

    #[test]
    fn from_config_accepts_valid_url() {
        AlpacaRotatorReader::from_config(&cfg("http://127.0.0.1:32324")).unwrap();
    }

    #[test]
    fn from_config_rejects_invalid_url() {
        let err = AlpacaRotatorReader::from_config(&cfg("not a url")).unwrap_err();
        assert!(matches!(err, SkySurveyCameraError::RotatorClient(_)));
    }

    #[tokio::test]
    async fn position_angle_surfaces_transport_error() {
        // Port 1 is privileged-but-unused on dev/CI machines; the
        // discovery roundtrip is refused immediately rather than
        // hanging. Mirrors mount.rs's transport-error test.
        let reader = AlpacaRotatorReader::from_config(&cfg("http://127.0.0.1:1")).unwrap();
        let err = reader.position_angle().await.unwrap_err();
        assert!(matches!(
            err,
            RotatorReadError::Transport(_) | RotatorReadError::Timeout(_)
        ));
    }
}
