use std::sync::Arc;

use ascom_alpaca::api::Rotator;
use tracing::{debug, error};

use super::binding::{establish_listed, reported_name, EstablishError};
use super::session::DeviceSession;
use crate::config;

/// Facts read once when a rotator session is established (rp.md § Rig
/// state), served from the entry afterwards and replaced on every
/// re-establish.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RotatorInvariants {
    /// The driver's ASCOM `Name`. `None` when the read failed or the
    /// name is blank.
    pub name: Option<String>,
    /// The driver's ASCOM `StepSize`, the smallest move it makes, in
    /// degrees. `None` when the driver does not implement it or reports
    /// a value that is not positive and finite.
    pub step_size_degrees: Option<f64>,
}

pub struct RotatorEntry {
    pub id: String,
    pub config: config::RotatorConfig,
    pub session: DeviceSession<dyn Rotator, RotatorInvariants>,

    /// One rotation through the rotator at a time (rp.md § Rotator Tool
    /// Details), and a train lease's hold on it (§ Train Lease).
    pub slot: super::DeviceSlot,
}

impl RotatorEntry {
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.session.is_connected()
    }

    #[must_use]
    pub fn device(&self) -> Option<Arc<dyn Rotator>> {
        self.session.device()
    }
}

/// The `StepSize` read as the cached fact: a positive finite value, or
/// unknown.
fn step_size_read(rotator_id: &str, read: ascom_alpaca::ASCOMResult<f64>) -> Option<f64> {
    match read {
        Ok(degrees) if degrees.is_finite() && degrees > 0.0 => Some(degrees),
        other => {
            debug!(rotator_id, step_size = ?other, "no usable StepSize at session-establish time; frames will record no step size for the rotator");
            None
        }
    }
}

/// Locate the configured device on its Alpaca server, switch it on, and
/// read what it says about itself — the shared routine behind the
/// startup connect and the reconnect supervisor's re-establish (rp.md §
/// Device Session Recovery). Property reads — nothing moves.
pub(super) async fn establish_rotator(
    config: &config::RotatorConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> Result<(Arc<dyn Rotator>, RotatorInvariants), EstablishError> {
    let rot = establish_listed(config, ca_cert_path).await?;
    let invariants = RotatorInvariants {
        name: reported_name(&config.id, rot.name().await),
        step_size_degrees: step_size_read(&config.id, rot.step_size().await),
    };
    Ok((rot, invariants))
}

pub(super) async fn connect_rotator(
    config: &config::RotatorConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> RotatorEntry {
    debug!(rotator_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to rotator");

    match establish_rotator(config, ca_cert_path).await {
        Ok((rot, invariants)) => {
            debug!(rotator_id = %config.id, ?invariants, "rotator connected successfully; cached its name and step size");
            RotatorEntry {
                id: config.id.clone(),
                config: config.clone(),
                session: DeviceSession::connected_with(rot, invariants),
                slot: crate::equipment::DeviceSlot::default(),
            }
        }
        Err(msg) => {
            error!(rotator_id = %config.id, error = %msg, "failed to connect rotator");
            RotatorEntry {
                id: config.id.clone(),
                config: config.clone(),
                session: DeviceSession::disconnected(),
                slot: crate::equipment::DeviceSlot::default(),
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
            unique_id: None,
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

    /// Answer a `GET` of one rotator property with `value`.
    fn with_property(router: Router, property: &str, value: serde_json::Value) -> Router {
        router.route(
            &format!("/api/v1/rotator/0/{property}"),
            get(move || async move {
                Json(serde_json::json!({
                    "Value": value,
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
        assert!(entry.is_connected(), "expected entry to be connected");
        assert!(entry.device().is_some(), "expected entry to hold a device");
        assert_eq!(entry.id, "falcon");
    }

    /// The driver's name and `StepSize` are read once and cached on the
    /// entry.
    #[tokio::test]
    async fn connect_rotator_caches_its_name_and_step_size() {
        let router = with_property(
            with_property(two_rotator_router(), "name", serde_json::json!("Falcon")),
            "stepsize",
            serde_json::json!(0.01),
        );
        let stub = spawn_stub(router).await;
        let entry = connect_rotator(&rotator_config_for(&stub.url(), 0), None).await;
        assert_eq!(
            entry.session.metadata(),
            RotatorInvariants {
                name: Some("Falcon".to_string()),
                step_size_degrees: Some(0.01),
            }
        );
    }

    /// A driver without a name or `StepSize`, or one that reports a step
    /// size that is not positive and finite, leaves each unknown and the
    /// rotator connected.
    #[tokio::test]
    async fn connect_rotator_without_a_usable_name_or_step_size_caches_neither() {
        for step_size in [
            None,
            Some(serde_json::json!(0.0)),
            Some(serde_json::json!(-1.0)),
        ] {
            let router = match step_size.clone() {
                Some(value) => with_property(two_rotator_router(), "stepsize", value),
                None => two_rotator_router(),
            };
            let stub = spawn_stub(router).await;
            let entry = connect_rotator(&rotator_config_for(&stub.url(), 0), None).await;
            assert!(
                entry.is_connected(),
                "{step_size:?}: expected a connected entry"
            );
            assert_eq!(
                entry.session.metadata(),
                RotatorInvariants::default(),
                "{step_size:?}"
            );
        }
    }

    /// `device_number` indexes among the server's Rotators, so connecting to
    /// index 1 must skip past the rotator at index 0.
    #[tokio::test]
    async fn connect_rotator_skips_to_the_requested_index() {
        let stub = spawn_stub(two_rotator_router()).await;
        let entry = connect_rotator(&rotator_config_for(&stub.url(), 1), None).await;
        assert!(entry.is_connected(), "expected entry to be connected");
        assert!(entry.device().is_some(), "expected entry to hold a device");
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
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }

    #[tokio::test]
    async fn connect_rotator_client_build_failure_returns_disconnected_entry() {
        let entry = connect_rotator(&rotator_config_for("not-a-url", 0), None).await;
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
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
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
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
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }
}
