use std::sync::Arc;

use ascom_alpaca::api::FilterWheel;
use tracing::{debug, error};

use super::binding::{establish_listed, reported_name, EstablishError};
use super::session::DeviceSession;
use crate::config;

/// Facts read once when a filter wheel session is established (rp.md §
/// Core Fields, `filter_wheel_name`), served from the entry afterwards
/// and replaced on every re-establish.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilterWheelInvariants {
    /// The driver's ASCOM `Name`. `None` when the read failed or the
    /// name is blank.
    pub name: Option<String>,
}

pub struct FilterWheelEntry {
    pub id: String,
    pub config: config::FilterWheelConfig,
    pub session: DeviceSession<dyn FilterWheel, FilterWheelInvariants>,

    /// One filter change through the wheel at a time (rp.md § Filter
    /// Wheel Tool Details), and a train lease's hold on it (§ Train
    /// Lease).
    pub slot: super::DeviceSlot,
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

/// Locate the configured filter wheel on its Alpaca server, switch it
/// on, and read its name — the shared routine behind the startup
/// connect and the reconnect supervisor's re-establish (rp.md § Device
/// Session Recovery). A property read — nothing moves.
pub(super) async fn establish_filter_wheel(
    config: &config::FilterWheelConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> Result<(Arc<dyn FilterWheel>, FilterWheelInvariants), EstablishError> {
    let fw = establish_listed(config, ca_cert_path).await?;
    let invariants = FilterWheelInvariants {
        name: reported_name(&config.id, fw.name().await),
    };
    Ok((fw, invariants))
}

pub(super) async fn connect_filter_wheel(
    config: &config::FilterWheelConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> FilterWheelEntry {
    debug!(fw_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to filter wheel");

    match establish_filter_wheel(config, ca_cert_path).await {
        Ok((fw, invariants)) => {
            debug!(fw_id = %config.id, name = ?invariants.name, "filter wheel connected successfully; cached its name");
            FilterWheelEntry {
                id: config.id.clone(),
                config: config.clone(),
                session: DeviceSession::connected_with(fw, invariants),
                slot: crate::equipment::DeviceSlot::default(),
            }
        }
        Err(msg) => {
            error!(fw_id = %config.id, error = %msg, "failed to connect filter wheel");
            FilterWheelEntry {
                id: config.id.clone(),
                config: config.clone(),
                session: DeviceSession::disconnected(),
                slot: crate::equipment::DeviceSlot::default(),
            }
        }
    }
}
