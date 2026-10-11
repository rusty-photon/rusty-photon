use std::sync::Arc;

use ascom_alpaca::api::{CoverCalibrator, TypedDevice};
use tracing::{debug, error};

use super::binding::{establish_listed, RosterAddress};
use super::session::DeviceSession;
use crate::config;

pub struct CoverCalibratorEntry {
    pub id: String,
    pub config: config::CoverCalibratorConfig,
    pub session: DeviceSession<dyn CoverCalibrator>,
}

impl CoverCalibratorEntry {
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.session.is_connected()
    }

    #[must_use]
    pub fn device(&self) -> Option<Arc<dyn CoverCalibrator>> {
        self.session.device()
    }
}

/// Locate the configured cover calibrator on its Alpaca server and
/// switch it on — the shared routine behind the startup connect and the
/// reconnect supervisor's re-establish (rp.md § Device Session
/// Recovery).
pub(super) async fn establish_cover_calibrator(
    config: &config::CoverCalibratorConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> Result<Arc<dyn CoverCalibrator>, String> {
    let address = RosterAddress {
        kind: "cover calibrator",
        id: Some(&config.id),
        alpaca_url: &config.alpaca_url,
        device_number: config.device_number,
        unique_id: config.unique_id.as_ref(),
        auth: config.auth.as_ref(),
    };
    establish_listed(&address, ca_cert_path, |device| match device {
        TypedDevice::CoverCalibrator(device) => Some(device),
        _ => None,
    })
    .await
}

pub(super) async fn connect_cover_calibrator(
    config: &config::CoverCalibratorConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> CoverCalibratorEntry {
    debug!(cc_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to cover calibrator");

    match establish_cover_calibrator(config, ca_cert_path).await {
        Ok(cc) => {
            debug!(cc_id = %config.id, "cover calibrator connected successfully");
            CoverCalibratorEntry {
                id: config.id.clone(),
                config: config.clone(),
                session: DeviceSession::connected(cc),
            }
        }
        Err(msg) => {
            error!(cc_id = %config.id, error = %msg, "failed to connect cover calibrator");
            CoverCalibratorEntry {
                id: config.id.clone(),
                config: config.clone(),
                session: DeviceSession::disconnected(),
            }
        }
    }
}
