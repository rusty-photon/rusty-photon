use std::sync::Arc;

use ascom_alpaca::api::Focuser;
use tracing::{debug, error};

use super::binding::{establish_listed, EstablishError};
use super::session::DeviceSession;
use crate::config;

/// Facts read once when a focuser session is established (rp.md §
/// Train optics), served from the entry afterwards and replaced on
/// every re-establish.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FocuserInvariants {
    /// The driver's `StepSize` in microns — the image-plane travel of
    /// one step when `focusers[].microns_per_step` is not configured.
    /// `None` when the driver does not implement it or reports a
    /// non-positive or non-finite value.
    pub step_size_um: Option<f64>,
    /// The driver's `MaxStep` — the top of an absolute focuser's
    /// travel, whose bottom ASCOM fixes at `0` (rp.md § Focuser Tool
    /// Details). `None` when the read failed.
    pub max_step: Option<i32>,
}

pub struct FocuserEntry {
    pub id: String,
    pub config: config::FocuserConfig,
    /// The handle and the connect-time reads of the same session,
    /// installed together by [`DeviceSession`]. The temperature watch
    /// takes the handle alone and the train optics the step size alone;
    /// a move takes the handle and the travel ceiling together, through
    /// [`Self::device_with_travel_bounds`].
    pub session: DeviceSession<dyn Focuser, FocuserInvariants>,

    /// One move through the focuser at a time (rp.md § Focuser Tool
    /// Details), and a train lease's hold on it (§ Train Lease).
    pub slot: super::DeviceSlot,
}

impl FocuserEntry {
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.session.is_connected()
    }

    #[must_use]
    pub fn device(&self) -> Option<Arc<dyn Focuser>> {
        self.session.device()
    }

    /// Snapshot of the connect-time reads.
    #[must_use]
    pub fn invariants(&self) -> FocuserInvariants {
        self.session.metadata()
    }

    /// The handle of the current session together with the `(min, max)`
    /// travel every move through it stays inside: the configured bounds
    /// narrowed to the driver's travel, which stands in where the config
    /// sets none (rp.md § Focuser Tool Details). One
    /// [`DeviceSession::snapshot`], so a target checked against these
    /// bounds is commanded through the driver whose `MaxStep` set them.
    #[must_use]
    pub fn device_with_travel_bounds(&self) -> Option<(Arc<dyn Focuser>, TravelBounds)> {
        self.session.snapshot().map(|(device, invariants)| {
            let bounds = travel_bounds(
                (self.config.min_position, self.config.max_position),
                invariants.max_step,
            );
            (device, bounds)
        })
    }
}

/// A focuser's `(min, max)` travel bounds; `None` is no bound on that
/// side.
pub type TravelBounds = (Option<i32>, Option<i32>);

/// The configured bounds narrowed to `0`..`max_step`. The floor is the
/// ASCOM spec's, so it holds without a read; the ceiling is `None` only
/// when neither the config nor the `MaxStep` read supplies one.
fn travel_bounds(configured: TravelBounds, max_step: Option<i32>) -> TravelBounds {
    let min = configured.0.map_or(0, |min| min.max(0));
    let max = match (configured.1, max_step) {
        (Some(max), Some(ceiling)) => Some(max.min(ceiling)),
        (max, ceiling) => max.or(ceiling),
    };
    (Some(min), max)
}

/// The `MaxStep` read as the cached fact. A `Position` is an `i32`, so a
/// ceiling above `i32::MAX` is no ceiling a move could reach.
fn max_step_read(focuser_id: &str, read: ascom_alpaca::ASCOMResult<u32>) -> Option<i32> {
    match read {
        Ok(steps) => Some(i32::try_from(steps).unwrap_or(i32::MAX)),
        Err(e) => {
            debug!(
                focuser_id,
                error = %e,
                "MaxStep unavailable at session-establish time; the travel ceiling is the configured max_position alone"
            );
            None
        }
    }
}

