use std::sync::Arc;

use ascom_alpaca::api::{FilterWheel, TypedDevice};
use tracing::{debug, error};

use super::binding::{establish_listed, RosterAddress};
use super::session::DeviceSession;
use crate::config;

pub struct FilterWheelEntry {
    pub id: String,
    pub config: config::FilterWheelConfig,
    pub session: DeviceSession<dyn FilterWheel>,
}

impl FilterWheelEntry {
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.session.is_connected()
    }

    #[must_use]
    pub fn device(&self) -> Option<Arc<dyn FilterWheel>> {
        self.session.device()
    }
}

/// Locate the configured filter wheel on its Alpaca server and switch
/// it on — the shared routine behind the startup connect and the
/// reconnect supervisor's re-establish (rp.md § Device Session
/// Recovery).
pub(super) async fn establish_filter_wheel(
    config: &config::FilterWheelConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> Result<Arc<dyn FilterWheel>, String> {
    let address = RosterAddress {
        kind: "filter wheel",
        id: Some(&config.id),
        alpaca_url: &config.alpaca_url,
        device_number: config.device_number,
        unique_id: config.unique_id.as_ref(),
        auth: config.auth.as_ref(),
    };
    establish_listed(&address, ca_cert_path, |device| match device {
        TypedDevice::FilterWheel(device) => Some(device),
        _ => None,
    })
    .await
}

pub(super) async fn connect_filter_wheel(
    config: &config::FilterWheelConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> FilterWheelEntry {
    debug!(fw_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to filter wheel");

    match establish_filter_wheel(config, ca_cert_path).await {
        Ok(fw) => {
            debug!(fw_id = %config.id, "filter wheel connected successfully");
            FilterWheelEntry {
                id: config.id.clone(),
                config: config.clone(),
                session: DeviceSession::connected(fw),
            }
        }
        Err(msg) => {
            error!(fw_id = %config.id, error = %msg, "failed to connect filter wheel");
            FilterWheelEntry {
                id: config.id.clone(),
                config: config.clone(),
                session: DeviceSession::disconnected(),
            }
        }
    }
}
