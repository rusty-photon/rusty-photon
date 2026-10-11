use std::sync::Arc;

use ascom_alpaca::api::ObservingConditions;
use tracing::{debug, error};

use super::binding::{establish_listed, EstablishError};
use super::session::DeviceSession;
use crate::config;

pub struct ObservingConditionsEntry {
    pub id: String,
    pub config: config::ObservingConditionsConfig,
    pub session: DeviceSession<dyn ObservingConditions>,
}

impl ObservingConditionsEntry {
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.session.is_connected()
    }

    #[must_use]
    pub fn device(&self) -> Option<Arc<dyn ObservingConditions>> {
        self.session.device()
    }
}

/// Locate the configured device on its Alpaca server and switch it
/// on — the shared routine behind the startup connect and the
/// reconnect supervisor's re-establish (rp.md § Device Session
/// Recovery).
pub(super) async fn establish_observing_conditions(
    config: &config::ObservingConditionsConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> Result<Arc<dyn ObservingConditions>, EstablishError> {
    establish_listed(config, ca_cert_path).await
}

pub(super) async fn connect_observing_conditions(
    config: &config::ObservingConditionsConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> ObservingConditionsEntry {
    debug!(oc_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to observing conditions");

    match establish_observing_conditions(config, ca_cert_path).await {
        Ok(oc) => {
            debug!(oc_id = %config.id, "observing conditions connected successfully");
            ObservingConditionsEntry {
                id: config.id.clone(),
                config: config.clone(),
                session: DeviceSession::connected(oc),
            }
        }
        Err(msg) => {
            error!(oc_id = %config.id, error = %msg, "failed to connect observing conditions");
            ObservingConditionsEntry {
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

    fn oc_config_for(url: &str, device_number: u32) -> config::ObservingConditionsConfig {
        config::ObservingConditionsConfig {
            id: "ppba-weather".to_string(),
            name: None,
            alpaca_url: url.to_string(),
            device_number,
            unique_id: None,
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
        assert!(entry.is_connected(), "expected entry to be connected");
        assert!(entry.device().is_some(), "expected entry to hold a device");
        assert_eq!(entry.id, "ppba-weather");
    }

    /// `device_number` indexes among the server's `ObservingConditions`
    /// devices, so connecting to index 1 must skip past index 0.
    #[tokio::test]
    async fn connect_observing_conditions_skips_to_the_requested_index() {
        let stub = spawn_stub(two_oc_router()).await;
        let entry = connect_observing_conditions(&oc_config_for(&stub.url(), 1), None).await;
        assert!(entry.is_connected(), "expected entry to be connected");
        assert!(entry.device().is_some(), "expected entry to hold a device");
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
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }

    #[tokio::test]
    async fn connect_observing_conditions_client_build_failure_returns_disconnected_entry() {
        let entry = connect_observing_conditions(&oc_config_for("not-a-url", 0), None).await;
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
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
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
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
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }
}
