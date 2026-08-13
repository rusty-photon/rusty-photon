use std::sync::Arc;

use ascom_alpaca::api::{ObservingConditions, TypedDevice};
use tracing::{debug, error};

use super::alpaca::{
    build_alpaca_client, retry_connect_attempt, AttemptOutcome, GET_DEVICES_TIMEOUT,
};
use crate::config;

pub struct ObservingConditionsEntry {
    pub id: String,
    pub connected: bool,
    pub config: config::ObservingConditionsConfig,
    pub device: Option<Arc<dyn ObservingConditions>>,
}

pub(super) async fn connect_observing_conditions(
    config: &config::ObservingConditionsConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> ObservingConditionsEntry {
    debug!(oc_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to observing conditions");

    let client = match build_alpaca_client(&config.alpaca_url, config.auth.as_ref(), ca_cert_path) {
        Ok(c) => c,
        Err(e) => {
            error!(oc_id = %config.id, error = %e, "failed to create Alpaca client for observing conditions");
            return ObservingConditionsEntry {
                id: config.id.clone(),
                connected: false,
                config: config.clone(),
                device: None,
            };
        }
    };

    let label = format!("observing conditions {}", config.id);
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

        let mut oc_index = 0u32;
        let mut found_oc: Option<Arc<dyn ObservingConditions>> = None;
        for device in devices {
            if let TypedDevice::ObservingConditions(oc) = device {
                if oc_index == config.device_number {
                    found_oc = Some(oc);
                    break;
                }
                oc_index = oc_index.saturating_add(1);
            }
        }

        let oc = match found_oc {
            Some(o) => o,
            None => {
                return AttemptOutcome::Permanent(format!(
                    "observing conditions at index {} not found on Alpaca server",
                    config.device_number
                ));
            }
        };

        match oc.set_connected(true).await {
            Ok(()) => AttemptOutcome::Ok(oc),
            Err(e) => AttemptOutcome::Transient(format!("set_connected: {e}")),
        }
    })
    .await;

    match outcome {
        Ok(oc) => {
            debug!(oc_id = %config.id, "observing conditions connected successfully");
            ObservingConditionsEntry {
                id: config.id.clone(),
                connected: true,
                config: config.clone(),
                device: Some(oc),
            }
        }
        Err(msg) => {
            error!(oc_id = %config.id, error = %msg, "failed to connect observing conditions");
            ObservingConditionsEntry {
                id: config.id.clone(),
                connected: false,
                config: config.clone(),
                device: None,
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

    fn oc_config_for(url: &str, device_number: u32) -> config::ObservingConditionsConfig {
        config::ObservingConditionsConfig {
            id: "ppba-weather".to_string(),
            name: None,
            alpaca_url: url.to_string(),
            device_number,
            auth: None,
        }
    }

    /// Stub advertising two `ObservingConditions` devices that accept
    /// `set_connected(true)` — two so a `device_number = 1` connect has to
    /// skip past index 0.
    fn two_oc_router() -> Router {
        Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [
                            {
                                "DeviceName": "ObservingConditions 0",
                                "DeviceType": "ObservingConditions",
                                "DeviceNumber": 0,
                                "UniqueID": "test-oc-uid-0"
                            },
                            {
                                "DeviceName": "ObservingConditions 1",
                                "DeviceType": "ObservingConditions",
                                "DeviceNumber": 1,
                                "UniqueID": "test-oc-uid-1"
                            }
                        ],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/observingconditions/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/observingconditions/1/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
    }

    #[tokio::test]
    async fn connect_observing_conditions_success_returns_connected_entry() {
        let stub = spawn_stub(two_oc_router()).await;
        let entry = connect_observing_conditions(&oc_config_for(&stub.url(), 0), None).await;
        assert!(entry.connected, "expected entry to be connected");
        assert!(entry.device.is_some(), "expected entry to hold a device");
        assert_eq!(entry.id, "ppba-weather");
    }

    /// `device_number` indexes among the server's `ObservingConditions`
    /// devices, so connecting to index 1 must skip past index 0.
    #[tokio::test]
    async fn connect_observing_conditions_skips_to_the_requested_index() {
        let stub = spawn_stub(two_oc_router()).await;
        let entry = connect_observing_conditions(&oc_config_for(&stub.url(), 1), None).await;
        assert!(entry.connected, "expected entry to be connected");
        assert!(entry.device.is_some(), "expected entry to hold a device");
    }

    /// A server that answers but has no `ObservingConditions` at the
    /// requested index is a permanent failure — no retries, disconnected
    /// entry.
    #[tokio::test]
    async fn connect_observing_conditions_not_found_returns_disconnected_entry() {
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
        let entry = connect_observing_conditions(&oc_config_for(&stub.url(), 0), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    #[tokio::test]
    async fn connect_observing_conditions_client_build_failure_returns_disconnected_entry() {
        let entry = connect_observing_conditions(&oc_config_for("not-a-url", 0), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    /// A failed `get_devices` maps to `Transient`, so the loop exhausts all
    /// attempts and returns a disconnected entry. `start_paused` collapses
    /// the retry backoff.
    #[tokio::test(start_paused = true)]
    async fn connect_observing_conditions_get_devices_error_returns_disconnected_entry() {
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
        let entry = connect_observing_conditions(&oc_config_for(&stub.url(), 0), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    /// A `set_connected` error maps to `Transient`: the device is found,
    /// but turning it on fails on every attempt. `start_paused` collapses
    /// the retry backoff.
    #[tokio::test(start_paused = true)]
    async fn connect_observing_conditions_set_connected_error_returns_disconnected_entry() {
        let app = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [{
                            "DeviceName": "ObservingConditions 0",
                            "DeviceType": "ObservingConditions",
                            "DeviceNumber": 0,
                            "UniqueID": "test-oc-uid"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/observingconditions/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 1025,
                        "ErrorMessage": "simulated set_connected failure"
                    }))
                }),
            );
        let stub = spawn_stub(app).await;
        let entry = connect_observing_conditions(&oc_config_for(&stub.url(), 0), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }
}
