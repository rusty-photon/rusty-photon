use std::sync::Arc;

use ascom_alpaca::api::{Telescope, TypedDevice};
use tracing::{debug, error};

use super::alpaca::{
    build_alpaca_client, retry_connect_attempt, AttemptOutcome, GET_DEVICES_TIMEOUT,
};
use crate::config;

/// Singular mount entry. Piggyback rigs share one mount across multiple
/// optical trains, so `EquipmentRegistry.mount` is an `Option`, not a
/// `Vec`. No `id` field — there is nothing to disambiguate.
pub struct MountEntry {
    pub connected: bool,
    pub config: config::MountConfig,
    pub device: Option<Arc<dyn Telescope>>,
}

pub(super) async fn connect_mount(
    config: &config::MountConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> MountEntry {
    debug!(alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to mount");

    let client = match build_alpaca_client(&config.alpaca_url, config.auth.as_ref(), ca_cert_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "failed to create Alpaca client for mount");
            return MountEntry {
                connected: false,
                config: config.clone(),
                device: None,
            };
        }
    };

    let outcome = retry_connect_attempt("mount", |_attempt| async {
        let devices = match tokio::time::timeout(GET_DEVICES_TIMEOUT, client.get_devices()).await {
            Ok(Ok(devices)) => devices,
            Ok(Err(e)) => return AttemptOutcome::Transient(format!("get_devices: {e}")),
            Err(_) => {
                return AttemptOutcome::Transient(format!(
                    "get_devices: timeout after {GET_DEVICES_TIMEOUT:?}"
                ));
            }
        };

        let mut mount_index = 0u32;
        let mut found_mount: Option<Arc<dyn Telescope>> = None;
        for device in devices {
            if let TypedDevice::Telescope(t) = device {
                if mount_index == config.device_number {
                    found_mount = Some(t);
                    break;
                }
                mount_index = mount_index.saturating_add(1);
            }
        }

        let t = match found_mount {
            Some(t) => t,
            None => {
                return AttemptOutcome::Permanent(format!(
                    "mount at index {} not found on Alpaca server",
                    config.device_number
                ));
            }
        };

        match t.set_connected(true).await {
            Ok(()) => AttemptOutcome::Ok(t),
            Err(e) => AttemptOutcome::Transient(format!("set_connected: {e}")),
        }
    })
    .await;

    match outcome {
        Ok(t) => {
            debug!("mount connected successfully");
            MountEntry {
                connected: true,
                config: config.clone(),
                device: Some(t),
            }
        }
        Err(msg) => {
            error!(error = %msg, "failed to connect mount");
            MountEntry {
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
    use crate::error::RpError;

    use axum::{
        routing::{get, put},
        Json, Router,
    };

    fn mount_config_for(url: &str) -> config::MountConfig {
        config::MountConfig {
            alpaca_url: url.to_string(),
            device_number: 0,
            settle_after_slew: None,
            slew_rate_arcsec_per_sec: config::mount::SlewRateArcsecPerSec::default(),
            guiding: None,
            auth: None,
        }
    }

    #[tokio::test]
    async fn connect_mount_invalid_url_returns_disconnected_entry() {
        let cfg = mount_config_for("not-a-url");
        let entry = connect_mount(&cfg, None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    #[tokio::test]
    async fn connect_mount_unreachable_returns_disconnected_entry() {
        let cfg = mount_config_for("http://127.0.0.1:1");
        let entry = connect_mount(&cfg, None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    #[tokio::test]
    async fn connect_mount_no_telescope_in_devices_returns_disconnected_entry() {
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
        let entry = connect_mount(&mount_config_for(&stub.url()), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    #[tokio::test]
    async fn connect_mount_set_connected_fails_returns_disconnected_entry() {
        let app = Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [{
                            "DeviceName": "Telescope 0",
                            "DeviceType": "Telescope",
                            "DeviceNumber": 0,
                            "UniqueID": "test-mount-uid"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/telescope/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 1024,
                        "ErrorMessage": "simulated set_connected failure"
                    }))
                }),
            );
        let stub = spawn_stub(app).await;
        let entry = connect_mount(&mount_config_for(&stub.url()), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn connect_mount_timeout_returns_disconnected_entry() {
        let app = Router::new().route(
            "/management/v1/configureddevices",
            get(|| async { std::future::pending::<Json<serde_json::Value>>().await }),
        );
        let stub = spawn_stub(app).await;
        let entry = connect_mount(&mount_config_for(&stub.url()), None).await;
        assert!(!entry.connected);
        assert!(entry.device.is_none());
    }

    fn ok_mount_router() -> Router {
        Router::new()
            .route(
                "/management/v1/configureddevices",
                get(|| async {
                    Json(serde_json::json!({
                        "Value": [{
                            "DeviceName": "Telescope 0",
                            "DeviceType": "Telescope",
                            "DeviceNumber": 0,
                            "UniqueID": "test-mount-uid"
                        }],
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/telescope/0/connected",
                put(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
    }

    #[tokio::test]
    async fn connect_mount_success_returns_connected_entry() {
        let stub = spawn_stub(ok_mount_router()).await;
        let entry = connect_mount(&mount_config_for(&stub.url()), None).await;
        assert!(entry.connected, "expected entry to be connected");
        assert!(entry.device.is_some(), "expected entry to hold a device");
    }

    #[tokio::test]
    async fn equipment_registry_surfaces_connected_mount_in_status_and_lookup() {
        let stub = spawn_stub(ok_mount_router()).await;
        let equipment_cfg = config::EquipmentConfig {
            mount: Some(mount_config_for(&stub.url())),
            ..Default::default()
        };
        let registry = EquipmentRegistry::new(&equipment_cfg, None).await;

        let found = registry
            .find_mount()
            .expect("find_mount should return the configured mount");
        assert!(found.connected);

        let status = registry.status();
        let mount_status = status
            .mount
            .as_ref()
            .expect("EquipmentStatus.mount should be Some when configured");
        assert!(mount_status.connected);
    }

    /// Build an `ok_mount_router` extended with `SiteLatitude` /
    /// `SiteLongitude` Get handlers that return the supplied values.
    fn mount_router_with_site(lat: f64, lon: f64) -> Router {
        ok_mount_router()
            .route(
                "/api/v1/telescope/0/sitelatitude",
                get(move || async move {
                    Json(serde_json::json!({
                        "Value": lat,
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
            .route(
                "/api/v1/telescope/0/sitelongitude",
                get(move || async move {
                    Json(serde_json::json!({
                        "Value": lon,
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            )
    }

    /// Build an `ok_mount_router` whose `SiteLatitude` / `SiteLongitude`
    /// endpoints respond with `NOT_IMPLEMENTED` (ASCOM error 0x400),
    /// modelling a mount that lacks the property. The `validate_site`
    /// path treats this as "skip validation" rather than "fail loud".
    fn mount_router_without_site() -> Router {
        ok_mount_router()
            .route(
                "/api/v1/telescope/0/sitelatitude",
                get(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0x400,
                        "ErrorMessage": "Property SiteLatitude is not implemented"
                    }))
                }),
            )
            .route(
                "/api/v1/telescope/0/sitelongitude",
                get(|| async {
                    Json(serde_json::json!({
                        "ErrorNumber": 0x400,
                        "ErrorMessage": "Property SiteLongitude is not implemented"
                    }))
                }),
            )
    }

    async fn registry_with_mount(stub_url: &str) -> EquipmentRegistry {
        let equipment_cfg = config::EquipmentConfig {
            mount: Some(mount_config_for(stub_url)),
            ..Default::default()
        };
        EquipmentRegistry::new(&equipment_cfg, None).await
    }

    #[tokio::test]
    async fn validate_site_no_op_when_site_absent() {
        let stub = spawn_stub(ok_mount_router()).await;
        let registry = registry_with_mount(&stub.url()).await;
        registry
            .validate_site(None)
            .await
            .expect("missing site config must short-circuit cleanly");
    }

    #[tokio::test]
    async fn validate_site_no_op_when_mount_absent() {
        let registry = EquipmentRegistry::new(&config::EquipmentConfig::default(), None).await;
        let site = config::SiteConfig {
            latitude_degrees: 47.6062,
            longitude_degrees: -122.3321,
        };
        registry
            .validate_site(Some(&site))
            .await
            .expect("no mount → no validation, no error");
    }

    #[tokio::test]
    async fn validate_site_skips_when_mount_lacks_property() {
        let stub = spawn_stub(mount_router_without_site()).await;
        let registry = registry_with_mount(&stub.url()).await;
        let site = config::SiteConfig {
            latitude_degrees: 47.6062,
            longitude_degrees: -122.3321,
        };
        // No `CanGetSiteLatitude`/`Longitude` in ASCOM — the read
        // attempt itself is the capability probe. NOT_IMPLEMENTED
        // (or any other error) should be debug-logged and skipped.
        registry
            .validate_site(Some(&site))
            .await
            .expect("missing property must skip, not error");
    }

    #[tokio::test]
    async fn validate_site_passes_when_mount_agrees() {
        let stub = spawn_stub(mount_router_with_site(47.6062, -122.3321)).await;
        let registry = registry_with_mount(&stub.url()).await;
        let site = config::SiteConfig {
            latitude_degrees: 47.6062,
            longitude_degrees: -122.3321,
        };
        registry.validate_site(Some(&site)).await.unwrap();
    }

    #[tokio::test]
    async fn validate_site_passes_when_within_tolerance() {
        // Diff = 0.005° in each dim, below the 0.01° hard cap.
        let stub = spawn_stub(mount_router_with_site(47.611, -122.337)).await;
        let registry = registry_with_mount(&stub.url()).await;
        let site = config::SiteConfig {
            latitude_degrees: 47.606,
            longitude_degrees: -122.332,
        };
        registry.validate_site(Some(&site)).await.unwrap();
    }

    #[tokio::test]
    async fn validate_site_errors_on_latitude_mismatch() {
        // Mount reports lat off by 1°, well past the 0.01° cap.
        let stub = spawn_stub(mount_router_with_site(48.6062, -122.3321)).await;
        let registry = registry_with_mount(&stub.url()).await;
        let site = config::SiteConfig {
            latitude_degrees: 47.6062,
            longitude_degrees: -122.3321,
        };
        let err = registry.validate_site(Some(&site)).await.unwrap_err();
        match err {
            RpError::SiteMismatch {
                config_lat,
                config_lon,
                mount_lat,
                mount_lon,
            } => {
                assert!((config_lat - 47.6062).abs() < 1e-9);
                assert!((config_lon - -122.3321).abs() < 1e-9);
                assert!((mount_lat - 48.6062).abs() < 1e-9);
                assert!((mount_lon - -122.3321).abs() < 1e-9);
            }
            other => panic!("expected SiteMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn validate_site_passes_across_antimeridian() {
        // Configured at 179.999° E; mount reports -179.999° E. They
        // describe meridians 0.002° apart going the short way — well
        // within the 0.01° tolerance. Without the wraparound fix,
        // raw subtraction would produce ~360° and SiteMismatch.
        let stub = spawn_stub(mount_router_with_site(0.0, -179.999)).await;
        let registry = registry_with_mount(&stub.url()).await;
        let site = config::SiteConfig {
            latitude_degrees: 0.0,
            longitude_degrees: 179.999,
        };
        registry.validate_site(Some(&site)).await.unwrap();
    }

    #[tokio::test]
    async fn validate_site_errors_on_longitude_mismatch() {
        let stub = spawn_stub(mount_router_with_site(47.6062, -120.0)).await;
        let registry = registry_with_mount(&stub.url()).await;
        let site = config::SiteConfig {
            latitude_degrees: 47.6062,
            longitude_degrees: -122.3321,
        };
        let err = registry.validate_site(Some(&site)).await.unwrap_err();
        assert!(
            matches!(err, RpError::SiteMismatch { .. }),
            "expected SiteMismatch, got {err}"
        );
        // The Display impl must name both pairs so an operator who
        // sees this in a startup log knows what the disagreement is.
        let s = err.to_string();
        assert!(s.contains("47.6062"), "missing config lat: {s}");
        assert!(s.contains("-122.3321"), "missing config lon: {s}");
        assert!(s.contains("-120.0000"), "missing mount lon: {s}");
    }
}
