use std::sync::Arc;

use ascom_alpaca::api::{Focuser, TypedDevice};
use tracing::{debug, error};

use super::alpaca::{
    build_alpaca_client, retry_connect_attempt, AttemptOutcome, GET_DEVICES_TIMEOUT,
};
use crate::config;

pub struct FocuserEntry {
    pub id: String,
    pub connected: bool,
    pub config: config::FocuserConfig,
    pub device: Option<Arc<dyn Focuser>>,
}

pub(super) async fn connect_focuser(
    config: &config::FocuserConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> FocuserEntry {
    debug!(focuser_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to focuser");

    let client = match build_alpaca_client(&config.alpaca_url, config.auth.as_ref(), ca_cert_path) {
        Ok(c) => c,
        Err(e) => {
            error!(focuser_id = %config.id, error = %e, "failed to create Alpaca client for focuser");
            return FocuserEntry {
                id: config.id.clone(),
                connected: false,
                config: config.clone(),
                device: None,
            };
        }
    };

    let label = format!("focuser {}", config.id);
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

        let mut focuser_index = 0u32;
        let mut found_focuser: Option<Arc<dyn Focuser>> = None;
        for device in devices {
            if let TypedDevice::Focuser(foc) = device {
                if focuser_index == config.device_number {
                    found_focuser = Some(foc);
                    break;
                }
                focuser_index = focuser_index.saturating_add(1);
            }
        }

        let foc = match found_focuser {
            Some(f) => f,
            None => {
                return AttemptOutcome::Permanent(format!(
                    "focuser at index {} not found on Alpaca server",
                    config.device_number
                ));
            }
        };

        match foc.set_connected(true).await {
            Ok(()) => AttemptOutcome::Ok(foc),
            Err(e) => AttemptOutcome::Transient(format!("set_connected: {e}")),
        }
    })
    .await;

    match outcome {
        Ok(foc) => {
            debug!(focuser_id = %config.id, "focuser connected successfully");
            FocuserEntry {
                id: config.id.clone(),
                connected: true,
                config: config.clone(),
                device: Some(foc),
            }
        }
        Err(msg) => {
            error!(focuser_id = %config.id, error = %msg, "failed to connect focuser");
            FocuserEntry {
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
    use crate::equipment::EquipmentRegistry;

    use axum::{
        routing::{get, put},
        Json, Router,
    };

    fn focuser_config_for(url: &str) -> config::FocuserConfig {
        config::FocuserConfig {
            id: "main-focuser".to_string(),
            alpaca_url: url.to_string(),
            device_number: 0,
            min_position: None,
            max_position: None,
            steps_per_sec: Default::default(),
            auth: None,
        }
    }

    /// `connect_focuser` swallows every failure mode into a disconnected
    /// `FocuserEntry`; the registry never refuses to start because a focuser
    /// went missing. The two unit tests below pin two of the failure paths
    /// directly, complementing the single failure path exercised by the
    /// "rp is running with a focuser at \"<http://localhost:1>\" device 0"
    /// BDD scenario.
    #[tokio::test]
    async fn connect_focuser_invalid_url_returns_disconnected_entry() {
        let cfg = config::FocuserConfig {
            id: "main-focuser".to_string(),
            alpaca_url: "not-a-url".to_string(),
            device_number: 0,
            min_position: None,
            max_position: None,
            steps_per_sec: Default::default(),
            auth: None,
        };
        let entry = connect_focuser(&cfg, None).await;
        assert_eq!(entry.id, "main-focuser");
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    #[tokio::test]
    async fn connect_focuser_unreachable_returns_disconnected_entry() {
        // 127.0.0.1:1 is reserved and refuses connections — `get_devices`
        // returns an error inside the 5s timeout window, exercising the
        // `Ok(Err(e))` arm of `connect_focuser`'s match.
        let cfg = config::FocuserConfig {
            id: "main-focuser".to_string(),
            alpaca_url: "http://127.0.0.1:1".to_string(),
            device_number: 0,
            min_position: None,
            max_position: None,
            steps_per_sec: Default::default(),
            auth: None,
        };
        let entry = connect_focuser(&cfg, None).await;
        assert_eq!(entry.id, "main-focuser");
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    /// `Value: []` — `get_devices` succeeds but yields no Focuser, so the
    /// device-search loop falls into the `None` arm and `connect_focuser`
    /// returns a disconnected entry without ever calling `set_connected`.
    ///
    /// Transaction IDs are deliberately omitted from the response. The
    /// upstream client deserializes them as `Option<NonZeroU32>`; emitting
    /// the literal `0` would fail `NonZeroU32` parsing and short-circuit
    /// the request as a network error rather than letting the device-list
    /// branch fire.
    #[tokio::test]
    async fn connect_focuser_no_focuser_in_devices_returns_disconnected_entry() {
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
        let entry = connect_focuser(&focuser_config_for(&stub.url()), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    /// Server returns a Focuser at device 0, but the `set_connected` PUT
    /// responds with `ErrorNumber != 0`, exercising the final match arm in
    /// `connect_focuser` (the Alpaca-level rejection of `Connected=true`).
    #[tokio::test]
    async fn connect_focuser_set_connected_fails_returns_disconnected_entry() {
        let app = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [{
                            "DeviceName": "Focuser 0",
                            "DeviceType": "Focuser",
                            "DeviceNumber": 0,
                            "UniqueID": "test-focuser-uid"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/focuser/0/connected",
                put(|| async {
                    // Alpaca convention: action-level failure is signalled
                    // by a non-zero ErrorNumber + ErrorMessage in the body,
                    // not by an HTTP status code. ASCOMErrorCode is in
                    // 0x400..=0xFFF, so 1024 (0x400) is the smallest valid
                    // non-OK value.
                    Json(serde_json::json!({
                        "ErrorNumber": 1024,
                        "ErrorMessage": "simulated set_connected failure"
                    }))
                }),
            );
        let stub = spawn_stub(app).await;
        let entry = connect_focuser(&focuser_config_for(&stub.url()), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    /// Handler hangs forever; the 5 s `tokio::time::timeout` wrapping
    /// `client.get_devices()` fires and `connect_focuser` falls into the
    /// `Err(_)` (timeout) arm. `start_paused = true` lets tokio
    /// auto-advance virtual time once every other future is pending, so
    /// the test completes in real-time milliseconds rather than waiting 5
    /// wallclock seconds.
    #[tokio::test(start_paused = true)]
    async fn connect_focuser_timeout_returns_disconnected_entry() {
        let app = Router::new().route(
            "/management/v1/configureddevices",
            get(|| async { std::future::pending::<Json<serde_json::Value>>().await }),
        );
        let stub = spawn_stub(app).await;
        let entry = connect_focuser(&focuser_config_for(&stub.url()), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    /// Build a router that successfully advertises one focuser at index 0
    /// and accepts `set_connected(true)`. Shared by the success-path
    /// `connect_focuser` test and the `EquipmentRegistry` end-to-end test.
    fn ok_focuser_router() -> Router {
        Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [{
                            "DeviceName": "Focuser 0",
                            "DeviceType": "Focuser",
                            "DeviceNumber": 0,
                            "UniqueID": "test-focuser-uid"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/focuser/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
    }

    /// Server advertises a focuser at index 0 and accepts `set_connected`,
    /// exercising the `Ok(())` arm of `connect_focuser` plus the device
    /// iteration `Some(_)` match arm — the success path that doesn't run
    /// in any of the failure-branch tests above.
    #[tokio::test]
    async fn connect_focuser_success_returns_connected_entry() {
        let stub = spawn_stub(ok_focuser_router()).await;
        let entry = connect_focuser(&focuser_config_for(&stub.url()), None).await;
        assert!(entry.connected, "expected entry to be connected");
        assert!(entry.device.is_some(), "expected entry to hold a device");
        assert_eq!(entry.id, "main-focuser");
    }

    /// `EquipmentRegistry::new` with a focuser entry, plus `status()` and
    /// `find_focuser`. Pins the `EquipmentStatus.focusers` collection and
    /// the lookup helper that aren't exercised by `connect_focuser` tests
    /// in isolation.
    #[tokio::test]
    async fn equipment_registry_surfaces_connected_focuser_in_status_and_lookup() {
        let stub = spawn_stub(ok_focuser_router()).await;
        let equipment_cfg = config::EquipmentConfig {
            focusers: vec![focuser_config_for(&stub.url())],
            ..Default::default()
        };
        let registry = EquipmentRegistry::new(&equipment_cfg, None).await;
        assert_eq!(registry.focusers.len(), 1);

        let found = registry
            .find_focuser("main-focuser")
            .expect("find_focuser should return the configured focuser");
        assert!(found.connected);
        assert!(registry.find_focuser("nonexistent").is_none());

        let status = registry.status();
        assert_eq!(status.focusers.len(), 1);
        assert_eq!(status.focusers[0].id, "main-focuser");
        assert!(status.focusers[0].connected);
        assert!(
            status.mount.is_none(),
            "mount should be None when unconfigured"
        );
    }
}
