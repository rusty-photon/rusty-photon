use std::sync::Arc;

use ascom_alpaca::api::Telescope;
use tracing::{debug, error};

use super::alpaca::ConnectBudget;
use super::binding::{establish_listed, EstablishError};
use super::session::DeviceSession;
use crate::config;

/// The read-only custom Action a mount driver answers with whether a
/// power cut and the next power-up leave the mount where it parks
/// (star-adventurer-gti.md § `ParkMatchesPowerUpPose`).
pub const PARK_POSE_ACTION: &str = "ParkMatchesPowerUpPose";

/// `get_park_state`'s `park_matches_power_up_pose`: the driver's answer
/// to [`PARK_POSE_ACTION`] when its `SupportedActions` lists it, else
/// the mount entry's `power_cut_keeps_pose`, `None` when that is unset.
///
/// # Errors
///
/// A message naming the read that failed: `SupportedActions`, the
/// Action, or an answer without a boolean `matches`.
pub async fn park_matches_power_up_pose(
    mount: &dyn Telescope,
    config: &config::MountConfig,
) -> Result<Option<bool>, String> {
    let actions = mount
        .supported_actions()
        .await
        .map_err(|e| format!("failed to read the mount's SupportedActions: {e}"))?;
    if !actions
        .iter()
        .any(|action| action.eq_ignore_ascii_case(PARK_POSE_ACTION))
    {
        return Ok(config.power_cut_keeps_pose);
    }
    let answer = mount
        .action(PARK_POSE_ACTION.to_string(), String::new())
        .await
        .map_err(|e| format!("failed to read the mount's {PARK_POSE_ACTION}: {e}"))?;
    let matches = serde_json::from_str::<serde_json::Value>(&answer)
        .ok()
        .and_then(|value| value.get("matches").and_then(serde_json::Value::as_bool));
    debug!(?matches, "read the mount's {PARK_POSE_ACTION}");
    matches.map(Some).ok_or_else(|| {
        format!("the mount's {PARK_POSE_ACTION} answer carries no boolean matches: {answer}")
    })
}

/// Singular mount entry. Piggyback rigs share one mount across multiple
/// optical trains, so `EquipmentRegistry.mount` is an `Option`, not a
/// `Vec`. No `id` field — there is nothing to disambiguate.
pub struct MountEntry {
    pub config: config::MountConfig,
    pub session: DeviceSession<dyn Telescope>,
}

impl MountEntry {
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.session.is_connected()
    }

    #[must_use]
    pub fn device(&self) -> Option<Arc<dyn Telescope>> {
        self.session.device()
    }
}

/// Locate the configured mount on its Alpaca server and switch it on —
/// the shared routine behind the startup connect and the reconnect
/// supervisor's re-establish (rp.md § Device Session Recovery).
pub(super) async fn establish_mount(
    config: &config::MountConfig,
    ca_cert_path: Option<&std::path::Path>,
    budget: ConnectBudget,
) -> Result<Arc<dyn Telescope>, EstablishError> {
    establish_listed(config, ca_cert_path, budget).await
}