/// The `StepSize` read as the cached fact: a positive finite value, or
/// unknown — a driver that does not implement the property answers
/// `NOT_IMPLEMENTED`, and one that reports zero has nothing to say.
fn step_size_read(focuser_id: &str, read: ascom_alpaca::ASCOMResult<f64>) -> Option<f64> {
    match read {
        Ok(microns) if microns.is_finite() && microns > 0.0 => Some(microns),
        Ok(microns) => {
            debug!(
                focuser_id,
                step_size = microns,
                "driver reports a non-positive StepSize; microns_per_step stays unknown unless configured"
            );
            None
        }
        Err(e) => {
            debug!(
                focuser_id,
                error = %e,
                "StepSize unavailable at session-establish time; microns_per_step stays unknown unless configured"
            );
            None
        }
    }
}

/// Locate the configured focuser on its Alpaca server and switch it
/// on — the shared routine behind the startup connect and the reconnect
/// supervisor's re-establish (rp.md § Device Session Recovery).
pub(super) async fn establish_focuser(
    config: &config::FocuserConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> Result<(Arc<dyn Focuser>, FocuserInvariants), EstablishError> {
    let foc = establish_listed(config, ca_cert_path).await?;

    // `StepSize` and `MaxStep` are properties of the mechanism,
    // invariant for the life of the session: read once here, after
    // `set_connected` (some drivers reject property reads on a
    // disconnected device), and served from the entry — the step size to
    // every `get_train_info`, the ceiling to every move's travel bounds.
    // Property reads — nothing moves.
    let invariants = FocuserInvariants {
        step_size_um: step_size_read(&config.id, foc.step_size().await),
        max_step: max_step_read(&config.id, foc.max_step().await),
    };
    Ok((foc, invariants))
}

pub(super) async fn connect_focuser(
    config: &config::FocuserConfig,
    ca_cert_path: Option<&std::path::Path>,
) -> FocuserEntry {
    debug!(focuser_id = %config.id, alpaca_url = %config.alpaca_url, device_number = config.device_number, "connecting to focuser");

    match establish_focuser(config, ca_cert_path).await {
        Ok((foc, invariants)) => {
            debug!(
                focuser_id = %config.id,
                step_size_um = ?invariants.step_size_um,
                max_step = ?invariants.max_step,
                "focuser connected successfully; cached its step size and max step"
            );
            FocuserEntry {
                id: config.id.clone(),
                config: config.clone(),
                session: DeviceSession::connected_with(foc, invariants),
                slot: crate::equipment::DeviceSlot::default(),
            }
        }
        Err(msg) => {
            error!(focuser_id = %config.id, error = %msg, "failed to connect focuser");
            FocuserEntry {
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
    use crate::equipment::EquipmentRegistry;

    use axum::{
        routing::{get, put},
        Json, Router,
    };

    fn focuser_config_for(url: &str) -> config::FocuserConfig {
        config::FocuserConfig {
            microns_per_step: None,
            id: "main-focuser".to_string(),
            alpaca_url: url.to_string(),
            device_number: 0,
            unique_id: None,
            min_position: None,
            max_position: None,
            steps_per_sec: config::focuser::FocuserStepsPerSec::default(),
            backlash: None,
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
            microns_per_step: None,
            id: "main-focuser".to_string(),
            alpaca_url: "not-a-url".to_string(),
            device_number: 0,
            unique_id: None,
            min_position: None,
            max_position: None,
            steps_per_sec: config::focuser::FocuserStepsPerSec::default(),
            backlash: None,
            auth: None,
        };
        let entry = connect_focuser(&cfg, None).await;
        assert_eq!(entry.id, "main-focuser");
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }

    #[tokio::test]
    async fn connect_focuser_unreachable_returns_disconnected_entry() {
        // 127.0.0.1:1 is reserved and refuses connections — `get_devices`
        // returns an error inside the 5s timeout window, exercising the
        // `Ok(Err(e))` arm of `connect_focuser`'s match.
        let cfg = config::FocuserConfig {
            microns_per_step: None,
            id: "main-focuser".to_string(),
            alpaca_url: "http://127.0.0.1:1".to_string(),
            device_number: 0,
            unique_id: None,
            min_position: None,
            max_position: None,
            steps_per_sec: config::focuser::FocuserStepsPerSec::default(),
            backlash: None,
            auth: None,
        };
        let entry = connect_focuser(&cfg, None).await;
        assert_eq!(entry.id, "main-focuser");
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
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
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
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
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
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
        assert!(!entry.is_connected());
        assert!(entry.device().is_none());
    }

    /// Build a router that successfully advertises one focuser at index 0
    /// and accepts `set_connected(true)`, answering `StepSize` with
    /// `step_size` when given. Shared by the success-path
    /// `connect_focuser` test and the `EquipmentRegistry` end-to-end test.
    fn focuser_router_with_step_size(step_size: Option<serde_json::Value>) -> Router {
        let router = Router::new()
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
            );
        match step_size {
            Some(value) => router.route(
                "/api/v1/focuser/0/stepsize",
                get(move || async move {
                    Json(serde_json::json!({
                        "Value": value,
                        "ErrorNumber": 0,
                        "ErrorMessage": ""
                    }))
                }),
            ),
            None => router,
        }
    }

    fn travel_bounds_of(entry: &FocuserEntry) -> TravelBounds {
        entry
            .device_with_travel_bounds()
            .expect("a connected entry has a session")
            .1
    }

    /// Add a `MaxStep` answer to a focuser router.
    fn with_max_step(router: Router, max_step: u32) -> Router {
        router.route(
            "/api/v1/focuser/0/maxstep",
            get(move || async move {
                Json(serde_json::json!({
                    "Value": max_step,
                    "ErrorNumber": 0,
                    "ErrorMessage": ""
                }))
            }),
        )
    }

    fn ok_focuser_router() -> Router {
        with_max_step(
            focuser_router_with_step_size(Some(serde_json::json!(2.5))),
            22_000,
        )
    }

    /// Server advertises a focuser at index 0 and accepts `set_connected`,
    /// exercising the `Ok(())` arm of `connect_focuser` plus the device
    /// iteration `Some(_)` match arm — the success path that doesn't run
    /// in any of the failure-branch tests above. The driver's `StepSize`
    /// and `MaxStep` are read once and cached on the entry.
    #[tokio::test]
    async fn connect_focuser_success_returns_connected_entry() {
        let stub = spawn_stub(ok_focuser_router()).await;
        let entry = connect_focuser(&focuser_config_for(&stub.url()), None).await;
        assert!(entry.is_connected(), "expected entry to be connected");
        assert!(entry.device().is_some(), "expected entry to hold a device");
        assert_eq!(entry.id, "main-focuser");
        assert_eq!(entry.invariants().step_size_um, Some(2.5));
        assert_eq!(entry.invariants().max_step, Some(22_000));
    }

    /// A focuser configured without bounds is bounded by the driver's
    /// travel once connected: `0` to the `MaxStep` it read.
    #[tokio::test]
    async fn a_connected_focuser_without_configured_bounds_is_bounded_by_its_driver() {
        let stub = spawn_stub(ok_focuser_router()).await;
        let entry = connect_focuser(&focuser_config_for(&stub.url()), None).await;
        assert_eq!(travel_bounds_of(&entry), (Some(0), Some(22_000)));
    }

    /// A `MaxStep` read that fails (the stub answers 404, the client an
    /// error) leaves the ceiling unknown; the session itself is
    /// unaffected, and the floor still holds.
    #[tokio::test]
    async fn connect_focuser_leaves_the_max_step_unknown_when_the_read_fails() {
        let stub = spawn_stub(focuser_router_with_step_size(None)).await;
        let entry = connect_focuser(&focuser_config_for(&stub.url()), None).await;
        assert!(entry.is_connected(), "expected a connected entry");
        assert_eq!(entry.invariants().max_step, None);
        assert_eq!(travel_bounds_of(&entry), (Some(0), None));
    }

    #[test]
    fn max_step_read_saturates_a_ceiling_no_position_can_reach() {
        assert_eq!(max_step_read("foc", Ok(u32::MAX)), Some(i32::MAX));
    }

    #[test]
    fn max_step_read_keeps_a_ceiling_inside_the_position_range() {
        assert_eq!(max_step_read("foc", Ok(60_000)), Some(60_000));
    }

    #[test]
    fn travel_bounds_fall_back_to_the_driver_travel_when_the_config_sets_none() {
        assert_eq!(
            travel_bounds((None, None), Some(22_000)),
            (Some(0), Some(22_000))
        );
    }

    #[test]
    fn travel_bounds_keep_the_floor_when_the_max_step_is_unknown() {
        assert_eq!(travel_bounds((None, None), None), (Some(0), None));
    }

    #[test]
    fn travel_bounds_keep_configured_bounds_inside_the_driver_travel() {
        assert_eq!(
            travel_bounds((Some(1000), Some(9000)), Some(22_000)),
            (Some(1000), Some(9000))
        );
    }

    #[test]
    fn travel_bounds_raise_a_negative_configured_minimum_to_zero() {
        assert_eq!(
            travel_bounds((Some(-500), Some(9000)), Some(22_000)),
            (Some(0), Some(9000))
        );
    }

    #[test]
    fn travel_bounds_lower_a_configured_maximum_beyond_max_step_to_it() {
        assert_eq!(
            travel_bounds((Some(1000), Some(60_000)), Some(22_000)),
            (Some(1000), Some(22_000))
        );
    }

    #[test]
    fn travel_bounds_fill_each_unset_side_on_its_own() {
        assert_eq!(
            travel_bounds((Some(1000), None), Some(22_000)),
            (Some(1000), Some(22_000))
        );
        assert_eq!(
            travel_bounds((None, Some(9000)), Some(22_000)),
            (Some(0), Some(9000))
        );
    }

    #[test]
    fn travel_bounds_leave_the_configured_maximum_as_written_when_the_max_step_is_unknown() {
        assert_eq!(
            travel_bounds((None, Some(60_000)), None),
            (Some(0), Some(60_000))
        );
    }

    /// A driver without `StepSize` (the stub answers 404, the client an
    /// error) or one reporting zero leaves the fact unknown; the session
    /// itself is unaffected.
    #[tokio::test]
    async fn connect_focuser_leaves_the_step_size_unknown_when_the_driver_has_none() {
        for step_size in [
            None,
            Some(serde_json::json!(0)),
            Some(serde_json::json!(-4.0)),
        ] {
            let stub = spawn_stub(focuser_router_with_step_size(step_size.clone())).await;
            let entry = connect_focuser(&focuser_config_for(&stub.url()), None).await;
            assert!(
                entry.is_connected(),
                "{step_size:?}: expected a connected entry"
            );
            assert_eq!(
                entry.invariants().step_size_um,
                None,
                "{step_size:?}: the step size must stay unknown"
            );
        }
    }

    /// A re-established session replaces the handle and the facts read
    /// from it in one step, so the entry never serves the step size of
    /// a session other than the one its handle belongs to.
    #[tokio::test]
    async fn installing_a_session_replaces_the_handle_and_the_facts_together() {
        let first = spawn_stub(ok_focuser_router()).await;
        let entry = connect_focuser(&focuser_config_for(&first.url()), None).await;
        assert_eq!(entry.invariants().step_size_um, Some(2.5));

        let second = spawn_stub(with_max_step(
            focuser_router_with_step_size(Some(serde_json::json!(20.0))),
            30_000,
        ))
        .await;
        let fresh = connect_focuser(&focuser_config_for(&second.url()), None).await;
        entry.session.install(
            fresh
                .device()
                .expect("the second connect must yield a handle"),
            fresh.invariants(),
        );
        assert_eq!(entry.invariants().step_size_um, Some(20.0));
        assert_eq!(travel_bounds_of(&entry), (Some(0), Some(30_000)));
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
        assert!(found.is_connected());
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
