use std::sync::Arc;

use ascom_alpaca::api::{Switch, TypedDevice};
use tracing::{debug, error};

use super::alpaca::{
    build_alpaca_client, retry_connect_attempt, AttemptOutcome, GET_DEVICES_TIMEOUT,
};
use crate::config;

pub struct SwitchEntry {
    pub id: String,
    pub connected: bool,
    pub config: config::SwitchConfig,
    pub device: Option<Arc<dyn Switch>>,
}

pub(super) async fn connect_switch(
    config: &config::SwitchConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> SwitchEntry {
    debug!(switch_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to switch");

    let client = match build_alpaca_client(&config.alpaca_url, config.auth.as_ref(), ca_cert_path) {
        Ok(c) => c,
        Err(e) => {
            error!(switch_id = %config.id, error = %e, "failed to create Alpaca client for switch");
            return SwitchEntry {
                id: config.id.clone(),
                connected: false,
                config: config.clone(),
                device: None,
            };
        }
    };

    let label = format!("switch {}", config.id);
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

        let mut sw_index = 0u32;
        let mut found_sw: Option<Arc<dyn Switch>> = None;
        for device in devices {
            if let TypedDevice::Switch(sw) = device {
                if sw_index == config.device_number {
                    found_sw = Some(sw);
                    break;
                }
                sw_index = sw_index.saturating_add(1);
            }
        }

        let sw = match found_sw {
            Some(s) => s,
            None => {
                return AttemptOutcome::Permanent(format!(
                    "switch at index {} not found on Alpaca server",
                    config.device_number
                ));
            }
        };

        match sw.set_connected(true).await {
            Ok(()) => AttemptOutcome::Ok(sw),
            Err(e) => AttemptOutcome::Transient(format!("set_connected: {e}")),
        }
    })
    .await;

    match outcome {
        Ok(sw) => {
            debug!(switch_id = %config.id, "switch connected successfully");
            SwitchEntry {
                id: config.id.clone(),
                connected: true,
                config: config.clone(),
                device: Some(sw),
            }
        }
        Err(msg) => {
            error!(switch_id = %config.id, error = %msg, "failed to connect switch");
            SwitchEntry {
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

    fn switch_config_for(url: &str, device_number: u32) -> config::SwitchConfig {
        config::SwitchConfig {
            id: "ppba".to_string(),
            name: None,
            alpaca_url: url.to_string(),
            device_number,
            auth: None,
        }
    }

    /// Stub advertising two Switches that accept `set_connected(true)` — two
    /// so a `device_number = 1` connect has to skip past index 0.
    fn two_switch_router() -> Router {
        Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [
                            {
                                "DeviceName": "Switch 0",
                                "DeviceType": "Switch",
                                "DeviceNumber": 0,
                                "UniqueID": "test-switch-uid-0"
                            },
                            {
                                "DeviceName": "Switch 1",
                                "DeviceType": "Switch",
                                "DeviceNumber": 1,
                                "UniqueID": "test-switch-uid-1"
                            }
                        ],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/switch/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/switch/1/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
    }

    #[tokio::test]
    async fn connect_switch_success_returns_connected_entry() {
        let stub = spawn_stub(two_switch_router()).await;
        let entry = connect_switch(&switch_config_for(&stub.url(), 0), None).await;
        assert!(entry.connected, "expected entry to be connected");
        assert!(entry.device.is_some(), "expected entry to hold a device");
        assert_eq!(entry.id, "ppba");
    }

    /// `device_number` indexes among the server's Switches, so connecting to
    /// index 1 must skip past the switch at index 0.
    #[tokio::test]
    async fn connect_switch_skips_to_the_requested_index() {
        let stub = spawn_stub(two_switch_router()).await;
        let entry = connect_switch(&switch_config_for(&stub.url(), 1), None).await;
        assert!(entry.connected, "expected entry to be connected");
        assert!(entry.device.is_some(), "expected entry to hold a device");
    }

    /// A server that answers but has no Switch at the requested index is a
    /// permanent failure — no retries, disconnected entry.
    #[tokio::test]
    async fn connect_switch_not_found_returns_disconnected_entry() {
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
        let entry = connect_switch(&switch_config_for(&stub.url(), 0), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    #[tokio::test]
    async fn connect_switch_client_build_failure_returns_disconnected_entry() {
        let entry = connect_switch(&switch_config_for("not-a-url", 0), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    /// A failed `get_devices` maps to `Transient`, so the loop exhausts all
    /// attempts and returns a disconnected entry. `start_paused` collapses
    /// the retry backoff.
    #[tokio::test(start_paused = true)]
    async fn connect_switch_get_devices_error_returns_disconnected_entry() {
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
        let entry = connect_switch(&switch_config_for(&stub.url(), 0), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    /// A `set_connected` error maps to `Transient`: the switch is found, but
    /// turning it on fails on every attempt. `start_paused` collapses the
    /// retry backoff.
    #[tokio::test(start_paused = true)]
    async fn connect_switch_set_connected_error_returns_disconnected_entry() {
        let app = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [{
                            "DeviceName": "Switch 0",
                            "DeviceType": "Switch",
                            "DeviceNumber": 0,
                            "UniqueID": "test-switch-uid"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/switch/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 1025,
                        "ErrorMessage": "simulated set_connected failure"
                    }))
                }),
            );
        let stub = spawn_stub(app).await;
        let entry = connect_switch(&switch_config_for(&stub.url(), 0), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }
}
