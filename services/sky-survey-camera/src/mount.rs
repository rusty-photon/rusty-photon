//! ASCOM Alpaca mount client used by `PointingSource::Telescope`.
//!
//! The camera only needs `right_ascension` / `declination` from the
//! ASCOM Telescope. Wrapping the giant `ascom_alpaca::api::Telescope`
//! trait in the narrower `MountReader` trait (defined in `pointing.rs`)
//! keeps unit-test mocks tiny.
//!
//! Connection-state policy: this client **never** calls
//! `set_connected(true)` on the Telescope. Whoever owns the mount —
//! typically the `rp` orchestrator in production, or the test harness
//! in BDD — is responsible for that. A read against a disconnected
//! Telescope surfaces the standard ASCOM error via `MountReadError`.

use ascom_alpaca::api::{Telescope, TypedDevice};
use ascom_alpaca::Client;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::debug;

use crate::alpaca::build_alpaca_client;
use crate::config::TelescopeFollowConfig;
use crate::error::{MountReadError, SkySurveyCameraError};
use crate::pointing::{MountPosition, MountReader};

/// One-shot deadline for resolving the Telescope device on the Alpaca
/// server. Discovery is cached after success, so this only matters on
/// the first exposure (and on retry after a failure). Independent of
/// the configurable per-read timeout that bounds steady-state latency.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Production `MountReader` impl.
///
/// Builds the Alpaca client at construction (cheap, no network) and
/// resolves the Telescope device lazily on the first `read_position`
/// call. The resolved handle is cached on success; on failure the cache
/// is left empty so the next call retries discovery.
#[derive(Debug)]
pub struct AlpacaMountReader {
    client: Client,
    device_number: u32,
    request_timeout: Duration,
    cached: RwLock<Option<Arc<dyn Telescope>>>,
}

impl AlpacaMountReader {
    /// Build the reader from the follow-mode telescope config.
    ///
    /// # Errors
    ///
    /// Returns [`SkySurveyCameraError::MountClient`] if the Alpaca
    /// client cannot be built from `alpaca_url`.
    pub fn from_config(config: &TelescopeFollowConfig) -> Result<Self, SkySurveyCameraError> {
        let client = build_alpaca_client(&config.alpaca_url, config.auth.as_ref())
            .map_err(|e| SkySurveyCameraError::MountClient(e.to_string()))?;
        Ok(Self {
            client,
            device_number: config.device_number,
            request_timeout: config.request_timeout,
            cached: RwLock::new(None),
        })
    }

    async fn resolve_telescope(&self) -> Result<Arc<dyn Telescope>, MountReadError> {
        if let Some(t) = self.cached.read().await.as_ref() {
            return Ok(Arc::clone(t));
        }

        let devices = match tokio::time::timeout(DISCOVERY_TIMEOUT, self.client.get_devices()).await
        {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => return Err(MountReadError::Transport(e.to_string())),
            Err(_) => return Err(MountReadError::Timeout(DISCOVERY_TIMEOUT)),
        };

        let mut idx = 0u32;
        let mut found: Option<Arc<dyn Telescope>> = None;
        for device in devices {
            if let TypedDevice::Telescope(t) = device {
                if idx == self.device_number {
                    found = Some(t);
                    break;
                }
                idx = idx.saturating_add(1);
            }
        }

        let telescope = found.ok_or(MountReadError::DeviceNotFound {
            device_number: self.device_number,
        })?;

        *self.cached.write().await = Some(Arc::clone(&telescope));
        Ok(telescope)
    }
}

#[async_trait]
impl MountReader for AlpacaMountReader {
    async fn read_position(&self) -> Result<MountPosition, MountReadError> {
        let telescope = self.resolve_telescope().await?;
        let timeout = self.request_timeout;
        let read = async move {
            let ra_hours = telescope
                .right_ascension()
                .await
                .map_err(|e| MountReadError::Ascom(e.to_string()))?;
            let dec_deg = telescope
                .declination()
                .await
                .map_err(|e| MountReadError::Ascom(e.to_string()))?;
            Ok::<_, MountReadError>(MountPosition { ra_hours, dec_deg })
        };
        match tokio::time::timeout(timeout, read).await {
            Ok(Ok(p)) => {
                debug!(ra_hours = p.ra_hours, dec_deg = p.dec_deg, "mount read");
                Ok(p)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(MountReadError::Timeout(timeout)),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn cfg(url: &str) -> TelescopeFollowConfig {
        TelescopeFollowConfig {
            alpaca_url: url.into(),
            device_number: 0,
            offset_ra_arcsec: 0.0,
            offset_dec_arcsec: 0.0,
            request_timeout: Duration::from_secs(2),
            auth: None,
        }
    }

    #[test]
    fn from_config_accepts_valid_url() {
        AlpacaMountReader::from_config(&cfg("http://127.0.0.1:32323")).unwrap();
    }

    #[test]
    fn from_config_rejects_invalid_url() {
        let err = AlpacaMountReader::from_config(&cfg("not a url")).unwrap_err();
        assert!(matches!(err, SkySurveyCameraError::MountClient(_)));
    }

    #[tokio::test]
    async fn read_position_surfaces_transport_error() {
        // Port 1 is reserved; nothing should answer. We expect the
        // discovery roundtrip to fail with a transport error rather
        // than hang or panic. The resolve timeout is short enough
        // (5s) that this completes quickly even when DNS resolution
        // succeeds.
        let reader = AlpacaMountReader::from_config(&cfg("http://127.0.0.1:1")).unwrap();
        let err = reader.read_position().await.unwrap_err();
        assert!(matches!(
            err,
            MountReadError::Transport(_) | MountReadError::Timeout(_)
        ));
    }
}
