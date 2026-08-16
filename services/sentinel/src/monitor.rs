//! Monitor trait and state types

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// The state of a monitored device.
///
/// The two casings below are deliberately different and must not be unified:
/// `Display` yields the `PascalCase` variant name, which is the operator-facing
/// text (`GET /api/status`, the dashboard, the Pushover `%new_state%`
/// placeholder), while serde yields `snake_case`, which is the JSON wire form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, derive_more::Display)]
#[serde(rename_all = "snake_case")]
pub enum MonitorState {
    Safe,
    Unsafe,
    Unknown,
}

/// A change in monitor state
#[derive(Debug, Clone)]
pub struct StateChange {
    pub monitor_name: String,
    pub previous: MonitorState,
    pub current: MonitorState,
    pub timestamp: Instant,
}

/// Trait for polling a device's state
#[async_trait]
pub trait Monitor: Send + Sync + std::fmt::Debug {
    /// Get the monitor name
    fn name(&self) -> &str;

    /// Poll the current state of the device
    async fn poll(&self) -> MonitorState;

    /// Connect to the device
    async fn connect(&self) -> crate::Result<()>;

    /// Disconnect from the device
    async fn disconnect(&self) -> crate::Result<()>;

    /// The polling interval for this monitor
    fn polling_interval(&self) -> Duration;
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn display_is_pascal_case() {
        assert_eq!(MonitorState::Safe.to_string(), "Safe");
        assert_eq!(MonitorState::Unsafe.to_string(), "Unsafe");
        assert_eq!(MonitorState::Unknown.to_string(), "Unknown");
    }

    #[test]
    fn serde_form_is_snake_case_and_differs_from_display() {
        assert_eq!(
            serde_json::to_string(&MonitorState::Safe).unwrap(),
            "\"safe\""
        );
        assert_eq!(
            serde_json::to_string(&MonitorState::Unsafe).unwrap(),
            "\"unsafe\""
        );
        assert_eq!(
            serde_json::to_string(&MonitorState::Unknown).unwrap(),
            "\"unknown\""
        );
    }
}
