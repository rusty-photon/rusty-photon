use std::sync::Arc;

use ascom_alpaca::api::{CoverCalibrator, TypedDevice};
use tracing::{debug, error};

use super::alpaca::{
    build_alpaca_client, retry_connect_attempt, AttemptOutcome, GET_DEVICES_TIMEOUT,
};
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
    let client = build_alpaca_client(&config.alpaca_url, config.auth.as_ref(), ca_cert_path)
        .map_err(|e| format!("failed to create Alpaca client: {e}"))?;

    let label = format!("cover calibrator {}", config.id);
    retry_connect_attempt(&label, |_attempt| async {
        let devices = match tokio::time::timeout(GET_DEVICES_TIMEOUT, client.get_devices()).await {
            Ok(Ok(devices)) => devices,
            Ok(Err(e)) => return AttemptOutcome::Transient(format!("get_devices: {e}")),
            Err(_) => {
                return AttemptOutcome::Transient(format!(
                    "get_devices: timeout after {GET_DEVICES_TIMEOUT:?}"
                ));
            }
        };

        let mut cc_index = 0u32;
        let mut found_cc: Option<Arc<dyn CoverCalibrator>> = None;
        for device in devices {
            if let TypedDevice::CoverCalibrator(cc) = device {
                if cc_index == config.device_number {
                    found_cc = Some(cc);
                    break;
                }
                cc_index = cc_index.saturating_add(1);
            }
        }

        let Some(cc) = found_cc else {
            return AttemptOutcome::Permanent(format!(
                "cover calibrator at index {} not found on Alpaca server",
                config.device_number
            ));
        };

        match cc.set_connected(true).await {
            Ok(()) => AttemptOutcome::Ok(cc),
            Err(e) => AttemptOutcome::Transient(format!("set_connected: {e}")),
        }
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