pub(super) async fn connect_mount(
    config: &config::MountConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> MountEntry {
    debug!(alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to mount");

    match establish_mount(config, ca_cert_path, ConnectBudget::Full).await {
        Ok(t) => {
            debug!("mount connected successfully");
            MountEntry {
                config: config.clone(),
                session: DeviceSession::connected(t),
            }
        }
        Err(msg) => {
            error!(error = %msg, "failed to connect mount");
            MountEntry {
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
    use crate::equipment::EquipmentRegistry;
    use crate::error::RpError;

    use axum::{
        routing::{get, put},
        Json, Router,
    };

    /// A mount whose driver lists `actions` in `SupportedActions` and
    /// answers its custom Action with `answer`.
    struct PoseMount {
        actions: Vec<String>,
        answer: ascom_alpaca::ASCOMResult<String>,
    }

    impl std::fmt::Debug for PoseMount {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "PoseMount")
        }
    }

    #[async_trait::async_trait]
    impl ascom_alpaca::api::Device for PoseMount {
        fn static_name(&self) -> &'static str {
            "pose-mount"
        }
        fn unique_id(&self) -> &'static str {
            "pose-mount"
        }
        async fn connected(&self) -> ascom_alpaca::ASCOMResult<bool> {
            Ok(true)
        }
        async fn set_connected(&self, _: bool) -> ascom_alpaca::ASCOMResult<()> {
            Ok(())
        }
        async fn description(&self) -> ascom_alpaca::ASCOMResult<String> {
            Ok("pose-mount".into())
        }
        async fn driver_info(&self) -> ascom_alpaca::ASCOMResult<String> {
            Ok("pose-mount".into())
        }
        async fn driver_version(&self) -> ascom_alpaca::ASCOMResult<String> {
            Ok("0.0".into())
        }
        async fn supported_actions(&self) -> ascom_alpaca::ASCOMResult<Vec<String>> {
            Ok(self.actions.clone())
        }
        async fn action(
            &self,
            action: String,
            _parameters: String,
        ) -> ascom_alpaca::ASCOMResult<String> {
            assert_eq!(action, PARK_POSE_ACTION, "rp asked for another Action");
            self.answer.clone()
        }
    }

    /// The required members a pose read never touches.
    #[async_trait::async_trait]
    impl Telescope for PoseMount {
        async fn at_home(&self) -> ascom_alpaca::ASCOMResult<bool> {
            Err(ascom_alpaca::ASCOMError::NOT_IMPLEMENTED)
        }
        async fn at_park(&self) -> ascom_alpaca::ASCOMResult<bool> {
            Err(ascom_alpaca::ASCOMError::NOT_IMPLEMENTED)
        }
        async fn declination_rate(&self) -> ascom_alpaca::ASCOMResult<f64> {
            Err(ascom_alpaca::ASCOMError::NOT_IMPLEMENTED)
        }
        async fn equatorial_system(
            &self,
        ) -> ascom_alpaca::ASCOMResult<ascom_alpaca::api::telescope::EquatorialCoordinateType>
        {
            Err(ascom_alpaca::ASCOMError::NOT_IMPLEMENTED)
        }
        async fn right_ascension(&self) -> ascom_alpaca::ASCOMResult<f64> {
            Err(ascom_alpaca::ASCOMError::NOT_IMPLEMENTED)
        }
        async fn right_ascension_rate(&self) -> ascom_alpaca::ASCOMResult<f64> {
            Err(ascom_alpaca::ASCOMError::NOT_IMPLEMENTED)
        }
        async fn sidereal_time(&self) -> ascom_alpaca::ASCOMResult<f64> {
            Err(ascom_alpaca::ASCOMError::NOT_IMPLEMENTED)
        }
        async fn tracking(&self) -> ascom_alpaca::ASCOMResult<bool> {
            Err(ascom_alpaca::ASCOMError::NOT_IMPLEMENTED)
        }
        async fn tracking_rate(
            &self,
        ) -> ascom_alpaca::ASCOMResult<ascom_alpaca::api::telescope::DriveRate> {
            Err(ascom_alpaca::ASCOMError::NOT_IMPLEMENTED)
        }
        async fn axis_rates(
            &self,
            _axis: ascom_alpaca::api::telescope::TelescopeAxis,
        ) -> ascom_alpaca::ASCOMResult<Vec<std::ops::RangeInclusive<f64>>> {
            Err(ascom_alpaca::ASCOMError::NOT_IMPLEMENTED)
        }
        async fn utc_date(&self) -> ascom_alpaca::ASCOMResult<std::time::SystemTime> {
            Err(ascom_alpaca::ASCOMError::NOT_IMPLEMENTED)
        }
    }

    fn pose_mount(actions: &[&str], answer: ascom_alpaca::ASCOMResult<String>) -> PoseMount {
        PoseMount {
            actions: actions.iter().map(|a| (*a).to_string()).collect(),
            answer,
        }
    }

    #[tokio::test]
    async fn a_driver_that_lists_the_action_answers_for_the_mount() {
        let mut config = mount_config_for("http://127.0.0.1:1");
        config.power_cut_keeps_pose = Some(true);
        let mount = pose_mount(
            &[PARK_POSE_ACTION],
            Ok(r#"{"matches": false, "power_up_pose": "ap_park_3"}"#.to_string()),
        );
        assert_eq!(
            park_matches_power_up_pose(&mount, &config).await.unwrap(),
            Some(false),
            "the driver's answer wins over the config"
        );
        let mount = pose_mount(&[PARK_POSE_ACTION], Ok(r#"{"matches": true}"#.to_string()));
        assert_eq!(
            park_matches_power_up_pose(&mount, &config).await.unwrap(),
            Some(true)
        );
    }

    #[tokio::test]
    async fn a_mount_without_the_action_reads_the_config_and_null_without_it() {
        let mut config = mount_config_for("http://127.0.0.1:1");
        let mount = pose_mount(
            &["SomethingElse"],
            Err(ascom_alpaca::ASCOMError::NOT_IMPLEMENTED),
        );
        assert_eq!(
            park_matches_power_up_pose(&mount, &config).await.unwrap(),
            None
        );
        config.power_cut_keeps_pose = Some(true);
        assert_eq!(
            park_matches_power_up_pose(&mount, &config).await.unwrap(),
            Some(true)
        );
        config.power_cut_keeps_pose = Some(false);
        assert_eq!(
            park_matches_power_up_pose(&mount, &config).await.unwrap(),
            Some(false)
        );
    }

    #[tokio::test]
    async fn a_failed_or_unreadable_action_answer_is_an_error() {
        let config = mount_config_for("http://127.0.0.1:1");
        let mount = pose_mount(
            &[PARK_POSE_ACTION],
            Err(ascom_alpaca::ASCOMError::NOT_CONNECTED),
        );
        let err = park_matches_power_up_pose(&mount, &config)
            .await
            .unwrap_err();
        assert!(err.contains(PARK_POSE_ACTION), "{err}");
        let mount = pose_mount(
            &[PARK_POSE_ACTION],
            Ok(r#"{"power_up_pose": "ap_park_3"}"#.to_string()),
        );
        let err = park_matches_power_up_pose(&mount, &config)
            .await
            .unwrap_err();
        assert!(err.contains("no boolean matches"), "{err}");
    }

    fn mount_config_for(url: &str) -> config::MountConfig {
        config::MountConfig {
            alpaca_url: url.to_string(),
            device_number: 0,
            unique_id: None,
            settle_after_slew: None,
            slew_rate_arcsec_per_sec: config::mount::SlewRateArcsecPerSec::default(),
            guiding: None,
            auth: None,
            power_cut_keeps_pose: None,
        }
    }

    #[tokio::test]
    async fn connect_mount_invalid_url_returns_disconnected_entry() {
        let cfg = mount_config_for("not-a-url");
        let entry = connect_mount(&cfg, None).await;
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }

    #[tokio::test]
    async fn connect_mount_unreachable_returns_disconnected_entry() {
        let cfg = mount_config_for("http://127.0.0.1:1");
        let entry = connect_mount(&cfg, None).await;
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
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
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
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
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn connect_mount_timeout_returns_disconnected_entry() {
        let app = Router::new().route(
            "/management/v1/configureddevices",
            get(|| async { std::future::pending::<Json<serde_json::Value>>().await }),
        );
        let stub = spawn_stub(app).await;
        let entry = connect_mount(&mount_config_for(&stub.url()), None).await;
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
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
        assert!(entry.is_connected(), "expected entry to be connected");
        assert!(entry.device().is_some(), "expected entry to hold a device");
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
        assert!(found.is_connected());

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
        // within the 0.01° tolerance. Without the wraparound, raw
        // subtraction would produce ~360° and SiteMismatch.
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
