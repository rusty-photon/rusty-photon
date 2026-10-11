use std::sync::Arc;

use ascom_alpaca::api::SafetyMonitor;
use tracing::{debug, error};

use super::binding::{establish_listed, EstablishError};
use super::session::DeviceSession;
use crate::config;

pub struct SafetyMonitorEntry {
    pub id: String,
    pub config: config::SafetyMonitorConfig,
    pub session: DeviceSession<dyn SafetyMonitor>,
}

impl SafetyMonitorEntry {
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.session.is_connected()
    }

    #[must_use]
    pub fn device(&self) -> Option<Arc<dyn SafetyMonitor>> {
        self.session.device()
    }
}

/// Locate the configured monitor on its Alpaca server and switch it on —
/// the shared routine behind the startup connect and the reconnect
/// supervisor's re-establish (rp.md § Device Session Recovery).
pub(super) async fn establish_safety_monitor(
    config: &config::SafetyMonitorConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> Result<Arc<dyn SafetyMonitor>, EstablishError> {
    establish_listed(config, ca_cert_path).await
}

pub(super) async fn connect_safety_monitor(
    config: &config::SafetyMonitorConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> SafetyMonitorEntry {
    debug!(sm_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to safety monitor");

    match establish_safety_monitor(config, ca_cert_path).await {
        Ok(sm) => {
            debug!(sm_id = %config.id, "safety monitor connected successfully");
            SafetyMonitorEntry {
                id: config.id.clone(),
                config: config.clone(),
                session: DeviceSession::connected(sm),
            }
        }
        Err(msg) => {
            // A safety monitor that cannot be read counts as unsafe
            // (fail-unsafe, rp.md § Safety) — so a failed connect here
            // gates the session until the reconnect supervisor
            // re-establishes the device.
            error!(sm_id = %config.id, error = %msg, "failed to connect safety monitor");
            SafetyMonitorEntry {
                id: config.id.clone(),
                config: config.clone(),
                session: DeviceSession::disconnected(),
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config;
    use crate::equipment::test_support::spawn_stub;

    use axum::{
        routing::{get, put},
        Json, Router,
    };

    fn sm_config_for(url: &str, device_number: u32) -> config::SafetyMonitorConfig {
        config::SafetyMonitorConfig {
            id: "weather-watcher".to_string(),
            alpaca_url: url.to_string(),
            device_number,
            unique_id: None,
            auth: None,
        }
    }

    /// Stub advertising two `SafetyMonitors` that accept
    /// `set_connected(true)` — two so a `device_number = 1` connect has
    /// to skip past index 0.
    fn two_monitor_router() -> Router {
        Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [
                            {
                                "DeviceName": "Safety Monitor 0",
                                "DeviceType": "SafetyMonitor",
                                "DeviceNumber": 0,
                                "UniqueID": "test-sm-uid-0"
                            },
                            {
                                "DeviceName": "Safety Monitor 1",
                                "DeviceType": "SafetyMonitor",
                                "DeviceNumber": 1,
                                "UniqueID": "test-sm-uid-1"
                            }
                        ],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/safetymonitor/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/safetymonitor/1/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
    }

    #[tokio::test]
    async fn connect_safety_monitor_success_returns_connected_entry() {
        let stub = spawn_stub(two_monitor_router()).await;
        let entry = connect_safety_monitor(&sm_config_for(&stub.url(), 0), None).await;
        assert!(entry.is_connected(), "expected entry to be connected");
        assert!(entry.device().is_some(), "expected entry to hold a device");
        assert_eq!(entry.id, "weather-watcher");
    }

    /// `device_number` indexes among the server's `SafetyMonitors`, so
    /// connecting to index 1 must skip past the monitor at index 0.
    #[tokio::test]
    async fn connect_safety_monitor_skips_to_the_requested_index() {
        let stub = spawn_stub(two_monitor_router()).await;
        let entry = connect_safety_monitor(&sm_config_for(&stub.url(), 1), None).await;
        assert!(entry.is_connected(), "expected entry to be connected");
        assert!(entry.device().is_some(), "expected entry to hold a device");
    }

    /// A server that answers but has no `SafetyMonitor` at the requested
    /// index is a permanent failure — no retries, disconnected entry.
    /// The entry then reads as unsafe (fail-unsafe posture).
    #[tokio::test]
    async fn connect_safety_monitor_not_found_returns_disconnected_entry() {
        let app = Router::new().route(
            "/management/v1/configureddevices",
            get(|| async {
                Json(serde_json::json!({
                    "Value": [],
                    "ErrorNumber": 0,
                    "ErrorMessage": ""
                }))
            }),
        );
        let stub = spawn_stub(app).await;
        let entry = connect_safety_monitor(&sm_config_for(&stub.url(), 0), None).await;
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }

    #[tokio::test]
    async fn connect_safety_monitor_client_build_failure_returns_disconnected_entry() {
        let entry = connect_safety_monitor(&sm_config_for("not-a-url", 0), None).await;
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }

    /// A failed `get_devices` maps to `Transient`, so the loop exhausts
    /// all attempts and returns a disconnected (→ fail-unsafe) entry.
    /// `start_paused` collapses the retry backoff.
    #[tokio::test(start_paused = true)]
    async fn connect_safety_monitor_get_devices_error_returns_disconnected_entry() {
        let app = Router::new().route(
            "/management/v1/configureddevices",
            get(|| async {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "simulated get_devices failure",
                )
            }),
        );
        let stub = spawn_stub(app).await;
        let entry = connect_safety_monitor(&sm_config_for(&stub.url(), 0), None).await;
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }

    /// A `set_connected` error maps to `Transient`: the monitor is found,
    /// but turning it on fails on every attempt. `start_paused` collapses
    /// the retry backoff.
    #[tokio::test(start_paused = true)]
    async fn connect_safety_monitor_set_connected_error_returns_disconnected_entry() {
        let app = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [{
                            "DeviceName": "Safety Monitor 0",
                            "DeviceType": "SafetyMonitor",
                            "DeviceNumber": 0,
                            "UniqueID": "test-sm-uid"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/safetymonitor/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 1025,
                        "ErrorMessage": "simulated set_connected failure"
                    }))
                }),
            );
        let stub = spawn_stub(app).await;
        let entry = connect_safety_monitor(&sm_config_for(&stub.url(), 0), None).await;
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }
}
