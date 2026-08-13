use std::sync::Arc;

use ascom_alpaca::api::{Rotator, TypedDevice};
use tracing::{debug, error};

use super::alpaca::{
    build_alpaca_client, retry_connect_attempt, AttemptOutcome, GET_DEVICES_TIMEOUT,
};
use crate::config;

pub struct RotatorEntry {
    pub id: String,
    pub connected: bool,
    pub config: config::RotatorConfig,
    pub device: Option<Arc<dyn Rotator>>,
}

pub(super) async fn connect_rotator(
    config: &config::RotatorConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> RotatorEntry {
    debug!(rotator_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to rotator");

    let client = match build_alpaca_client(&config.alpaca_url, config.auth.as_ref(), ca_cert_path) {
        Ok(c) => c,
        Err(e) => {
            error!(rotator_id = %config.id, error = %e, "failed to create Alpaca client for rotator");
            return RotatorEntry {
                id: config.id.clone(),
                connected: false,
                config: config.clone(),
                device: None,
            };
        }
    };

    let label = format!("rotator {}", config.id);
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

        let mut rot_index = 0u32;
        let mut found_rot: Option<Arc<dyn Rotator>> = None;
        for device in devices {
            if let TypedDevice::Rotator(rot) = device {
                if rot_index == config.device_number {
                    found_rot = Some(rot);
                    break;
                }
                rot_index = rot_index.saturating_add(1);
            }
        }

        let rot = match found_rot {
            Some(r) => r,
            None => {
                return AttemptOutcome::Permanent(format!(
                    "rotator at index {} not found on Alpaca server",
                    config.device_number
                ));
            }
        };

        match rot.set_connected(true).await {
            Ok(()) => AttemptOutcome::Ok(rot),
            Err(e) => AttemptOutcome::Transient(format!("set_connected: {e}")),
        }
    })
    .await;

    match outcome {
        Ok(rot) => {
            debug!(rotator_id = %config.id, "rotator connected successfully");
            RotatorEntry {
                id: config.id.clone(),
                connected: true,
                config: config.clone(),
                device: Some(rot),
            }
        }
        Err(msg) => {
            error!(rotator_id = %config.id, error = %msg, "failed to connect rotator");
            RotatorEntry {
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

    fn rotator_config_for(url: &str, device_number: u32) -> config::RotatorConfig {
        config::RotatorConfig {
            id: "falcon".to_string(),
            name: None,
            alpaca_url: url.to_string(),
            device_number,
            auth: None,
        }
    }

    /// Stub advertising two Rotators that accept `set_connected(true)` — two
    /// so a `device_number = 1` connect has to skip past index 0.
    fn two_rotator_router() -> Router {
        Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [
                            {
                                "DeviceName": "Rotator 0",
                                "DeviceType": "Rotator",
                                "DeviceNumber": 0,
                                "UniqueID": "test-rotator-uid-0"
                            },
                            {
                                "DeviceName": "Rotator 1",
                                "DeviceType": "Rotator",
                                "DeviceNumber": 1,
                                "UniqueID": "test-rotator-uid-1"
                            }
                        ],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/rotator/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/rotator/1/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
    }

    #[tokio::test]
    async fn connect_rotator_success_returns_connected_entry() {
        let stub = spawn_stub(two_rotator_router()).await;
        let entry = connect_rotator(&rotator_config_for(&stub.url(), 0), None).await;
        assert!(entry.connected, "expected entry to be connected");
        assert!(entry.device.is_some(), "expected entry to hold a device");
        assert_eq!(entry.id, "falcon");
    }

    /// `device_number` indexes among the server's Rotators, so connecting to
    /// index 1 must skip past the rotator at index 0.
    #[tokio::test]
    async fn connect_rotator_skips_to_the_requested_index() {
        let stub = spawn_stub(two_rotator_router()).await;
        let entry = connect_rotator(&rotator_config_for(&stub.url(), 1), None).await;
        assert!(entry.connected, "expected entry to be connected");
        assert!(entry.device.is_some(), "expected entry to hold a device");
    }

    /// A server that answers but has no Rotator at the requested index is a
    /// permanent failure — no retries, disconnected entry.
    #[tokio::test]
    async fn connect_rotator_not_found_returns_disconnected_entry() {
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
        let entry = connect_rotator(&rotator_config_for(&stub.url(), 0), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    #[tokio::test]
    async fn connect_rotator_client_build_failure_returns_disconnected_entry() {
        let entry = connect_rotator(&rotator_config_for("not-a-url", 0), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    /// A failed `get_devices` maps to `Transient`, so the loop exhausts all
    /// attempts and returns a disconnected entry. `start_paused` collapses
    /// the retry backoff.
    #[tokio::test(start_paused = true)]
    async fn connect_rotator_get_devices_error_returns_disconnected_entry() {
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
        let entry = connect_rotator(&rotator_config_for(&stub.url(), 0), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    /// A `set_connected` error maps to `Transient`: the rotator is found,
    /// but turning it on fails on every attempt. `start_paused` collapses
    /// the retry backoff.
    #[tokio::test(start_paused = true)]
    async fn connect_rotator_set_connected_error_returns_disconnected_entry() {
        let app = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [{
                            "DeviceName": "Rotator 0",
                            "DeviceType": "Rotator",
                            "DeviceNumber": 0,
                            "UniqueID": "test-rotator-uid"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/rotator/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 1025,
                        "ErrorMessage": "simulated set_connected failure"
                    }))
                }),
            );
        let stub = spawn_stub(app).await;
        let entry = connect_rotator(&rotator_config_for(&stub.url(), 0), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }
}
