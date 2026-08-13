use std::sync::Arc;

use ascom_alpaca::api::{CoverCalibrator, TypedDevice};
use tracing::{debug, error};

use super::alpaca::{
    build_alpaca_client, retry_connect_attempt, AttemptOutcome, GET_DEVICES_TIMEOUT,
};
use crate::config;

pub struct CoverCalibratorEntry {
    pub id: String,
    pub connected: bool,
    pub config: config::CoverCalibratorConfig,
    pub device: Option<Arc<dyn CoverCalibrator>>,
}

pub(super) async fn connect_cover_calibrator(
    config: &config::CoverCalibratorConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> CoverCalibratorEntry {
    debug!(cc_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to cover calibrator");

    let client = match build_alpaca_client(&config.alpaca_url, config.auth.as_ref(), ca_cert_path) {
        Ok(c) => c,
        Err(e) => {
            error!(cc_id = %config.id, error = %e, "failed to create Alpaca client for cover calibrator");
            return CoverCalibratorEntry {
                id: config.id.clone(),
                connected: false,
                config: config.clone(),
                device: None,
            };
        }
    };

    let label = format!("cover calibrator {}", config.id);
    let outcome = retry_connect_attempt(&label, |_attempt| async {
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

        let cc = match found_cc {
            Some(c) => c,
            None => {
                return AttemptOutcome::Permanent(format!(
                    "cover calibrator at index {} not found on Alpaca server",
                    config.device_number
                ));
            }
        };

        match cc.set_connected(true).await {
            Ok(()) => AttemptOutcome::Ok(cc),
            Err(e) => AttemptOutcome::Transient(format!("set_connected: {e}")),
        }
    })
    .await;

    match outcome {
        Ok(cc) => {
            debug!(cc_id = %config.id, "cover calibrator connected successfully");
            CoverCalibratorEntry {
                id: config.id.clone(),
                connected: true,
                config: config.clone(),
                device: Some(cc),
            }
        }
        Err(msg) => {
            error!(cc_id = %config.id, error = %msg, "failed to connect cover calibrator");
            CoverCalibratorEntry {
                id: config.id.clone(),
                connected: false,
                config: config.clone(),
                device: None,
            }
        }
    }
}
