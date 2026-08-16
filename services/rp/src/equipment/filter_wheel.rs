use std::sync::Arc;

use ascom_alpaca::api::{FilterWheel, TypedDevice};
use tracing::{debug, error};

use super::alpaca::{
    build_alpaca_client, retry_connect_attempt, AttemptOutcome, GET_DEVICES_TIMEOUT,
};
use crate::config;

pub struct FilterWheelEntry {
    pub id: String,
    pub connected: bool,
    pub config: config::FilterWheelConfig,
    pub device: Option<Arc<dyn FilterWheel>>,
}

pub(super) async fn connect_filter_wheel(
    config: &config::FilterWheelConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> FilterWheelEntry {
    debug!(fw_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to filter wheel");

    let client = match build_alpaca_client(&config.alpaca_url, config.auth.as_ref(), ca_cert_path) {
        Ok(c) => c,
        Err(e) => {
            error!(fw_id = %config.id, error = %e, "failed to create Alpaca client for filter wheel");
            return FilterWheelEntry {
                id: config.id.clone(),
                connected: false,
                config: config.clone(),
                device: None,
            };
        }
    };

    let label = format!("filter wheel {}", config.id);
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

        let mut fw_index = 0u32;
        let mut found_fw: Option<Arc<dyn FilterWheel>> = None;
        for device in devices {
            if let TypedDevice::FilterWheel(fw) = device {
                if fw_index == config.device_number {
                    found_fw = Some(fw);
                    break;
                }
                fw_index = fw_index.saturating_add(1);
            }
        }

        let Some(fw) = found_fw else {
            return AttemptOutcome::Permanent(format!(
                "filter wheel at index {} not found on Alpaca server",
                config.device_number
            ));
        };

        match fw.set_connected(true).await {
            Ok(()) => AttemptOutcome::Ok(fw),
            Err(e) => AttemptOutcome::Transient(format!("set_connected: {e}")),
        }
    })
    .await;

    match outcome {
        Ok(fw) => {
            debug!(fw_id = %config.id, "filter wheel connected successfully");
            FilterWheelEntry {
                id: config.id.clone(),
                connected: true,
                config: config.clone(),
                device: Some(fw),
            }
        }
        Err(msg) => {
            error!(fw_id = %config.id, error = %msg, "failed to connect filter wheel");
            FilterWheelEntry {
                id: config.id.clone(),
                connected: false,
                config: config.clone(),
                device: None,
            }
        }
    }
}
