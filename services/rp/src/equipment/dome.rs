use std::sync::Arc;

use ascom_alpaca::api::{Dome, TypedDevice};
use tracing::{debug, error};

use super::alpaca::{
    build_alpaca_client, retry_connect_attempt, AttemptOutcome, GET_DEVICES_TIMEOUT,
};
use super::session::DeviceSession;
use crate::config;

pub struct DomeEntry {
    pub id: String,
    pub config: config::DomeConfig,
    pub session: DeviceSession<dyn Dome>,
}

impl DomeEntry {
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.session.is_connected()
    }

    #[must_use]
    pub fn device(&self) -> Option<Arc<dyn Dome>> {
        self.session.device()
    }
}

/// Locate the configured device on its Alpaca server and switch it
/// on — the shared routine behind the startup connect and the
/// reconnect supervisor's re-establish (rp.md § Device Session
/// Recovery).
pub(super) async fn establish_dome(
    config: &config::DomeConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> Result<Arc<dyn Dome>, String> {
    let client = build_alpaca_client(&config.alpaca_url, config.auth.as_ref(), ca_cert_path)
        .map_err(|e| format!("failed to create Alpaca client: {e}"))?;

    let label = format!("dome {}", config.id);
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

        let mut dome_index = 0u32;
        let mut found_dome: Option<Arc<dyn Dome>> = None;
        for device in devices {
            if let TypedDevice::Dome(dome) = device {
                if dome_index == config.device_number {
                    found_dome = Some(dome);
                    break;
                }
                dome_index = dome_index.saturating_add(1);
            }
        }

        let Some(dome) = found_dome else {
            return AttemptOutcome::Permanent(format!(
                "dome at index {} not found on Alpaca server",
                config.device_number
            ));
        };

        match dome.set_connected(true).await {
            Ok(()) => AttemptOutcome::Ok(dome),
            Err(e) => AttemptOutcome::Transient(format!("set_connected: {e}")),
        }
    })
    .await
}

pub(super) async fn connect_dome(
    config: &config::DomeConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> DomeEntry {
    debug!(dome_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to dome");

    match establish_dome(config, ca_cert_path).await {
        Ok(dome) => {
            debug!(dome_id = %config.id, "dome connected successfully");
            DomeEntry {
                id: config.id.clone(),
                config: config.clone(),
                session: DeviceSession::connected(dome),
            }
        }
        Err(msg) => {
            error!(dome_id = %config.id, error = %msg, "failed to connect dome");
            DomeEntry {
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

    fn dome_config_for(url: &str, device_number: u32) -> config::DomeConfig {
        config::DomeConfig {
            id: "roll-off".to_string(),
            name: None,
            alpaca_url: url.to_string(),
            device_number,
            auth: None,
        }
    }

    /// Stub advertising two Domes that accept `set_connected(true)` — two so
    /// a `device_number = 1` connect has to skip past index 0.
    fn two_dome_router() -> Router {
        Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [
                            {
                                "DeviceName": "Dome 0",
                                "DeviceType": "Dome",
                                "DeviceNumber": 0,
                                "UniqueID": "test-dome-uid-0"
                            },
                            {
                                "DeviceName": "Dome 1",
                                "DeviceType": "Dome",
                                "DeviceNumber": 1,
                                "UniqueID": "test-dome-uid-1"
                            }
                        ],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/dome/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/dome/1/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
    }

    #[tokio::test]
    async fn connect_dome_success_returns_connected_entry() {
        let stub = spawn_stub(two_dome_router()).await;
        let entry = connect_dome(&dome_config_for(&stub.url(), 0), None).await;
        assert!(entry.is_connected(), "expected entry to be connected");
        assert!(entry.device().is_some(), "expected entry to hold a device");
        assert_eq!(entry.id, "roll-off");
    }

    /// `device_number` indexes among the server's Domes, so connecting to
    /// index 1 must skip past the dome at index 0.
    #[tokio::test]
    async fn connect_dome_skips_to_the_requested_index() {
        let stub = spawn_stub(two_dome_router()).await;
        let entry = connect_dome(&dome_config_for(&stub.url(), 1), None).await;
        assert!(entry.is_connected(), "expected entry to be connected");
        assert!(entry.device().is_some(), "expected entry to hold a device");
    }

    /// A server that answers but has no Dome at the requested index is a
    /// permanent failure — no retries, disconnected entry.
    #[tokio::test]
    async fn connect_dome_not_found_returns_disconnected_entry() {
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
        let entry = connect_dome(&dome_config_for(&stub.url(), 0), None).await;
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }

    #[tokio::test]
    async fn connect_dome_client_build_failure_returns_disconnected_entry() {
        let entry = connect_dome(&dome_config_for("not-a-url", 0), None).await;
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }

    /// A failed `get_devices` maps to `Transient`, so the loop exhausts all
    /// attempts and returns a disconnected entry. `start_paused` collapses
    /// the retry backoff.
    #[tokio::test(start_paused = true)]
    async fn connect_dome_get_devices_error_returns_disconnected_entry() {
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
        let entry = connect_dome(&dome_config_for(&stub.url(), 0), None).await;
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }

    /// A `set_connected` error maps to `Transient`: the dome is found, but
    /// turning it on fails on every attempt. `start_paused` collapses the
    /// retry backoff.
    #[tokio::test(start_paused = true)]
    async fn connect_dome_set_connected_error_returns_disconnected_entry() {
        let app = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [{
                            "DeviceName": "Dome 0",
                            "DeviceType": "Dome",
                            "DeviceNumber": 0,
                            "UniqueID": "test-dome-uid"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/dome/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 1025,
                        "ErrorMessage": "simulated set_connected failure"
                    }))
                }),
            );
        let stub = spawn_stub(app).await;
        let entry = connect_dome(&dome_config_for(&stub.url(), 0), None).await;
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }
}
