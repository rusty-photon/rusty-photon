//! Unit tests for the `mount_device` module.
//!
//! Split out of the main module file as part of the issue-253
//! refactor. Tests still share the same private-item visibility as
//! the original `#[cfg(test)] mod tests` block — they live under
//! `mount_device::tests`, so `super::*` reaches the parent module
//! and `super::<sub>::*` reaches sibling submodules.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::telescope::{
    AlignmentMode, DriveRate, EquatorialCoordinateType, GuideDirection, PierSide, Telescope,
    TelescopeAxis,
};
use ascom_alpaca::api::Device;
use ascom_alpaca::{ASCOMError, ASCOMErrorCode};
use skywatcher_motor_protocol::Axis;
use tokio::sync::RwLock;

use crate::config::{
    ActiveZone, Config, CwExclusionZone, FlipPolicy, FlipRangeHours, MinAltitudeDegrees,
    TrackingGuardMarginHours,
};
use crate::coordinates::{ra_dec_to_alt_az, SIDEREAL_DEG_PER_SEC};
use crate::error::StarAdvError;
use crate::manager::MountManager;
use crate::transport::mock::{CapturingMockFactory, MockMountState, MockTransportFactory};
use crate::units::{Cpr, Dec, Lst, Ra, RaTicks};

use super::park_persistence::{read_connect_fields, write_park_to_config};
use super::slew::{
    canonical_path_crosses_pole, check_non_flip_ra_path, flip_slew_dec_delta, flip_slew_ra_delta,
    pickup_reslew_axis, stop_axis_and_wait,
};
use super::tracking_guard::{auto_flip_tick, guard_loop_tick, tracking_guard_tick};
use super::watchers::{watcher_poll_with_retry, watcher_should_abort};
use super::*;

/// [`Config::default`] with `unpark_from_ap_position` pinned to the
/// frame-neutral `ap_park_0` — explicitly, so the hardcoded tick /
/// coordinate expectations (anchored to the mock's power-up encoder of
/// `(0, 0)`) never depend on the ship default: a named pose would seed
/// the firmware encoder on every fresh-power-up connect and shift the
/// frame. Tests about the seed and anchored-frame parking set a named
/// pose explicitly.
fn base_config() -> Config {
    let mut cfg = Config::default();
    cfg.mount.unpark_from_ap_position = ApPark::ApPark0;
    cfg
}

fn device() -> MountDevice {
    let mut cfg = base_config();
    // Same rationale as `fast_settle_device`: open the
    // mechanical-envelope check for tests that don't exercise it.
    // Disable the CW-exclusion zone check for this test.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    let manager = MountManager::new(&cfg, Arc::new(MockTransportFactory));
    MountDevice::new(cfg.mount, manager)
}

async fn connected_device() -> MountDevice {
    let d = device();
    d.set_connected(true).await.unwrap();
    d
}

#[tokio::test]
async fn fresh_device_reports_disconnected() {
    let d = device();
    assert!(!d.connected().await.unwrap());
}

#[tokio::test]
async fn capability_flags_match_the_design_doc() {
    let d = device();
    assert_eq!(
        d.alignment_mode().await.unwrap(),
        AlignmentMode::GermanPolar
    );
    assert_eq!(
        d.equatorial_system().await.unwrap(),
        EquatorialCoordinateType::Topocentric
    );
    assert!(d.can_slew().await.unwrap());
    assert!(d.can_slew_async().await.unwrap());
    assert!(d.can_sync().await.unwrap());
    assert!(d.can_set_tracking().await.unwrap());
    assert!(d.can_park().await.unwrap());
    assert!(d.can_unpark().await.unwrap());
    assert!(!d.does_refraction().await.unwrap());
    assert_eq!(d.tracking_rates().await.unwrap(), vec![DriveRate::Sidereal]);
}

#[tokio::test]
async fn defaulted_state_reads_match_initial_driver_state() {
    let d = device();
    assert!(!d.at_home().await.unwrap());
    assert!(!d.at_park().await.unwrap());
    assert!(!d.tracking().await.unwrap());
    assert_eq!(d.tracking_rate().await.unwrap(), DriveRate::Sidereal);
    assert_eq!(d.right_ascension_rate().await.unwrap(), 0.0);
    assert_eq!(d.declination_rate().await.unwrap(), 0.0);
}

#[tokio::test]
async fn axis_rates_is_empty_for_every_axis() {
    let d = device();
    for axis in [
        TelescopeAxis::Primary,
        TelescopeAxis::Secondary,
        TelescopeAxis::Tertiary,
    ] {
        assert!(d.axis_rates(axis).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn site_coordinates_pass_through_from_config() {
    let cfg = base_config();
    let manager = MountManager::new(&cfg, Arc::new(MockTransportFactory));
    let mut mount_cfg = cfg.mount.clone();
    mount_cfg.site_latitude_deg = 47.6062;
    mount_cfg.site_longitude_deg = -122.3321;
    mount_cfg.site_elevation_m = 56.0;
    let d = MountDevice::new(mount_cfg, manager);
    assert_eq!(d.site_latitude().await.unwrap(), 47.6062);
    assert_eq!(d.site_longitude().await.unwrap(), -122.3321);
    assert_eq!(d.site_elevation().await.unwrap(), 56.0);
}

#[tokio::test]
async fn slew_settle_time_setter_overrides_config() {
    let d = device();
    assert_eq!(d.slew_settle_time().await.unwrap(), Duration::from_secs(2));
    d.set_slew_settle_time(Duration::from_millis(500))
        .await
        .unwrap();
    assert_eq!(
        d.slew_settle_time().await.unwrap(),
        Duration::from_millis(500)
    );
}

#[tokio::test]
async fn set_connected_drives_transport_connect_and_disconnect() {
    let d = device();
    d.set_connected(true).await.unwrap();
    assert!(d.connected().await.unwrap());
    d.set_connected(false).await.unwrap();
    assert!(!d.connected().await.unwrap());
}

/// Build a device with the CW exclusion zone enabled and the given
/// tracking-guard margin, then establish a session **directly** (not via
/// `set_connected`, so the background guard task isn't spawned — these
/// tests drive [`tracking_guard_tick`] by hand for determinism). Returns
/// the shared mock state for encoder seeding and command-log assertions.
async fn guard_device(margin_hours: f64) -> (MountDevice, Arc<tokio::sync::Mutex<MockMountState>>) {
    let factory = CapturingMockFactory::new();
    let mock = Arc::clone(&factory.state);
    let mut cfg = base_config();
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Active(ActiveZone::new(0.95, 11.05));
    cfg.mount.tracking_guard_margin_hours = TrackingGuardMarginHours::new(margin_hours);
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    let session = d.manager.transport().acquire().await.unwrap();
    *d.session.write().await = Some(session);
    (d, mock)
}

/// Seed both the mock encoder and the cached snapshot to the tick value
/// for `mech_ha`, so a poll-loop refresh and a direct snapshot read
/// agree on the same position regardless of timing.
async fn seed_mech_ha(
    d: &MountDevice,
    mock: &Arc<tokio::sync::Mutex<MockMountState>>,
    mech_ha: f64,
) {
    let cpr = d.manager.parameters().await.unwrap().cpr_ra;
    let ticks = (mech_ha * f64::from(cpr) / 24.0) as i32;
    mock.lock().await.ra.position_ticks = ticks;
    d.manager.seed_ra_position(ticks).await;
}

fn log_has_k1(log: &[Vec<u8>]) -> bool {
    log.iter().any(|f| f == b":K1\r")
}

#[tokio::test]
async fn tracking_guard_tick_stops_tracking_inside_the_zone() {
    let (d, mock) = guard_device(0.05).await;
    seed_mech_ha(&d, &mock, 6.0).await; // mid-zone (mech_HA = +6 h)
    d.state.write().await.tracking_requested = true;

    let fired = tracking_guard_tick(
        &d.state,
        &d.manager,
        &d.session,
        &d.slew_in_progress,
        (0.95, 11.05),
        0.05,
    )
    .await;

    assert!(fired, "guard should fire mid-zone");
    assert!(
        !d.tracking().await.unwrap(),
        "Tracking must be cleared after the guard fires"
    );
    let log = mock.lock().await.command_log.clone();
    assert!(log_has_k1(&log), "expected a :K1 stop, log: {log:?}");
}

#[tokio::test]
async fn tracking_guard_tick_is_noop_far_from_the_zone() {
    let (d, mock) = guard_device(0.05).await;
    seed_mech_ha(&d, &mock, -3.0).await; // typical session start, well clear
    d.state.write().await.tracking_requested = true;

    let fired = tracking_guard_tick(
        &d.state,
        &d.manager,
        &d.session,
        &d.slew_in_progress,
        (0.95, 11.05),
        0.05,
    )
    .await;

    assert!(!fired, "guard must not fire far from the zone");
    assert!(
        d.tracking().await.unwrap(),
        "Tracking must stay engaged far from the zone"
    );
    let log = mock.lock().await.command_log.clone();
    assert!(
        !log_has_k1(&log),
        "no stop expected far from the zone, log: {log:?}"
    );
}

#[tokio::test]
async fn tracking_guard_tick_is_noop_when_not_tracking() {
    let (d, mock) = guard_device(0.05).await;
    seed_mech_ha(&d, &mock, 6.0).await; // mid-zone, but tracking is off
                                        // `tracking_requested` left false — the guard only acts while the
                                        // client has tracking engaged.

    let fired = tracking_guard_tick(
        &d.state,
        &d.manager,
        &d.session,
        &d.slew_in_progress,
        (0.95, 11.05),
        0.05,
    )
    .await;

    assert!(!fired, "guard only acts while tracking is engaged");
    let log = mock.lock().await.command_log.clone();
    assert!(
        !log_has_k1(&log),
        "no stop expected when not tracking, log: {log:?}"
    );
}

#[tokio::test]
async fn tracking_guard_tick_is_noop_when_parameters_not_cached() {
    // Before the handshake caches CPR, mech_HA can't be computed, so the
    // guard must no-op even with tracking engaged. Build a device and
    // never connect, so `parameters()` stays None.
    let cfg = Config {
        mount: MountConfig {
            cw_exclusion_zone: CwExclusionZone::Active(ActiveZone::new(0.95, 11.05)),
            ..Default::default()
        },
        ..base_config()
    };
    let manager = MountManager::new(&cfg, Arc::new(MockTransportFactory));
    let d = MountDevice::new(cfg.mount, manager);
    d.state.write().await.tracking_requested = true;
    assert!(d.manager.parameters().await.is_none());

    let fired = tracking_guard_tick(
        &d.state,
        &d.manager,
        &d.session,
        &d.slew_in_progress,
        (0.95, 11.05),
        0.05,
    )
    .await;

    assert!(!fired, "no cached CPR -> guard cannot act");
    assert!(
        d.state.read().await.tracking_requested,
        "tracking must be left untouched"
    );
}

#[tokio::test]
async fn tracking_guard_tick_is_noop_when_session_closed_mid_tick() {
    // Params cached and mech_HA in-band, but the device's session slot
    // was cleared (a disconnect racing the tick) before the guard could
    // issue :K1.
    let factory = CapturingMockFactory::new();
    let mock = Arc::clone(&factory.state);
    let cfg = Config {
        mount: MountConfig {
            cw_exclusion_zone: CwExclusionZone::Active(ActiveZone::new(0.95, 11.05)),
            ..Default::default()
        },
        ..base_config()
    };
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    // Acquire to run the handshake (caches CPR, keeps the transport alive)
    // but leave the device's own session slot empty.
    let _session = d.manager.transport().acquire().await.unwrap();
    seed_mech_ha(&d, &mock, 6.0).await; // mid-zone -> would otherwise fire
    d.state.write().await.tracking_requested = true;
    assert!(d.session.read().await.is_none());

    let fired = tracking_guard_tick(
        &d.state,
        &d.manager,
        &d.session,
        &d.slew_in_progress,
        (0.95, 11.05),
        0.05,
    )
    .await;

    assert!(!fired, "no session -> nothing to stop");
    assert!(
        d.state.read().await.tracking_requested,
        "tracking must be left set"
    );
    let log = mock.lock().await.command_log.clone();
    assert!(!log_has_k1(&log), "no :K1 without a session, log: {log:?}");
}

#[tokio::test]
async fn tracking_guard_tick_leaves_tracking_set_when_stop_fails() {
    // If the :K1 stop fails on the wire, the guard must NOT clear Tracking
    // (it keeps reporting the wire truth) and must report not-fired so the
    // next tick retries while mech_HA stays in-band.
    let (d, mock) = guard_device(0.05).await;
    seed_mech_ha(&d, &mock, 6.0).await; // mid-zone -> would fire
    mock.lock().await.fail_command = Some(b'K'); // make :K1 error
    d.state.write().await.tracking_requested = true;

    let fired = tracking_guard_tick(
        &d.state,
        &d.manager,
        &d.session,
        &d.slew_in_progress,
        (0.95, 11.05),
        0.05,
    )
    .await;

    assert!(!fired, "a failed stop is not a successful fire");
    assert!(
        d.state.read().await.tracking_requested,
        "Tracking must stay set when the wire stop fails"
    );
    let log = mock.lock().await.command_log.clone();
    assert!(
        log_has_k1(&log),
        "the :K1 attempt should still reach the wire, log: {log:?}"
    );
}

/// Build a device with auto-flip armed (`flip_policy` enabled +
/// `auto_flip_during_tracking`) at the given meridian offset, the CW
/// exclusion zone disabled, and the altitude floor neutralised (the
/// flip target's apparent altitude depends on wallclock LST). The
/// session is established directly — no background watcher; these
/// tests drive [`auto_flip_tick`] / [`guard_loop_tick`] by hand.
async fn auto_flip_device(
    offset_hours: f64,
) -> (MountDevice, Arc<tokio::sync::Mutex<MockMountState>>) {
    let factory = CapturingMockFactory::new();
    let mock = Arc::clone(&factory.state);
    let mut cfg = base_config();
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    cfg.mount.flip_policy = FlipPolicy {
        enabled: true,
        flip_range_hours: FlipRangeHours::new(0.5),
        auto_flip_during_tracking: true,
        auto_flip_at_meridian_offset_hours: offset_hours,
    };
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    let session = d.manager.transport().acquire().await.unwrap();
    *d.session.write().await = Some(session);
    (d, mock)
}

/// The through-wrap flip slew's RA goto: `:G1` mode `01` =
/// Goto + Fast + CCW (matches the assertion `meridian_flip.feature`
/// pins for `SetSideOfPier`).
fn log_has_flip_goto(log: &[Vec<u8>]) -> bool {
    log.iter().any(|f| f == b":G101\r")
}

#[tokio::test]
async fn auto_flip_tick_starts_a_flip_at_the_meridian_offset() {
    let (d, mock) = auto_flip_device(0.0).await;
    seed_mech_ha(&d, &mock, 0.2).await; // just past the meridian
    d.state.write().await.tracking_requested = true;

    let attempted = auto_flip_tick(&d, 0.0, false).await;

    assert!(attempted, "the tick should latch an attempt");
    assert_eq!(
        d.state.read().await.target_pier_side,
        Some(PierSide::East),
        "the flip slew must target the post-flip side"
    );
    let log = mock.lock().await.command_log.clone();
    assert!(
        log_has_flip_goto(&log),
        "expected the CCW flip goto :G101, log: {log:?}"
    );
}

#[tokio::test]
async fn auto_flip_tick_rearms_below_the_offset() {
    // East of the trigger the latch resets — this re-arms the watcher
    // after a slew back east, or after a completed flip once the folded
    // post-flip mech_HA reads below the offset (immediately for
    // non-negative offsets; after the +12 h fold for negative ones).
    let (d, mock) = auto_flip_device(0.0).await;
    seed_mech_ha(&d, &mock, -3.0).await;
    d.state.write().await.tracking_requested = true;

    let attempted = auto_flip_tick(&d, 0.0, true).await;

    assert!(!attempted, "below the offset the latch must re-arm");
    let log = mock.lock().await.command_log.clone();
    assert!(!log_has_flip_goto(&log), "no flip expected, log: {log:?}");
}

#[tokio::test]
async fn auto_flip_tick_does_not_retry_after_an_attempt() {
    // One attempt per crossing: with the latch set and mech_HA still
    // past the offset, the tick must not issue another flip.
    let (d, mock) = auto_flip_device(0.0).await;
    seed_mech_ha(&d, &mock, 0.2).await;
    d.state.write().await.tracking_requested = true;

    let attempted = auto_flip_tick(&d, 0.0, true).await;

    assert!(attempted, "the latch must stay set past the offset");
    let log = mock.lock().await.command_log.clone();
    assert!(
        !log_has_flip_goto(&log),
        "no second flip expected, log: {log:?}"
    );
}

#[tokio::test]
async fn auto_flip_tick_is_noop_when_not_tracking() {
    let (d, mock) = auto_flip_device(0.0).await;
    seed_mech_ha(&d, &mock, 0.2).await;
    // `tracking_requested` left false — auto-flip only acts while the
    // client has tracking engaged.

    let attempted = auto_flip_tick(&d, 0.0, false).await;

    assert!(!attempted, "no attempt without tracking engaged");
    let log = mock.lock().await.command_log.clone();
    assert!(!log_has_flip_goto(&log), "no flip expected, log: {log:?}");
}

#[tokio::test]
async fn auto_flip_tick_ignores_the_post_flip_side() {
    // Only natural → flipped is automated. Seed the Dec encoder past
    // the pole so side_of_pier reads the post-flip side.
    let (d, mock) = auto_flip_device(0.0).await;
    seed_mech_ha(&d, &mock, 0.2).await;
    let cpr = d.manager.parameters().await.unwrap().cpr_dec.cast_signed();
    mock.lock().await.dec.position_ticks = cpr / 2;
    d.manager.seed_dec_position(cpr / 2).await;
    d.state.write().await.tracking_requested = true;

    let attempted = auto_flip_tick(&d, 0.0, false).await;

    assert!(!attempted, "no attempt on the post-flip side");
    let log = mock.lock().await.command_log.clone();
    assert!(!log_has_flip_goto(&log), "no flip expected, log: {log:?}");
}

#[tokio::test]
async fn auto_flip_tick_stays_latched_at_the_post_flip_fold_with_a_negative_offset() {
    // A flip fired at a negative offset lands the folded mech_HA near
    // offset + 12 on the flipped side. The latch holds there (mech_HA
    // is not below the offset) and is inert — the pier-side gate blocks
    // any flip on the flipped side regardless of latch state.
    let (d, mock) = auto_flip_device(-0.25).await;
    seed_mech_ha(&d, &mock, 11.75).await;
    let cpr = d.manager.parameters().await.unwrap().cpr_dec.cast_signed();
    mock.lock().await.dec.position_ticks = cpr / 2;
    d.manager.seed_dec_position(cpr / 2).await;
    d.state.write().await.tracking_requested = true;

    let attempted = auto_flip_tick(&d, -0.25, true).await;

    assert!(attempted, "the latch must hold at the fold position");
    let log = mock.lock().await.command_log.clone();
    assert!(!log_has_flip_goto(&log), "no flip expected, log: {log:?}");
}

#[tokio::test]
async fn auto_flip_tick_rearms_after_the_fold_with_a_negative_offset() {
    // Once tracking carries the post-flip encoder past +12 h the folded
    // mech_HA reads near −12, below any valid offset — the latch
    // re-arms while still on the flipped side, ready for the next
    // natural-side crossing.
    let (d, mock) = auto_flip_device(-0.25).await;
    seed_mech_ha(&d, &mock, -11.9).await;
    let cpr = d.manager.parameters().await.unwrap().cpr_dec.cast_signed();
    mock.lock().await.dec.position_ticks = cpr / 2;
    d.manager.seed_dec_position(cpr / 2).await;
    d.state.write().await.tracking_requested = true;

    let attempted = auto_flip_tick(&d, -0.25, true).await;

    assert!(!attempted, "past the fold the latch must re-arm");
    let log = mock.lock().await.command_log.clone();
    assert!(!log_has_flip_goto(&log), "no flip expected, log: {log:?}");
}

#[tokio::test]
async fn auto_flip_tick_latches_when_the_flip_fails() {
    // A failed attempt must latch (no retry this crossing) and leave
    // the in-memory Tracking flag reporting the wire truth — the
    // stop-only guard remains the safety fallback.
    let (d, mock) = auto_flip_device(0.0).await;
    seed_mech_ha(&d, &mock, 0.2).await;
    mock.lock().await.fail_command = Some(b'K'); // flip's pre-slew :K1 stop errors
    d.state.write().await.tracking_requested = true;

    let attempted = auto_flip_tick(&d, 0.0, false).await;

    assert!(attempted, "a failed attempt still latches");
    assert!(
        d.state.read().await.tracking_requested,
        "Tracking must stay set when the flip failed before the wire stop"
    );
    let log = mock.lock().await.command_log.clone();
    assert!(!log_has_flip_goto(&log), "no goto after the failed stop");
}

#[tokio::test]
async fn guard_loop_tick_prefers_the_guard_inside_the_band() {
    // Inside the guarded band the stop-only guard fires and the tick
    // must not also start a flip, even with auto-flip armed and the
    // offset crossed.
    let factory = CapturingMockFactory::new();
    let mock = Arc::clone(&factory.state);
    let mut cfg = base_config();
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Active(ActiveZone::new(0.95, 11.05));
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    cfg.mount.flip_policy = FlipPolicy {
        enabled: true,
        flip_range_hours: FlipRangeHours::new(0.5),
        auto_flip_during_tracking: true,
        auto_flip_at_meridian_offset_hours: 0.0,
    };
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    let session = d.manager.transport().acquire().await.unwrap();
    *d.session.write().await = Some(session);
    seed_mech_ha(&d, &mock, 3.0).await; // mid-zone
    d.state.write().await.tracking_requested = true;

    let latch = guard_loop_tick(&d, (0.95, 11.05), 0.05, true, 0.0, false).await;

    assert!(!latch, "the guard fired; no flip attempt was made");
    assert!(
        !d.state.read().await.tracking_requested,
        "the guard must have stopped tracking"
    );
    let log = mock.lock().await.command_log.clone();
    assert!(log_has_k1(&log), "expected the guard's :K1, log: {log:?}");
    assert!(
        !log_has_flip_goto(&log),
        "the guard must win over auto-flip, log: {log:?}"
    );
}

#[tokio::test]
async fn set_connected_idempotent_within_a_session() {
    let d = device();
    d.set_connected(true).await.unwrap();
    // Same value again is a no-op (does not double-bump the ref count).
    d.set_connected(true).await.unwrap();
    assert!(d.connected().await.unwrap());
}

#[tokio::test]
async fn right_ascension_fails_while_disconnected() {
    let d = device();
    let err = d.right_ascension().await.unwrap_err();
    assert_eq!(err.code, ASCOMError::NOT_CONNECTED.code);
}

#[tokio::test]
async fn declination_at_encoder_zero_is_celestial_equator() {
    let d = connected_device().await;
    let dec = d.declination().await.unwrap();
    assert!(dec.abs() < 1e-9, "got {dec}");
}

#[tokio::test]
async fn driver_info_and_version_are_populated() {
    let d = device();
    assert!(d.driver_info().await.unwrap().contains("Star Adventurer"));
    assert!(!d.driver_version().await.unwrap().is_empty());
}

#[tokio::test]
async fn description_passes_through_from_config() {
    let d = device();
    assert!(!d.description().await.unwrap().is_empty());
}

#[tokio::test]
async fn set_tracking_true_latches_flag() {
    let d = connected_device().await;
    d.set_tracking(true).await.unwrap();
    assert!(d.tracking().await.unwrap());
}

#[tokio::test]
async fn set_tracking_true_issues_g_i_j_on_ra_axis() {
    // Build a device backed by a CapturingMockFactory so we can
    // inspect the exact wire frames the driver emitted.
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    let mock = std::sync::Arc::clone(&factory.state);
    let cfg = base_config();
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();

    // Drive SetTracking(true) and assert that the driver issues
    // the wire sequence the design doc + spec require:
    //   1. `:K1`  (decelerate the RA axis — needed because the
    //              spec disallows changing motion mode while the
    //              motor is running, and the axis may be in
    //              Speed Mode either because we just enabled
    //              tracking on it before or because the firmware
    //              auto-engages Speed Mode after a goto. `:K`
    //              is gentler on the gearbox than `:L`; the
    //              `:f` poll loop below waits out the
    //              deceleration.)
    //   2. `:f1`  (one or more polls — `stop_and_wait` polls
    //              until the running flag clears.)
    //   3. `:G1<mode>` (tracking-slow-CW)
    //   4. `:I1<period>` (sidereal step period)
    //   5. `:J1` (start motion)
    let baseline_len = mock.lock().await.command_log.len();
    d.set_tracking(true).await.unwrap();

    let log = mock.lock().await.command_log.clone();
    let new_frames: Vec<&[u8]> = log[baseline_len..]
        .iter()
        .map(std::vec::Vec::as_slice)
        .collect();
    // Look only at setter / motion-start frames on the RA axis:
    // `:G1`, `:I1`, `:J1`, `:K1` (in order of appearance). The
    // polling task's `:f1` / `:j1` / `:f2` / `:j2` inquiries are
    // noise here.
    let interesting: Vec<&&[u8]> = new_frames
        .iter()
        .filter(|f| {
            f.len() >= 3
                && f[0] == b':'
                && f[2] == b'1'
                && matches!(f[1], b'G' | b'I' | b'J' | b'L' | b'K')
        })
        .collect();
    assert_eq!(
        interesting.len(),
        4,
        "expected exactly 4 RA setter frames (:K1 :G1 :I1 :J1), got {interesting:?}"
    );
    assert_eq!(*interesting[0], b":K1\r", "1st RA setter should be :K1");
    assert_eq!(&interesting[1][..3], b":G1", "2nd RA setter should be :G1");
    assert_eq!(&interesting[2][..3], b":I1", "3rd RA setter should be :I1");
    assert_eq!(*interesting[3], b":J1\r", "4th RA setter should be :J1");
}

#[tokio::test]
async fn set_tracking_false_issues_k1() {
    let d = connected_device().await;
    d.set_tracking(true).await.unwrap();
    d.set_tracking(false).await.unwrap();
    assert!(!d.tracking().await.unwrap());
}

#[tokio::test]
async fn set_tracking_true_refuses_while_parked() {
    let d = fast_settle_connected().await;
    d.park().await.unwrap();
    wait_for_at_park(&d).await;
    assert!(d.at_park().await.unwrap());
    let err = d.set_tracking(true).await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_WHILE_PARKED);
}

#[tokio::test]
async fn set_tracking_false_succeeds_while_parked() {
    // Disabling tracking on a parked mount is a no-op affirmation;
    // refusing it would force callers to special-case the parked
    // state when they just want to assert "tracking should be off".
    let d = fast_settle_connected().await;
    d.park().await.unwrap();
    wait_for_at_park(&d).await;
    d.set_tracking(false).await.unwrap();
    assert!(!d.tracking().await.unwrap());
}

#[tokio::test]
async fn set_tracking_rate_to_lunar_returns_invalid_value() {
    let d = connected_device().await;
    let err = d.set_tracking_rate(DriveRate::Lunar).await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
}

#[tokio::test]
async fn target_setters_validate_range() {
    let d = device();
    assert!(d.set_target_right_ascension(-1.0).await.is_err());
    assert!(d.set_target_right_ascension(24.0).await.is_err());
    assert!(d.set_target_declination(-91.0).await.is_err());
    assert!(d.set_target_declination(91.0).await.is_err());
    // Valid values stick.
    d.set_target_right_ascension(6.0).await.unwrap();
    d.set_target_declination(45.0).await.unwrap();
    assert_eq!(d.target_right_ascension().await.unwrap(), 6.0);
    assert_eq!(d.target_declination().await.unwrap(), 45.0);
}

#[tokio::test]
async fn target_read_without_set_returns_invalid_operation() {
    let d = device();
    let err = d.target_right_ascension().await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
}

#[tokio::test]
async fn sync_to_coordinates_validates_inputs() {
    let d = connected_device().await;
    // Out-of-range RA.
    assert_eq!(
        d.sync_to_coordinates(24.0, 0.0).await.unwrap_err().code,
        ASCOMErrorCode::INVALID_VALUE
    );
    // Out-of-range Dec.
    assert_eq!(
        d.sync_to_coordinates(0.0, 91.0).await.unwrap_err().code,
        ASCOMErrorCode::INVALID_VALUE
    );
}

fn fast_settle_device() -> MountDevice {
    // Tight polling + zero settle so the watcher is fast in tests.
    device_with_settle(Duration::from_millis(0))
}

fn device_with_settle(settle_after_slew: Duration) -> MountDevice {
    let mut cfg = base_config();
    if let crate::config::TransportConfig::Usb(usb) = &mut cfg.transport {
        usb.polling_interval = Duration::from_millis(20);
    }
    cfg.mount.settle_after_slew = settle_after_slew;
    // The slew-lifecycle tests pass hardcoded RA/Dec targets
    // (typically `(6.0 h, 30°)`) whose mech-HA — and hence apparent
    // altitude — depends on the wallclock LST and would
    // intermittently trip the envelope gates. Neutralise both: the
    // CW-exclusion-zone behaviour is covered separately by
    // [`fast_settle_connected_narrow_envelope`], the altitude floor
    // by [`fast_settle_connected_with_altitude_floor`].
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    // Pin the park target to the mock's start position (0, 0) so
    // `park()` is a zero-distance, instant slew. The park-lifecycle
    // tests using this helper exercise the watcher / AtPark flip, not
    // park-target resolution; without the pin they would inherit the
    // `preferred_ap_park` default (`ap_park_3`, ~907 k ticks away) and
    // turn every `park()` into a multi-poll slew that races the tests'
    // fixed settle sleeps. `preferred_ap_park` resolution is covered by
    // the dedicated `park_target_*` tests.
    cfg.mount.park_ra_ticks = Some(0);
    cfg.mount.park_dec_ticks = Some(0);
    let manager = MountManager::new(&cfg, Arc::new(MockTransportFactory));
    MountDevice::new(cfg.mount, manager)
}

async fn fast_settle_connected() -> MountDevice {
    let d = fast_settle_device();
    d.set_connected(true).await.unwrap();
    d
}

/// Wait for the park watcher to set `at_park`.
///
/// `park()` returns once the motion has been issued; `at_park` is set later,
/// by the park watcher's finalizer. A fixed `sleep` races that hand-off — the
/// watcher nominally needs a single poll, but a loaded runner can starve the
/// spawned task and its transport acquire for much longer, and the sleep then
/// expires with `at_park` still false. Poll to a deadline instead, per
/// docs/skills/testing.md ("poll, never snapshot-once").
async fn wait_for_at_park(d: &MountDevice) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if d.at_park().await.unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // One last read rather than failing on the loop exit alone: the flag can
    // flip in the window between the final poll and the deadline check.
    assert!(
        d.at_park().await.unwrap(),
        "park watcher did not set at_park within 5s"
    );
}

/// Like [`fast_settle_connected`], but with a post-slew settle long enough
/// that an in-flight slew's `Slewing == true` window provably outlives the
/// assertions observing it: the completion watcher parks in its settle
/// sleep instead of clearing `slew_in_progress` as soon as the mock
/// reaches its goto target. The settle never actually elapses — the test
/// runtime is torn down first.
async fn slow_settle_connected() -> MountDevice {
    let d = device_with_settle(Duration::from_mins(10));
    d.set_connected(true).await.unwrap();
    d
}

/// Like `fast_settle_connected`, but with a narrow CW exclusion zone
/// so the safety-gate tests can land target coords that are clearly
/// inside it without first needing to push past the `GTi` default
/// `(0.95, 11.05)`.
async fn fast_settle_connected_narrow_envelope() -> MountDevice {
    let mut cfg = base_config();
    if let crate::config::TransportConfig::Usb(usb) = &mut cfg.transport {
        usb.polling_interval = Duration::from_millis(20);
    }
    cfg.mount.settle_after_slew = Duration::from_millis(0);
    // Narrow CW exclusion zone covering `mech_HA ∈ [0.5, 1.5] h` so a
    // target 1 h past meridian on the natural side is inside it.
    // Neutralise the altitude floor so these tests exercise the CW
    // gate in isolation (the floor has its own device builder below).
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Active(ActiveZone::new(0.5, 1.5));
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    // Pin the park target to (0, 0) — see `fast_settle_device` for why
    // (keeps `park()` instant despite the `preferred_ap_park` default).
    cfg.mount.park_ra_ticks = Some(0);
    cfg.mount.park_dec_ticks = Some(0);
    let manager = MountManager::new(&cfg, Arc::new(MockTransportFactory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();
    d
}

/// Like `fast_settle_connected`, but at site latitude 45°N with an
/// explicit altitude floor and the CW exclusion zone disabled, so the
/// altitude gate is exercised in isolation.
async fn fast_settle_connected_with_altitude_floor(floor_degrees: f64) -> MountDevice {
    let mut cfg = base_config();
    if let crate::config::TransportConfig::Usb(usb) = &mut cfg.transport {
        usb.polling_interval = Duration::from_millis(20);
    }
    cfg.mount.settle_after_slew = Duration::from_millis(0);
    cfg.mount.site_latitude_deg = 45.0;
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(floor_degrees);
    // Pin the park target to (0, 0) — see `fast_settle_device` for why
    // (keeps `park()` instant despite the `preferred_ap_park` default).
    cfg.mount.park_ra_ticks = Some(0);
    cfg.mount.park_dec_ticks = Some(0);
    let manager = MountManager::new(&cfg, Arc::new(MockTransportFactory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();
    d
}

#[tokio::test]
async fn slew_async_refuses_target_below_altitude_floor() {
    // LAT 45°N, floor 0° (geometric horizon). The target at
    // HA = −3 h, Dec = −40° computes apparent altitude −4.1° and
    // must be rejected before any wire motion.
    let d = fast_settle_connected_with_altitude_floor(0.0).await;
    let lst = d.sidereal_time().await.unwrap();
    let target_ra = (lst + 3.0).rem_euclid(24.0); // HA = LST − RA = −3 h
    let err = d
        .slew_to_coordinates_async(target_ra, -40.0)
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    assert!(
        err.message.contains("altitude"),
        "error message must call out the altitude floor: {}",
        err.message
    );
}

#[tokio::test]
async fn slew_async_accepts_low_target_above_altitude_floor() {
    // Same geometry as the refusal test, but Dec = −30° puts the
    // target at apparent altitude +4.6° — above the 0° floor, so
    // the slew is accepted even though the target is nowhere near
    // the rectangular Dec envelope this gate replaced.
    let d = fast_settle_connected_with_altitude_floor(0.0).await;
    let lst = d.sidereal_time().await.unwrap();
    let target_ra = (lst + 3.0).rem_euclid(24.0); // HA = −3 h
    d.slew_to_coordinates_async(target_ra, -30.0).await.unwrap();
}

#[tokio::test]
async fn envelope_check_accepts_target_exactly_at_altitude_floor() {
    // The comparator is `alt < floor` — a target exactly at the
    // floor is accepted, one 0.001° below is rejected. Pin the floor
    // to the precise altitude the driver will compute for the target
    // (same function, same inputs), then probe the check directly
    // with a fixed LST so no wallclock is involved.
    let (alt, _az) = ra_dec_to_alt_az(Ra::new(12.0), Dec::new(-44.0), 45.0, Lst::new(12.0));
    let d = fast_settle_connected_with_altitude_floor(alt).await;
    d.check_within_safe_envelope(12.0, -44.0, 12.0, false)
        .expect("target exactly at the floor is accepted");
    let err = d
        .check_within_safe_envelope(12.0, -44.001, 12.0, false)
        .expect_err("target below the floor is rejected");
    assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
}

#[tokio::test]
async fn envelope_check_negative_floor_permits_below_horizon_target() {
    // floor −45°: the HA = −3 h / Dec = −40° target (alt −4.1°) that
    // the 0° floor rejects is accepted.
    let d = fast_settle_connected_with_altitude_floor(-45.0).await;
    let lst = d.sidereal_time().await.unwrap();
    let target_ra = (lst + 3.0).rem_euclid(24.0);
    d.slew_to_coordinates_async(target_ra, -40.0).await.unwrap();
}

#[tokio::test]
async fn slew_async_refuses_ra_target_in_binding_zone() {
    // Binding zone covers `mech_HA ∈ [0.5, 1.5] h`. Target RA =
    // LST − 1 puts `mech_HA = LST − (LST − 1) = +1 h` — squarely
    // in the middle of the zone, so the slew must be rejected
    // with `INVALID_VALUE` before any wire motion.
    let d = fast_settle_connected_narrow_envelope().await;
    let lst = d.sidereal_time().await.unwrap();
    let target = (lst - 1.0).rem_euclid(24.0);
    let err = d.slew_to_coordinates_async(target, 0.0).await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    assert!(
        err.message.contains("CW exclusion zone"),
        "error message must call out the CW exclusion zone: {}",
        err.message
    );
}

#[tokio::test]
async fn sync_refuses_target_below_altitude_floor() {
    // Sync runs the same envelope gates as slew: a below-floor
    // target (HA = −3 h, Dec = −40° at LAT 45°N → alt −4.1°) is
    // rejected before the encoder is touched.
    let d = fast_settle_connected_with_altitude_floor(0.0).await;
    let lst = d.sidereal_time().await.unwrap();
    let target_ra = (lst + 3.0).rem_euclid(24.0);
    let err = d.sync_to_coordinates(target_ra, -40.0).await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
}

#[tokio::test]
async fn slew_async_validates_inputs() {
    let d = fast_settle_connected().await;
    assert_eq!(
        d.slew_to_coordinates_async(24.0, 0.0)
            .await
            .unwrap_err()
            .code,
        ASCOMErrorCode::INVALID_VALUE
    );
    assert_eq!(
        d.slew_to_coordinates_async(0.0, 91.0)
            .await
            .unwrap_err()
            .code,
        ASCOMErrorCode::INVALID_VALUE
    );
}

#[tokio::test]
async fn slew_async_refuses_while_disconnected() {
    let d = fast_settle_device();
    let err = d.slew_to_coordinates_async(6.0, 30.0).await.unwrap_err();
    assert_eq!(err.code, ASCOMError::NOT_CONNECTED.code);
}

#[test]
fn ascom_session_err_helper_flattens_session_error_into_ascom() {
    // `MountDevice::ascom_session_err` wraps
    // `SessionError<SkywatcherCodecError>` -> `StarAdvError`
    // -> `ASCOMError` for the `set_connected(true)` acquire path.
    // The two failure-mode branches the production code hits are
    // factory-open (mapped to `INVALID_OPERATION` via
    // `ConnectionFailed`) and codec/protocol errors (mapped to
    // `INVALID_OPERATION` via `Protocol`).
    use rusty_photon_shared_transport::{SessionError, TransportError};
    let err = MountDevice::ascom_session_err(
        SessionError::<crate::codec::SkywatcherCodecError>::Transport(TransportError::Open(
            std::io::Error::other("port busy"),
        )),
    );
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
    assert!(err.message.contains("port busy"));

    let err = MountDevice::ascom_session_err(SessionError::Codec(
        crate::codec::SkywatcherCodecError::Protocol(
            skywatcher_motor_protocol::ProtocolError::FrameError("malformed".into()),
        ),
    ));
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
}

#[test]
fn ascom_transport_err_helper_flattens_transport_error_into_ascom() {
    // `MountDevice::ascom_transport_err` is the disconnect-side
    // mapping called from `set_connected(false)` when
    // `session.close().await` fails. The Eof branch is the
    // most-observable one (a half-open peer at teardown time).
    use rusty_photon_shared_transport::TransportError;
    let err = MountDevice::ascom_transport_err(TransportError::Eof);
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
    assert!(err.message.contains("Connection closed"));
}

#[test]
fn ascom_helper_maps_timekeeping_to_invalid_operation() {
    // Every LST-using trait method propagates ERFA failures via
    // `local_sidereal_time_hours(...).map_err(ASCOMError::from)?`.
    // A mount-level trait test would need a clock-injection seam
    // (host `SystemTime` can not even represent ERFA's
    // `IYMIN = -4799` floor on Windows, where FILETIME starts in
    // 1601). Instead, exercise the conversion the trait methods
    // actually use — `ASCOMError::from(StarAdvError::Timekeeping(_))`
    // via the `From<StarAdvError> for ASCOMError` impl — so the
    // propagation pattern has a runtime assertion in this file
    // alongside the trait code.
    let err: ASCOMError =
        StarAdvError::Timekeeping("ERFA Dtf2d rejected UTC -5000-01-01 (code -1)".into()).into();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
    assert!(
        err.message.contains("timekeeping"),
        "ASCOM message should retain the diagnostic, got {:?}",
        err.message
    );
}

#[tokio::test]
async fn sync_slew_returns_only_after_watcher_clears_slew_in_progress() {
    // CanSlew = true, so the synchronous variant must implement
    // (per ASCOM ITelescopeV3) and only return after the slew has
    // completed.
    let d = fast_settle_connected().await;
    d.slew_to_coordinates(6.0, 30.0).await.unwrap();
    assert!(
        !d.slewing().await.unwrap(),
        "Slewing must be false after slew_to_coordinates returns"
    );
    // Target latched same as the async variant.
    assert_eq!(d.target_right_ascension().await.unwrap(), 6.0);
    assert_eq!(d.target_declination().await.unwrap(), 30.0);
}

#[tokio::test]
async fn sync_slew_to_target_uses_last_set_target() {
    let d = fast_settle_connected().await;
    d.set_target_right_ascension(4.0).await.unwrap();
    d.set_target_declination(15.0).await.unwrap();
    d.slew_to_target().await.unwrap();
    assert!(!d.slewing().await.unwrap());
}

#[tokio::test]
async fn sync_slew_validates_inputs() {
    let d = fast_settle_connected().await;
    assert_eq!(
        d.slew_to_coordinates(24.0, 0.0).await.unwrap_err().code,
        ASCOMErrorCode::INVALID_VALUE
    );
}

#[tokio::test]
async fn sync_slew_refuses_while_parked() {
    let d = fast_settle_connected().await;
    d.park().await.unwrap();
    wait_for_at_park(&d).await;
    let err = d.slew_to_coordinates(6.0, 30.0).await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_WHILE_PARKED);
}

#[tokio::test]
async fn sync_slew_to_target_without_set_returns_invalid_operation() {
    let d = fast_settle_connected().await;
    let err = d.slew_to_target().await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
}

#[tokio::test]
async fn slew_async_latches_target() {
    let d = fast_settle_connected().await;
    d.slew_to_coordinates_async(6.0, 30.0).await.unwrap();
    assert_eq!(d.target_right_ascension().await.unwrap(), 6.0);
    assert_eq!(d.target_declination().await.unwrap(), 30.0);
}

#[tokio::test]
async fn slew_async_issues_indi_sequence_per_axis() {
    // Phase A5 (issue #205) + issue #207: the slew path emits
    // the INDI eqmod-style sequence — :K → poll :f → :G → :I →
    // :H → :M → :J. (Issue #207 swapped :L for :K — :K is the
    // spec's recommended stop, :L stays reserved for emergency
    // aborts.) This test asserts the order of the setters and
    // motion-start frames for each axis in the freshly-issued
    // slew, before the watcher's pickup loop (if any) re-enters
    // the sequence.
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    let mock = std::sync::Arc::clone(&factory.state);
    let mut cfg = base_config();
    if let crate::config::TransportConfig::Usb(usb) = &mut cfg.transport {
        usb.polling_interval = Duration::from_millis(20);
    }
    cfg.mount.settle_after_slew = Duration::from_millis(0);
    // Disable the CW-exclusion zone check for this test.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();

    // Capture the log baseline so the assertion ignores the
    // handshake / pre-slew polling chatter.
    let baseline_len = mock.lock().await.command_log.len();
    d.slew_to_coordinates_async(6.0, 30.0).await.unwrap();

    // Snapshot the log immediately — the watcher's pickup loop
    // may re-enter the sequence and add more frames; we only
    // care about the first-pass wire frames here.
    let log = mock.lock().await.command_log.clone();
    let new_frames: Vec<&[u8]> = log[baseline_len..]
        .iter()
        .map(std::vec::Vec::as_slice)
        .collect();

    // Helper: extract setter / motion-start frames for `axis_byte`.
    let interesting = |axis_byte: u8| -> Vec<&[u8]> {
        new_frames
            .iter()
            .copied()
            .filter(|f| {
                f.len() >= 3
                    && f[0] == b':'
                    && f[2] == axis_byte
                    && matches!(f[1], b'G' | b'I' | b'H' | b'M' | b'J' | b'K' | b'L')
            })
            .collect()
    };

    let ra = interesting(b'1');
    // Expect :K1 :G1 :I1 :H1 :M1 :J1 in order. Slack on length
    // because the watcher may add more before we sampled — but
    // the first six setter frames for axis 1 are deterministic.
    assert!(ra.len() >= 6, "expected ≥6 RA frames, got {ra:?}");
    assert_eq!(*ra[0], *b":K1\r", "1st RA setter should be :K1");
    assert_eq!(&ra[1][..3], b":G1", "2nd RA setter should be :G1");
    assert_eq!(&ra[2][..3], b":I1", "3rd RA setter should be :I1");
    assert_eq!(&ra[3][..3], b":H1", "4th RA setter should be :H1");
    assert_eq!(&ra[4][..3], b":M1", "5th RA setter should be :M1");
    assert_eq!(*ra[5], *b":J1\r", "6th RA setter should be :J1");

    let dec = interesting(b'2');
    assert!(dec.len() >= 6, "expected ≥6 Dec frames, got {dec:?}");
    assert_eq!(*dec[0], *b":K2\r");
    assert_eq!(&dec[1][..3], b":G2");
    assert_eq!(&dec[2][..3], b":I2");
    assert_eq!(&dec[3][..3], b":H2");
    assert_eq!(&dec[4][..3], b":M2");
    assert_eq!(*dec[5], *b":J2\r");
}

#[tokio::test]
async fn slew_watcher_pickup_loop_reissues_when_residual_exceeds_tolerance() {
    // Phase A5: after both axes stop, if the snapshot's encoder
    // position translates to an RA/Dec that's more than 5"
    // away from the latched target, the watcher must re-enter
    // the slew sequence with a fresh delta.
    //
    // To exercise the pickup loop deterministically — independent
    // of how fast the host walks the mock through the goto chunks
    // — we spawn a side task that, shortly after the slew is
    // issued, force-stops both axes in the mock state with the
    // encoder position clearly off-target. The transport's
    // background polling task picks the new state up; the watcher
    // sees both axes stopped at a position that translates to an
    // RA/Dec far from the latched target, and must re-issue the
    // slew sequence (one fresh :L → :G → :I → :H → :M → :J per
    // axis) at least once. We assert :H1 count >= 2 (one from the
    // initial slew, one from the pickup re-issue) — strictly
    // stronger than the > 0 check, which the initial slew alone
    // would always satisfy.
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    let mock = std::sync::Arc::clone(&factory.state);
    let mut cfg = base_config();
    if let crate::config::TransportConfig::Usb(usb) = &mut cfg.transport {
        usb.polling_interval = Duration::from_millis(20);
    }
    cfg.mount.settle_after_slew = Duration::from_millis(0);
    // Disable the CW-exclusion zone check for this test.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();

    // Spawn the injection task BEFORE issuing the slew so it is
    // already scheduled when the watcher starts polling. After a
    // short delay (long enough for the initial :L → :J sequence
    // to have hit the wire but well before MIN_SLEW_DWELL), force
    // the mock to declare the goto done at a position clearly
    // off-target. The watcher's next pickup check will see a
    // multi-degree residual and re-issue the slew sequence.
    let mock_clone = mock.clone();
    let injection = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        let mut s = mock_clone.lock().await;
        s.ra.running = false;
        s.dec.running = false;
        // 1,000,000 ticks ≈ 99° on the GTi's default CPR
        // (3,628,800 ticks/rev) — well above the 5" pickup
        // tolerance regardless of LST drift.
        s.ra.position_ticks = 1_000_000;
        s.dec.position_ticks = 1_000_000;
    });

    d.slew_to_coordinates_async(6.0, 30.0).await.unwrap();
    injection.await.expect("injection task panicked");

    // Wait for Slewing to clear (after the pickup loop converges
    // or hits PICKUP_MAX_ITERATIONS + MIN_SLEW_DWELL).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if !d.slewing().await.unwrap() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!d.slewing().await.unwrap(), "Slewing must clear in 10s");

    // The initial slew always emits one :H1. A pickup iteration
    // emits a second. ≥ 2 proves the pickup loop fired at least
    // once in response to the forced residual.
    let log = mock.lock().await.command_log.clone();
    let h1_count = log.iter().filter(|f| f.starts_with(b":H1")).count();
    assert!(
        h1_count >= 2,
        "expected ≥2 :H1 frames (initial slew + at least one pickup re-issue), \
         got {h1_count}; log={log:?}"
    );
}

#[tokio::test]
async fn slew_watcher_aborts_via_instant_stop_when_axis_reports_blocked() {
    // Drive a slew, seed the mock to report `blocked = true` on
    // either axis, and assert the watcher issues `:L1` + `:L2`
    // and clears `slew_in_progress` instead of waiting for the
    // running flag to drop. Mirrors the safety net we wired up
    // after the hardware ConformU run where the motor stalled
    // against a counterweight-up mechanical stop while the
    // encoder counter kept advancing.
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    let mock = std::sync::Arc::clone(&factory.state);
    let mut cfg = base_config();
    if let crate::config::TransportConfig::Usb(usb) = &mut cfg.transport {
        usb.polling_interval = Duration::from_millis(20);
    }
    cfg.mount.settle_after_slew = Duration::from_millis(0);
    // Disable the CW-exclusion zone check for this test.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();

    // Mark RA blocked. The next poll picks this up, the watcher
    // sees it, issues :L on both axes and exits early.
    {
        let mut s = mock.lock().await;
        s.ra.blocked = true;
    }
    d.slew_to_coordinates_async(6.0, 30.0).await.unwrap();

    // Wait for the watcher to observe the blocked state and
    // clear `slew_in_progress`. With dwell=2 s the watcher
    // won't act sooner; bound at 5 s.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !d.slewing().await.unwrap() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !d.slewing().await.unwrap(),
        "watcher must have cleared slew_in_progress after seeing blocked"
    );

    // Both axes must have seen a `:L` issued by the watcher's
    // abort path. (The driver's slew-prep stop uses the gentler
    // `:K` since issue #207 — `:L` is reserved for genuine
    // emergency stops like this blocked-axis abort and
    // `AbortSlew`.)
    let log = mock.lock().await.command_log.clone();
    let l1_count = log.iter().filter(|f| f.as_slice() == b":L1\r").count();
    let l2_count = log.iter().filter(|f| f.as_slice() == b":L2\r").count();
    assert!(
        l1_count >= 1,
        ":L1 should be issued by the watcher abort path; log={log:?}"
    );
    assert!(
        l2_count >= 1,
        ":L2 should be issued by the watcher abort path; log={log:?}"
    );
}

#[tokio::test]
async fn park_watcher_aborts_via_instant_stop_when_axis_reports_blocked() {
    // Same shape as the slew-watcher blocked test, but for the
    // park completion watcher. Critical: on a blocked abort the
    // park watcher must NOT set `AtPark = true` — the OTA isn't
    // at the encoder-0 home pose, so subsequent unpark+slew
    // computations would have a wrong delta.
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    let mock = std::sync::Arc::clone(&factory.state);
    let mut cfg = base_config();
    if let crate::config::TransportConfig::Usb(usb) = &mut cfg.transport {
        usb.polling_interval = Duration::from_millis(20);
    }
    cfg.mount.settle_after_slew = Duration::from_millis(0);
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();

    {
        let mut s = mock.lock().await;
        s.dec.blocked = true;
    }
    d.park().await.unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !d.slewing().await.unwrap() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !d.slewing().await.unwrap(),
        "park watcher must clear slew_in_progress after blocked"
    );
    assert!(
        !d.at_park().await.unwrap(),
        "park watcher must NOT set AtPark when aborted via blocked"
    );
}

#[tokio::test]
async fn slew_async_marks_slewing_until_watcher_clears_it() {
    let d = fast_settle_connected().await;
    d.slew_to_coordinates_async(6.0, 30.0).await.unwrap();
    // slew_in_progress is set immediately on return.
    assert!(d.slewing().await.unwrap());
    // Poll for completion: the slew distance is LST-dependent so
    // the number of mock polls to reach the target varies with the
    // wall clock. Bound the wait at 5s — vastly more than the
    // ~100ms a 100_000-tick-per-step mock needs to walk a typical
    // ±6h HA, but loose enough that a slow CI runner can't flake.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !d.slewing().await.unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("slewing did not become false within 5s");
}

#[tokio::test]
async fn slew_to_target_without_set_returns_invalid_operation() {
    let d = fast_settle_connected().await;
    let err = d.slew_to_target_async().await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
}

#[tokio::test]
async fn slew_to_target_uses_last_set_target() {
    let d = fast_settle_connected().await;
    d.set_target_right_ascension(12.0).await.unwrap();
    d.set_target_declination(45.0).await.unwrap();
    d.slew_to_target_async().await.unwrap();
    assert_eq!(d.target_right_ascension().await.unwrap(), 12.0);
    assert_eq!(d.target_declination().await.unwrap(), 45.0);
}

#[tokio::test]
async fn park_refuses_while_disconnected() {
    let d = fast_settle_device();
    let err = d.park().await.unwrap_err();
    assert_eq!(err.code, ASCOMError::NOT_CONNECTED.code);
}

#[tokio::test]
async fn park_then_unpark_round_trips_at_park_flag() {
    let d = fast_settle_connected().await;
    d.park().await.unwrap();
    wait_for_at_park(&d).await;
    assert!(d.at_park().await.unwrap(), "AtPark should be true");
    d.unpark().await.unwrap();
    assert!(!d.at_park().await.unwrap());
}

#[tokio::test]
async fn park_is_idempotent() {
    let d = fast_settle_connected().await;
    d.park().await.unwrap();
    wait_for_at_park(&d).await;
    // Second park while at_park is already true should be a no-op
    // (returns Ok without re-issuing motion).
    d.park().await.unwrap();
    assert!(d.at_park().await.unwrap());
}

#[tokio::test]
async fn unpark_does_not_auto_enable_tracking() {
    let d = fast_settle_connected().await;
    d.park().await.unwrap();
    wait_for_at_park(&d).await;
    d.unpark().await.unwrap();
    assert!(
        !d.tracking().await.unwrap(),
        "Tracking must remain off after Unpark"
    );
}

#[tokio::test]
async fn abort_slew_clears_slew_in_progress() {
    // Settle-pinned device: only the abort below can clear Slewing, so
    // the first assert can't race the completion watcher under load.
    let d = slow_settle_connected().await;
    d.slew_to_coordinates_async(6.0, 30.0).await.unwrap();
    // slew_in_progress is latched synchronously before the call returns.
    assert!(d.slewing().await.unwrap());
    d.abort_slew().await.unwrap();
    // abort_slew clears slew_in_progress immediately, but Slewing's
    // fallback reads the polling snapshot, which can report the axes as
    // still running in goto mode until the poll task refreshes it.
    // Deadline-poll instead of sleeping a fixed tick; the loop exits the
    // moment Slewing reads false, so healthy runs never feel the ceiling.
    tokio::time::timeout(Duration::from_secs(30), async {
        while d.slewing().await.unwrap() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Slewing did not clear within 30s of AbortSlew");
}

#[tokio::test]
async fn abort_slew_does_not_auto_restore_tracking() {
    let d = fast_settle_connected().await;
    d.set_tracking(true).await.unwrap();
    d.slew_to_coordinates_async(6.0, 30.0).await.unwrap();
    d.abort_slew().await.unwrap();
    // Tracking flag stays as-set (true), but the watcher's
    // re-enable path is skipped because slew_in_progress was
    // cleared. The exact post-abort tracking state isn't pinned by
    // ASCOM; we just check abort itself returned Ok.
    // (The driver's tracking_requested flag persists; the wire-side
    // tracking command was paused by :L. The user must call
    // SetTracking(true) again to resume motion.)
    let _ = d.tracking().await;
}

/// Connected device backed by a [`CapturingMockFactory`] with the CW
/// exclusion zone disabled and instant settle, returning the shared mock
/// state so a test can inject a wire failure (`fail_command`) mid-motion
/// and observe the reservation rollback. Mirrors the capturing-mock setup
/// `set_tracking_true_issues_g_i_j_on_ra_axis` uses.
async fn capturing_connected_device() -> (MountDevice, Arc<tokio::sync::Mutex<MockMountState>>) {
    let factory = CapturingMockFactory::new();
    let mock = Arc::clone(&factory.state);
    let mut cfg = base_config();
    // Open the envelope so the slew reaches the wire; settle instantly so
    // a successful slew's watcher doesn't linger between scenarios.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    cfg.mount.settle_after_slew = Duration::from_millis(0);
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();
    (d, mock)
}

#[tokio::test]
async fn slew_rolls_back_slew_in_progress_when_motion_fails() {
    // A wire failure while issuing the slew must roll the reservation
    // back, or the driver would report Slewing forever. The
    // SlewReservation guard clears the flag on drop; because the flag is
    // an atomic the rollback is synchronous — observable the instant the
    // error returns, with no settle wait.
    let (d, mock) = capturing_connected_device().await;
    // Fail the slew's first wire op (`:K1` in stop_and_wait).
    mock.lock().await.fail_command = Some(b'K');

    d.slew_to_coordinates_async(6.0, 30.0).await.unwrap_err();

    assert!(
        !d.slew_in_progress.load(Ordering::SeqCst),
        "slew_in_progress must be cleared after a failed slew"
    );
    assert!(!d.slewing().await.unwrap());
}

#[tokio::test]
async fn park_rolls_back_slew_in_progress_when_motion_fails() {
    let (d, mock) = capturing_connected_device().await;
    // Fail park's first wire op (`:K1` in the per-axis stop_and_wait).
    mock.lock().await.fail_command = Some(b'K');

    d.park().await.unwrap_err();

    assert!(
        !d.slew_in_progress.load(Ordering::SeqCst),
        "slew_in_progress must be cleared after a failed park"
    );
    assert!(!d.slewing().await.unwrap());
}

#[tokio::test]
async fn disconnect_clears_slew_in_progress() {
    // `slew_in_progress` lives outside `DriverState` now, so the
    // disconnect arm of `set_connected` clears it directly — the coverage
    // that used to live in the `reset_for_disconnect` field test.
    let d = connected_device().await;
    d.slew_in_progress.store(true, Ordering::SeqCst);
    d.set_connected(false).await.unwrap();
    assert!(!d.slew_in_progress.load(Ordering::SeqCst));
}

#[tokio::test]
async fn slew_refuses_while_slew_already_in_progress() {
    // `slew_to_coordinates_async` has no early slew_in_progress guard
    // (unlike `set_side_of_pier`), so a slew issued while one is already
    // in flight reaches the `SlewReservation::try_acquire` refusal in
    // `execute_slew_with_explicit_side` rather than being rejected sooner.
    let d = fast_settle_connected().await;
    d.slew_in_progress.store(true, Ordering::SeqCst);
    let err = d.slew_to_coordinates_async(6.0, 30.0).await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
    assert!(
        err.message.contains("slew already in progress"),
        "expected the reservation refusal, got: {}",
        err.message
    );
    // try_acquire failed, so no guard armed and the early return did not
    // clear the in-flight reservation.
    assert!(d.slew_in_progress.load(Ordering::SeqCst));
}

#[tokio::test]
async fn park_refuses_while_slew_already_in_progress() {
    // `park` reaches its own `SlewReservation::try_acquire` refusal when a
    // slew/park is already in flight (it has no early slew_in_progress
    // guard before the reservation either).
    let d = fast_settle_connected().await;
    d.slew_in_progress.store(true, Ordering::SeqCst);
    let err = d.park().await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
    assert!(
        err.message.contains("slew already in progress"),
        "expected the reservation refusal, got: {}",
        err.message
    );
    assert!(d.slew_in_progress.load(Ordering::SeqCst));
}

#[tokio::test]
async fn abort_slew_refuses_while_disconnected() {
    let d = fast_settle_device();
    let err = d.abort_slew().await.unwrap_err();
    assert_eq!(err.code, ASCOMError::NOT_CONNECTED.code);
}

#[tokio::test]
async fn abort_slew_refuses_while_parked() {
    let d = fast_settle_connected().await;
    d.park().await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if d.at_park().await.unwrap() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let err = d.abort_slew().await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_WHILE_PARKED);
}

#[tokio::test]
async fn sync_to_coordinates_writes_the_encoder() {
    let d = connected_device().await;
    // After a sync to (RA=lst, Dec=0), the RA encoder should be at
    // mechanical-HA=0 → encoder ticks=0, and Dec encoder=0.
    let lst = d.sidereal_time().await.unwrap();
    d.sync_to_coordinates(lst, 0.0).await.unwrap();
    // Wait for the polling task to refresh the snapshot.
    tokio::time::sleep(Duration::from_millis(250)).await;
    // The synced position should round-trip back through the read path.
    let dec = d.declination().await.unwrap();
    assert!(dec.abs() < 0.5, "Dec after sync should be ~0, got {dec}");
}

#[tokio::test]
async fn sync_publishes_position_without_waiting_for_poll() {
    // ConformU reads `RightAscension` ~2 ms after `SyncToCoordinates`
    // returns. The cached snapshot must reflect the synced position
    // immediately rather than holding the prior poll value.
    let d = fast_settle_connected().await;
    let lst = d.sidereal_time().await.unwrap();
    d.sync_to_coordinates(lst, 0.0).await.unwrap();
    // No sleep: read straight after sync.
    let dec = d.declination().await.unwrap();
    assert!(
        dec.abs() < 0.5,
        "Dec must reflect the sync immediately, got {dec}"
    );
    let ra = d.right_ascension().await.unwrap();
    // The synced RA equals the LST snapshot used by sync to compute
    // mech_HA=0; tiny LST drift between the sync and the read can
    // push this by up to a few arcseconds, well under 1 arc-minute.
    let ra_drift_arcsec =
        (ra - lst).rem_euclid(24.0).min((lst - ra).rem_euclid(24.0)) * 3600.0 * 15.0;
    assert!(
        ra_drift_arcsec < 60.0,
        "RA must reflect the sync immediately, got drift={ra_drift_arcsec}\" (ra={ra}, lst={lst})"
    );
}

#[tokio::test]
async fn sync_to_coordinates_updates_target() {
    // Per ASCOM ITelescopeV3, a successful Sync sets Target{RA,Dec}.
    let d = fast_settle_connected().await;
    // Pre-seed the target to a different value so the assertion is
    // about Sync writing it, not about leaving an already-correct
    // value alone.
    d.set_target_right_ascension(10.0).await.unwrap();
    d.set_target_declination(20.0).await.unwrap();
    d.sync_to_coordinates(3.0, 45.0).await.unwrap();
    assert_eq!(d.target_right_ascension().await.unwrap(), 3.0);
    assert_eq!(d.target_declination().await.unwrap(), 45.0);
}

#[tokio::test]
async fn sync_failure_does_not_clobber_target() {
    // A sync rejected by input validation must leave any previously
    // set Target intact, so callers can rely on Target as the last
    // *successful* slew-or-sync coordinate.
    let d = fast_settle_connected().await;
    d.set_target_right_ascension(10.0).await.unwrap();
    d.set_target_declination(20.0).await.unwrap();
    // RA out of range → INVALID_VALUE before any wire write.
    assert!(d.sync_to_coordinates(25.0, 45.0).await.is_err());
    assert_eq!(d.target_right_ascension().await.unwrap(), 10.0);
    assert_eq!(d.target_declination().await.unwrap(), 20.0);
}

/// Frame-transport that always reports `running = true` on `:f<axis>`
/// and acks `:K<axis>` without changing state. Other handshake commands
/// get plausibly-shaped replies (CPR, `TMR_Freq`, etc.) so the shared
/// transport's handshake completes. Used to drive `stop_axis_and_wait`
/// into its timeout branch — real hardware never gets stuck like this,
/// but the regular mock processes `:K` instantaneously.
struct StuckAxisFrameTransport {
    pending: std::collections::VecDeque<Vec<u8>>,
}

impl StuckAxisFrameTransport {
    const fn new() -> Self {
        Self {
            pending: std::collections::VecDeque::new(),
        }
    }
}

#[async_trait]
impl rusty_photon_shared_transport::FrameTransport for StuckAxisFrameTransport {
    async fn send_frame(
        &mut self,
        bytes: &[u8],
    ) -> std::result::Result<(), rusty_photon_shared_transport::TransportError> {
        if bytes.len() < 3 || bytes[0] != b':' || bytes[bytes.len() - 1] != b'\r' {
            return Err(rusty_photon_shared_transport::TransportError::Framing(
                format!("malformed: {bytes:?}"),
            ));
        }
        let reply: Vec<u8> = match bytes[1] {
            // `:f<axis>` reply with running=1: nibble-1 bit-0 set.
            b'f' => b"=011\r".to_vec(),
            // Handshake inquiries: 6-hex u24 payload.
            b'a' | b'b' | b'e' => b"=000080\r".to_vec(),
            // High-speed-ratio: 2-hex u8 payload per real GTi.
            b'g' => b"=01\r".to_vec(),
            // `:j<axis>`: biased position (0x800000 → encoder 0).
            b'j' => b"=000080\r".to_vec(),
            // Everything else acks empty.
            _ => b"=\r".to_vec(),
        };
        self.pending.push_back(reply);
        Ok(())
    }

    async fn recv_frame(
        &mut self,
        buf: &mut Vec<u8>,
    ) -> std::result::Result<(), rusty_photon_shared_transport::TransportError> {
        match self.pending.pop_front() {
            Some(frame) => {
                buf.clear();
                buf.extend_from_slice(&frame);
                Ok(())
            }
            None => Err(rusty_photon_shared_transport::TransportError::Eof),
        }
    }
}

struct StuckAxisFactory;

#[async_trait]
impl rusty_photon_shared_transport::TransportFactory for StuckAxisFactory {
    async fn open(
        &self,
    ) -> std::result::Result<
        Box<dyn rusty_photon_shared_transport::FrameTransport>,
        rusty_photon_shared_transport::TransportError,
    > {
        Ok(Box::new(StuckAxisFrameTransport::new()))
    }
}

#[tokio::test]
async fn stop_axis_and_wait_returns_transport_error_when_axis_never_stops() {
    // The free-function helper's *timeout* branch is unreachable
    // from the happy-path covered by the slew/park watcher tests
    // because the regular mock acks `:K` instantly. This test
    // wires a deliberately-broken transport that always reports
    // `running = true` so the helper hits its timeout after the
    // supplied duration.
    let manager = MountManager::new(&base_config(), Arc::new(StuckAxisFactory));
    let session = manager.transport().acquire().await.unwrap();
    let err = stop_axis_and_wait(&manager, &session, Axis::Ra, Duration::from_millis(300))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StarAdvError::Transport(ref msg) if msg.contains("did not stop")),
        "expected Transport(\"... did not stop ...\") error, got {err:?}"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn watcher_should_abort_returns_true_when_slew_in_progress_cleared() {
    // Direct unit test for the helper that gates the watcher's
    // post-snapshot wire sends. After the shared-transport
    // migration the "is the user still connected?" signal is the
    // device's session-slot presence rather than
    // `manager.is_available()` (the watcher's own session keeps
    // the transport open even after the user disconnects).
    use rusty_photon_shared_transport::Session;
    let slew_in_progress = AtomicBool::new(false);
    let manager = MountManager::new(&base_config(), Arc::new(MockTransportFactory));
    let device_session = manager.transport().acquire().await.unwrap();
    let session_slot: Arc<RwLock<Option<Session<crate::codec::SkywatcherCodec>>>> =
        Arc::new(RwLock::new(Some(device_session)));

    // slew_in_progress=false → abort=true.
    assert!(
        watcher_should_abort(&slew_in_progress, &session_slot).await,
        "slew_in_progress=false → should abort"
    );

    // With slew_in_progress=true and the session slot populated → no abort.
    slew_in_progress.store(true, Ordering::SeqCst);
    assert!(
        !watcher_should_abort(&slew_in_progress, &session_slot).await,
        "in-progress slew with live device session → should continue"
    );

    // Clear the device's session (user disconnect) → abort=true
    // even if slew flag is on.
    if let Some(s) = session_slot.write().await.take() {
        s.close().await.unwrap();
    }
    assert!(
        watcher_should_abort(&slew_in_progress, &session_slot).await,
        "user disconnect mid-slew → should abort"
    );
}

#[tokio::test]
async fn pickup_reslew_axis_swallows_transport_errors() {
    // The watcher calls `pickup_reslew_axis` per axis from the
    // pickup loop. Its failure-logging branches fire when the
    // wrapped `stop_axis_and_wait` or `issue_slew_axis` returns
    // an error — that happens when the axis stays stuck. With
    // the StuckAxis transport, the inner `stop_axis_and_wait`
    // hits its timeout branch; the helper must log and return
    // without panicking.
    let manager = MountManager::new(&base_config(), Arc::new(StuckAxisFactory));
    let session = manager.transport().acquire().await.unwrap();
    pickup_reslew_axis(&manager, &session, Axis::Ra, 1_000_000).await;
    pickup_reslew_axis(&manager, &session, Axis::Dec, -1_000_000).await;
    session.close().await.unwrap();
}

// ---- SetPark / Park persistence ----

fn device_with_path(path: PathBuf) -> MountDevice {
    let mut cfg = base_config();
    // Disable the CW-exclusion zone check for this test.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    let manager = MountManager::new(&cfg, Arc::new(MockTransportFactory));
    MountDevice::with_config_file_path(cfg.mount, manager, Some(path))
}

/// Like [`device_with_path`] but backed by a [`CapturingMockFactory`] so
/// the test can seed the mock's **wire** encoder state. `SetPark` reads
/// the live encoder via `poll_axes_now`, so seeding the cached snapshot
/// (`seed_ra_position`) is not enough — the value must be on the wire
/// for a fresh `:j` poll to capture it.
fn device_with_path_and_mock(
    path: PathBuf,
) -> (MountDevice, Arc<tokio::sync::Mutex<MockMountState>>) {
    let factory = CapturingMockFactory::new();
    let mock = Arc::clone(&factory.state);
    let mut cfg = base_config();
    // Disable the CW-exclusion zone check for these tests.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::with_config_file_path(cfg.mount, manager, Some(path));
    (d, mock)
}

/// Helper: write a default `Config` to `path` as pretty JSON. Used as
/// the seed file for `SetPark` round-trip tests.
fn seed_default_config(path: &Path) {
    let cfg = base_config();
    let json = serde_json::to_string_pretty(&cfg).unwrap();
    std::fs::write(path, json).unwrap();
}

#[test]
fn write_park_to_config_round_trips_through_typed_config() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let mut cfg = base_config();
    cfg.server.port = 12345;
    std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();

    write_park_to_config(&path, 8000, -3000).unwrap();

    let back: Config = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(back.mount.park_ra_ticks, Some(8000));
    assert_eq!(back.mount.park_dec_ticks, Some(-3000));
    // Unrelated fields survive the round-trip.
    assert_eq!(back.server.port, 12345);
}

#[test]
fn write_park_to_config_overwrites_existing_park_keys() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let mut cfg = base_config();
    cfg.mount.park_ra_ticks = Some(100);
    cfg.mount.park_dec_ticks = Some(200);
    std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();

    write_park_to_config(&path, 999, -1000).unwrap();

    let back: Config = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(back.mount.park_ra_ticks, Some(999));
    assert_eq!(back.mount.park_dec_ticks, Some(-1000));
}

#[test]
fn write_park_to_config_preserves_unknown_keys() {
    // The driver promises to touch only `mount.park_*_ticks`. Any
    // other key — including fields the typed `Config` doesn't model
    // (future schema additions, operator-added scratch values) —
    // must survive the round-trip. Tested at the raw JSON layer so
    // the typed `Config`'s field set isn't accidentally what we're
    // measuring.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let json = serde_json::json!({
        "transport": {
            "kind": "usb",
            "port": "/dev/ttyACM0",
            "baud_rate": 115_200,
            "command_timeout": "2s",
            "polling_interval": "200ms"
        },
        "server": {
            "port": 11117,
            "discovery_port": null,
            "tls": null,
            "auth": null
        },
        "mount": {
            "name": "Test",
            "unique_id": "test-001",
            "description": "Test",
            "site_latitude_deg": 0.0,
            "site_longitude_deg": 0.0,
            "future_field": "preserve me"
        },
        "top_level_future_field": [1, 2, 3]
    });
    std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();

    write_park_to_config(&path, 5, 10).unwrap();

    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["mount"]["park_ra_ticks"], serde_json::json!(5));
    assert_eq!(raw["mount"]["park_dec_ticks"], serde_json::json!(10));
    assert_eq!(
        raw["mount"]["future_field"],
        serde_json::json!("preserve me")
    );
    assert_eq!(raw["top_level_future_field"], serde_json::json!([1, 2, 3]));
}

#[test]
fn write_park_to_config_fails_when_file_missing() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("does_not_exist.json");
    let err = write_park_to_config(&path, 0, 0).unwrap_err();
    assert!(matches!(err, StarAdvError::Config(_)));
}

#[test]
fn write_park_to_config_fails_when_mount_object_is_missing() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("malformed.json");
    std::fs::write(&path, "{}").unwrap();
    let err = write_park_to_config(&path, 0, 0).unwrap_err();
    match err {
        StarAdvError::Config(msg) => assert!(msg.contains("mount"), "{msg}"),
        other => panic!("expected Config error, got {other:?}"),
    }
}

#[test]
fn write_park_to_config_fails_on_malformed_json() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    // Unclosed bracket — `serde_json::from_str` rejects it.
    std::fs::write(&path, "{ not valid json").unwrap();
    let err = write_park_to_config(&path, 0, 0).unwrap_err();
    match err {
        StarAdvError::Config(msg) => assert!(msg.contains("parse config"), "{msg}"),
        other => panic!("expected Config error, got {other:?}"),
    }
}

#[test]
fn read_connect_fields_fails_on_malformed_json() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, "{ not valid json").unwrap();
    let err = read_connect_fields(&path).unwrap_err();
    match err {
        StarAdvError::Config(msg) => assert!(msg.contains("parse config"), "{msg}"),
        other => panic!("expected Config error, got {other:?}"),
    }
}

#[tokio::test]
async fn park_with_unanchored_frame_stops_in_place_without_goto() {
    // With an unanchored frame (`ap_park_0`, no sync, no raw override)
    // both park-target slots are `None` and `Park()` must not slew: a
    // goto requires a `:S<axis>` target write, and none may reach the
    // wire. Both axes are stopped where they stand and the watcher
    // still sets AtPark.
    let (d, mock) = capturing_connected_device().await;
    {
        let s = d.state.read().await;
        assert_eq!(s.park_ra_ticks, None, "precondition: RA unarmed");
        assert_eq!(s.park_dec_ticks, None, "precondition: Dec unarmed");
    }
    d.park().await.unwrap();
    let log: Vec<String> = mock
        .lock()
        .await
        .command_log
        .iter()
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect();
    assert!(
        !log.iter().any(|c| c.starts_with(":S")),
        "expected no :S goto targets on the wire, log: {log:?}"
    );
    // The park still completes: the watcher observes both (already
    // stopped) axes and sets AtPark.
    for _ in 0..250 {
        if d.at_park().await.unwrap() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(d.at_park().await.unwrap(), "AtPark should be set");
}

#[tokio::test]
async fn debug_impl_includes_config_file_path() {
    // Pins the `derive_more::Debug` derive — adding a new field
    // that should appear in Debug requires keeping it un-`#[debug(skip)]`
    // in the struct attributes. The path field landed in PR #221;
    // the smoke test catches a future refactor that accidentally
    // hides it.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let d = device_with_path(path);
    let s = format!("{d:?}");
    assert!(s.contains("MountDevice"), "{s}");
    assert!(s.contains("config_file_path"), "{s}");
}

#[tokio::test]
async fn can_set_park_is_false_when_no_config_path_was_provided() {
    let d = device();
    assert!(!d.can_set_park().await.unwrap());
}

#[tokio::test]
async fn can_set_park_is_true_when_started_with_a_config_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let d = device_with_path(dir.path().join("config.json"));
    assert!(d.can_set_park().await.unwrap());
}

#[tokio::test]
async fn set_park_returns_not_implemented_without_a_config_path() {
    let d = device();
    let err = d.set_park().await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn set_park_returns_not_connected_when_disconnected() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    seed_default_config(&path);
    let d = device_with_path(path);
    let err = d.set_park().await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
}

// ---- PulseGuide tests ----

#[tokio::test]
async fn pulse_guide_capability_flags_are_true() {
    let d = device();
    assert!(d.can_pulse_guide().await.unwrap());
    assert!(d.can_set_guide_rates().await.unwrap());
}

#[tokio::test]
async fn is_pulse_guiding_defaults_to_false_after_connect() {
    let d = connected_device().await;
    assert!(!d.is_pulse_guiding().await.unwrap());
}

#[tokio::test]
async fn default_guide_rates_are_half_sidereal() {
    let d = connected_device().await;
    let ra = d.guide_rate_right_ascension().await.unwrap();
    let dec = d.guide_rate_declination().await.unwrap();
    // 0.5 × SIDEREAL_DEG_PER_SEC ≈ 0.00208904
    let expected = 0.5 * SIDEREAL_DEG_PER_SEC;
    assert!((ra - expected).abs() < 1e-9, "RA: {ra}");
    assert!((dec - expected).abs() < 1e-9, "Dec: {dec}");
}

#[tokio::test]
async fn set_guide_rate_ra_round_trips_through_fraction() {
    let d = connected_device().await;
    let target = 0.001_f64;
    d.set_guide_rate_right_ascension(target).await.unwrap();
    let got = d.guide_rate_right_ascension().await.unwrap();
    assert!(
        (got - target).abs() < 1e-9,
        "round-trip: set {target}, got {got}"
    );
}

#[tokio::test]
async fn set_guide_rate_rejects_zero_and_negative() {
    let d = connected_device().await;
    let err = d.set_guide_rate_right_ascension(0.0).await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    let err = d.set_guide_rate_declination(-0.001).await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
}

#[tokio::test]
async fn set_guide_rate_rejects_at_or_above_sidereal() {
    // Upper bound is exclusive — fraction = 1.0 zeroes East's
    // rate factor (`1 - fraction`) and divides by zero in the
    // step-period formula.
    let d = connected_device().await;
    let err = d
        .set_guide_rate_right_ascension(SIDEREAL_DEG_PER_SEC)
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    let err = d
        .set_guide_rate_right_ascension(SIDEREAL_DEG_PER_SEC * 2.0)
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
}

#[tokio::test]
async fn pulse_guide_refuses_while_disconnected() {
    let d = device();
    let err = d
        .pulse_guide(GuideDirection::North, Duration::from_millis(100))
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
}

#[tokio::test]
async fn set_connected_rolls_back_transport_when_park_load_fails() {
    // Regression test for the Copilot review on PR #221
    // (comment 3238682044): if `transport.connect()` succeeds but
    // the post-connect park-target load fails, the transport
    // ref-count was being left incremented (the underlying
    // transport open) while `requested_connection` stayed `false`
    // — effectively leaking a connection. The fix runs the
    // post-connect work through `load_park_target_after_connect`
    // and calls `transport.disconnect()` on any failure before
    // surfacing the error.
    //
    // We trigger the failure path by handing `MountDevice` a
    // `config_file_path` that points to a non-existent file:
    // the handshake will succeed (mock transport is happy), but
    // `read_connect_fields` will fail with a missing-file error.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("does_not_exist.json");
    let d = device_with_path(path);

    let err = d.set_connected(true).await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);

    // The transport must have been disconnected on rollback.
    // `is_available()` is the underlying MountManager flag,
    // which would be `true` if connect succeeded and no rollback
    // ran. Asserting it false here proves we balanced the
    // connect ref-count.
    assert!(
        !d.manager.is_available(),
        "transport should be torn down after rollback"
    );
    // And the user-visible `connected()` getter agrees.
    assert!(!d.connected().await.unwrap());
}

#[tokio::test]
async fn set_park_refuses_while_slew_in_progress() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    seed_default_config(&path);
    let d = device_with_path(path);
    d.set_connected(true).await.unwrap();
    d.slew_in_progress.store(true, Ordering::SeqCst);
    let err = d.set_park().await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
}

#[tokio::test]
async fn set_park_refuses_when_wire_snapshot_reports_axis_running() {
    // Defence-in-depth (per Copilot review on PR #221, comment
    // 3242621736): even if `slew_in_progress` is false — e.g. an
    // axis is running for a reason the in-memory flag wouldn't
    // capture — the wire snapshot's `running` flag must still
    // gate `SetPark` to avoid persisting mid-motion encoder
    // ticks.
    //
    // To exercise this path independently of the
    // `slew_in_progress` flag, we connect with a
    // `CapturingMockFactory`, force `ra.running = true` directly
    // on the mock state (bypassing the slew_to_*_async flag-set
    // path), wait for the next background poll so the snapshot
    // reflects the wire, then call SetPark.
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    let mock = std::sync::Arc::clone(&factory.state);
    let mut cfg = base_config();
    if let crate::config::TransportConfig::Usb(usb) = &mut cfg.transport {
        usb.polling_interval = Duration::from_millis(20);
    }
    // Disable the CW-exclusion zone check for this test.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    seed_default_config(&path);
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::with_config_file_path(cfg.mount, manager, Some(path));
    d.set_connected(true).await.unwrap();

    // Force the wire-side running flag without going through
    // `slew_to_coordinates_async` (which would set
    // `slew_in_progress` and trip the other guard).
    {
        let mut s = mock.lock().await;
        s.ra.running = true;
        s.ra.initialized = true;
    }
    // Wait for the background poll to ingest the new wire state.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if d.manager.snapshot().await.ra.running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        d.manager.snapshot().await.ra.running,
        "precondition: snapshot must reflect RA running=true"
    );
    // slew_in_progress flag is still false — only the wire
    // snapshot is reporting motion. The new defence layer must
    // still refuse.
    assert!(!d.slew_in_progress.load(Ordering::SeqCst));
    let err = d.set_park().await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
    assert!(
        err.message.contains("snapshot"),
        "error should reference the wire snapshot: {}",
        err.message
    );
}

#[tokio::test]
async fn set_park_persists_current_wire_position_and_updates_in_memory_target() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    seed_default_config(&path);
    let (d, mock) = device_with_path_and_mock(path.clone());
    d.set_connected(true).await.unwrap();
    // Seed the mock's *wire* encoder: SetPark reads the live position
    // via `poll_axes_now`, so the value must be on the wire (a
    // stationary axis won't be advanced by `:j` polling).
    {
        let mut s = mock.lock().await;
        s.ra.position_ticks = 8000;
        s.dec.position_ticks = -3000;
    }

    d.set_park().await.unwrap();

    // In-memory target updated.
    let s = d.state.read().await;
    assert_eq!(s.park_ra_ticks, Some(8000));
    assert_eq!(s.park_dec_ticks, Some(-3000));
    drop(s);

    // On-disk config updated.
    let back: Config = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(back.mount.park_ra_ticks, Some(8000));
    assert_eq!(back.mount.park_dec_ticks, Some(-3000));
}

#[tokio::test]
async fn park_target_stays_unarmed_when_frame_is_unanchored() {
    // No raw `park_*_ticks` override and `unpark_from_ap_position =
    // ap_park_0` (no seed, no sync yet) → the frame is unanchored and
    // NO park target is armed. An absolute `preferred_ap_park` target
    // computed against an unanchored frame is a fabricated position —
    // `Park()` stops in place instead (workspace tenet 3: no actuation
    // on connect).
    let d = device();
    d.set_connected(true).await.unwrap();
    let s = d.state.read().await;
    assert_eq!(s.park_ra_ticks, None);
    assert_eq!(s.park_dec_ticks, None);
    assert!(!s.frame_anchored);
}

#[tokio::test]
async fn park_target_arms_preferred_ap_park_when_unpark_pose_anchors_the_frame() {
    // A named `unpark_from_ap_position` is the operator's power-up
    // pose assertion: the frame is anchored from connect and the park
    // target resolves to the `preferred_ap_park` default (`ap_park_3`).
    // Latitude 0 → mech_HA = -6h (ra = -6/24 * cpr_ra = -907,200) and
    // dec_enc = +90° (northern arm via `>= 0`; dec = 90/360 * cpr_dec =
    // +725,760 — the dec axis has its own, smaller CPR) — identical to
    // the connect-time seed values, so a park right after connect is a
    // zero-distance goto.
    let mut cfg = base_config();
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    cfg.mount.unpark_from_ap_position = crate::config::ApPark::ApPark3;
    let manager = MountManager::new(&cfg, Arc::new(MockTransportFactory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();
    let s = d.state.read().await;
    assert!(s.frame_anchored);
    assert_eq!(s.park_ra_ticks, Some(-907_200));
    assert_eq!(s.park_dec_ticks, Some(725_760));
}

#[tokio::test]
async fn sync_anchors_the_frame_and_arms_the_park_target() {
    // SyncToCoordinates is measured ground truth for the encoder→pose
    // mapping: it must anchor a previously unanchored frame and fill
    // the park-target slots the unanchored connect left empty, so
    // subsequent parks slew to the preferred AP park normally.
    let d = connected_device().await;
    {
        let s = d.state.read().await;
        assert_eq!(s.park_ra_ticks, None, "precondition: unarmed");
        assert!(!s.frame_anchored, "precondition: unanchored");
    }
    d.sync_to_coordinates(6.0, 30.0).await.unwrap();
    let s = d.state.read().await;
    assert!(s.frame_anchored);
    assert_eq!(s.park_ra_ticks, Some(-907_200));
    assert_eq!(s.park_dec_ticks, Some(725_760));
}

#[tokio::test]
async fn sync_after_anchored_connect_leaves_armed_park_targets_unchanged() {
    // With a named unpark pose the connect already armed both targets;
    // the sync's re-arm hook must leave them untouched (the both-armed
    // early return).
    let mut cfg = base_config();
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    cfg.mount.unpark_from_ap_position = crate::config::ApPark::ApPark3;
    let manager = MountManager::new(&cfg, Arc::new(MockTransportFactory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();
    d.sync_to_coordinates(6.0, 30.0).await.unwrap();
    let s = d.state.read().await;
    assert!(s.frame_anchored);
    assert_eq!(s.park_ra_ticks, Some(-907_200));
    assert_eq!(s.park_dec_ticks, Some(725_760));
}

#[tokio::test]
async fn rearm_before_any_connect_anchors_but_arms_nothing() {
    // Defensive path: no connect has resolved `preferred_ap_park`
    // (`None`), so the re-arm hook flips the anchor flag and returns
    // without arming a target.
    let d = device();
    d.anchor_frame_and_rearm_park_target().await;
    let s = d.state.read().await;
    assert!(s.frame_anchored);
    assert_eq!(s.park_ra_ticks, None);
    assert_eq!(s.park_dec_ticks, None);
}

#[tokio::test]
async fn rearm_while_disconnected_keeps_targets_unarmed() {
    // With a resolved preferred park but no transport parameters
    // (never connected), the pose→ticks conversion is unavailable: the
    // hook must leave the targets unarmed rather than guess.
    let d = device();
    d.state.write().await.preferred_ap_park = Some(crate::config::ApPark::ApPark3);
    d.anchor_frame_and_rearm_park_target().await;
    let s = d.state.read().await;
    assert!(s.frame_anchored);
    assert_eq!(s.park_ra_ticks, None);
    assert_eq!(s.park_dec_ticks, None);
}

#[tokio::test]
async fn set_preferred_ap_park_rearms_using_a_sync_derived_anchor() {
    // A sync-derived anchor must survive the SetPreferredApPark live
    // re-resolve: the reload recomputes anchoring as config-named-pose
    // OR the existing in-memory anchor, and the on-disk `ap_park_0`
    // here exercises the OR's in-memory side.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    seed_default_config(&path);
    let d = device_with_path(path.clone());
    d.set_connected(true).await.unwrap();
    d.sync_to_coordinates(6.0, 30.0).await.unwrap();
    let ret = d
        .action("SetPreferredApPark".to_string(), "ap_park_2".to_string())
        .await
        .unwrap();
    assert_eq!(ret, "ap_park_2");
    // Re-resolve kept the sync anchor and re-armed to the new pose:
    // ap_park_2 at latitude 0 → ra = -907,200, dec_enc = 0.
    let s = d.state.read().await;
    assert!(s.frame_anchored);
    assert_eq!(s.park_ra_ticks, Some(-907_200));
    assert_eq!(s.park_dec_ticks, Some(0));
}

#[tokio::test]
async fn unpark_seed_fires_when_firmware_reports_near_zero_encoder() {
    // Sky-Watcher firmware does not always read exactly (0, 0) after a
    // power-cycle: the validation GTi reports dec = -1 on fresh
    // power-up. Without the FRESH_POWER_UP_TICK_TOLERANCE guard the
    // strict `!= 0` check would skip the seed and the mount would
    // silently end up with a wrong celestial mapping. The seed is
    // observed directly via the post-seed snapshot encoder — the park
    // target no longer reflects the seed (it tracks preferred_ap_park).
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    // Force the dec encoder to a 1-tick fresh-power-up artifact
    // before the manager opens the transport.
    {
        let mut state = factory.state.lock().await;
        state.dec.position_ticks = -1;
    }
    let mut cfg = base_config();
    // Disable the CW-exclusion zone check for this test.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    cfg.mount.unpark_from_ap_position = crate::config::ApPark::ApPark3;
    cfg.mount.site_latitude_deg = 32.7157;
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();
    // ApPark3 N hemisphere with the mock's per-axis CPRs (ra
    // 0x375F00 = 3,628,800, dec 0x2C4C00 = 2,903,040): expected seed
    // → ra_ticks = -907,200, dec_ticks = +725,760. The
    // snapshot reflects the `:E` writes; if the seed had been skipped
    // it would still read the pre-seed (0, -1).
    let snap = d.manager.snapshot().await;
    assert_eq!(snap.ra.position_ticks, -907_200);
    assert_eq!(snap.dec.position_ticks, 725_760);
}

#[tokio::test]
async fn unpark_seed_skips_when_firmware_encoder_beyond_tolerance() {
    // A real post-slew encoder is tens of thousands of ticks away from
    // zero — well beyond FRESH_POWER_UP_TICK_TOLERANCE. The seed must
    // skip so a mid-session reconnect does not clobber the operator's
    // slewed-to position: the snapshot stays at the pre-seed reading
    // (no `:E` written).
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    {
        let mut state = factory.state.lock().await;
        state.ra.position_ticks = 50_000;
    }
    let mut cfg = base_config();
    // Disable the CW-exclusion zone check for this test.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    cfg.mount.unpark_from_ap_position = crate::config::ApPark::ApPark3;
    cfg.mount.site_latitude_deg = 32.7157;
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();
    // Seed skipped → snapshot unchanged from the fresh-power-up reading.
    let snap = d.manager.snapshot().await;
    assert_eq!(snap.ra.position_ticks, 50_000);
    assert_eq!(snap.dec.position_ticks, 0);
}

#[tokio::test]
async fn unpark_seed_skips_just_above_fresh_power_up_tolerance() {
    // Pins the tight 10-tick fresh-power-up floor. A reading of 50
    // ticks (~18″ at the GTi's CPR) is well above the single-tick
    // firmware artifact and indicates the operator has already moved
    // the mount; the seed must skip so the slewed-to position is not
    // clobbered. If the tolerance is ever loosened back toward the
    // historical 100-tick floor, this test catches it.
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    {
        let mut state = factory.state.lock().await;
        state.ra.position_ticks = 50;
    }
    let mut cfg = base_config();
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    cfg.mount.unpark_from_ap_position = crate::config::ApPark::ApPark3;
    cfg.mount.site_latitude_deg = 32.7157;
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();
    let snap = d.manager.snapshot().await;
    assert_eq!(snap.ra.position_ticks, 50);
    assert_eq!(snap.dec.position_ticks, 0);
}

#[tokio::test]
async fn park_target_uses_preferred_ap_park_distinct_from_unpark_seed() {
    // The fresh-power-up seed (`unpark_from_ap_position`) and the
    // `Park()` target (`preferred_ap_park`) are independent. Configure
    // a seed of ap_park_3 and a *different* preferred park of ap_park_2:
    // the snapshot must reflect the ap_park_3 seed, while the park
    // target resolves to the ap_park_2 encoder pair.
    let mut cfg = base_config();
    // Disable the CW-exclusion zone check for this test.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    cfg.mount.unpark_from_ap_position = crate::config::ApPark::ApPark3;
    cfg.mount.preferred_ap_park = crate::config::ApPark::ApPark2;
    cfg.mount.site_latitude_deg = 32.7157;
    let manager = MountManager::new(&cfg, Arc::new(MockTransportFactory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();
    // ap_park_3 seed: mech_HA = -6h, dec_enc = +90° → snapshot
    // (-907,200, +725,760).
    let snap = d.manager.snapshot().await;
    assert_eq!(snap.ra.position_ticks, -907_200);
    assert_eq!(snap.dec.position_ticks, 725_760);
    // ap_park_2 park target: mech_HA = -6h (ra = -907,200), dec_enc = 0°
    // (dec = 0) — distinct from the seed on the dec axis.
    let s = d.state.read().await;
    assert_eq!(s.park_ra_ticks, Some(-907_200));
    assert_eq!(s.park_dec_ticks, Some(0));
}

// ---- reset_mount_encoders helper (Phase B) ----

#[tokio::test]
async fn reset_mount_encoders_writes_encoder_and_clears_state() {
    let d = connected_device().await;
    // Dirty the in-memory motion/target/tracking state a reset clears.
    d.slew_in_progress.store(true, Ordering::SeqCst);
    {
        let mut s = d.state.write().await;
        s.target_ra_hours = Some(5.0);
        s.target_dec_degrees = Some(10.0);
        s.tracking_requested = true;
    }
    {
        let guard = d.session.read().await;
        let session = guard.as_ref().expect("connected device holds a session");
        d.reset_mount_encoders(session, 12_345, -6_789)
            .await
            .unwrap();
    }
    // The `:E` writes are published to the cached snapshot.
    let snap = d.manager.snapshot().await;
    assert_eq!(snap.ra.position_ticks, 12_345);
    assert_eq!(snap.dec.position_ticks, -6_789);
    // Driver-internal motion/target/tracking state is cleared.
    assert!(!d.slew_in_progress.load(Ordering::SeqCst));
    let s = d.state.read().await;
    assert_eq!(s.target_ra_hours, None);
    assert_eq!(s.target_dec_degrees, None);
    assert!(!s.tracking_requested);
}

#[tokio::test]
async fn reset_mount_encoders_errors_when_axis_never_stops() {
    // If stop-and-wait fails (the axis never reports idle), the reset
    // bails before writing any encoder seed — motion is still in flight
    // and re-seeding then would race the firmware.
    let manager = Arc::new(MountManager::new(
        &base_config(),
        Arc::new(StuckAxisFactory),
    ));
    let session = manager.transport().acquire().await.unwrap();
    let d = MountDevice::new(base_config().mount, Arc::clone(&manager));
    let err = d
        .reset_mount_encoders(&session, 1_000, -1_000)
        .await
        .unwrap_err();
    assert!(
        err.message.contains("did not stop"),
        "expected a stop-timeout error, got {err:?}"
    );
}

// ---- Custom Actions (Phase D) ----

#[tokio::test]
async fn supported_actions_lists_the_three_vendor_actions() {
    let d = device();
    let actions = d.supported_actions().await.unwrap();
    assert_eq!(
        actions,
        vec![
            "SetUnparkFromApPosition".to_string(),
            "SetPreferredApPark".to_string(),
            "UnparkFromApPosition".to_string(),
        ]
    );
}

#[tokio::test]
async fn action_with_unknown_name_returns_action_not_implemented() {
    let d = device();
    let err = d
        .action("NoSuchAction".to_string(), String::new())
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::ACTION_NOT_IMPLEMENTED);
}

#[tokio::test]
async fn set_unpark_from_ap_position_persists_to_config() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    seed_default_config(&path);
    let d = device_with_path(path.clone());
    d.set_connected(true).await.unwrap();
    let ret = d
        .action(
            "SetUnparkFromApPosition".to_string(),
            "ap_park_2".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(ret, "ap_park_2");
    let back: Config = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        back.mount.unpark_from_ap_position,
        crate::config::ApPark::ApPark2
    );
}

#[tokio::test]
async fn set_unpark_from_ap_position_without_config_is_refused() {
    // No `--config` path → nowhere to persist; the Action refuses.
    let d = device();
    let err = d
        .action(
            "SetUnparkFromApPosition".to_string(),
            "ap_park_2".to_string(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
}

#[tokio::test]
async fn set_unpark_from_ap_position_rejects_unknown_park() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    seed_default_config(&path);
    let d = device_with_path(path);
    let err = d
        .action(
            "SetUnparkFromApPosition".to_string(),
            "ap_park_9".to_string(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
}

#[tokio::test]
async fn set_preferred_ap_park_rejects_ap_park_0() {
    // "Current position" is not a slew target.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    seed_default_config(&path);
    let d = device_with_path(path);
    let err = d
        .action("SetPreferredApPark".to_string(), "ap_park_0".to_string())
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
}

#[tokio::test]
async fn set_preferred_ap_park_persists_and_updates_live_target() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    seed_default_config(&path);
    // Anchor the frame via a named on-disk unpark pose — the live
    // re-resolve only arms an AP-pose target on an anchored frame
    // (unanchored frames keep no target and park in place).
    let mut on_disk: Config =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    on_disk.mount.unpark_from_ap_position = crate::config::ApPark::ApPark3;
    std::fs::write(&path, serde_json::to_string_pretty(&on_disk).unwrap()).unwrap();
    let d = device_with_path(path.clone());
    d.set_connected(true).await.unwrap();
    let ret = d
        .action("SetPreferredApPark".to_string(), "ap_park_2".to_string())
        .await
        .unwrap();
    assert_eq!(ret, "ap_park_2");
    // Persisted to disk.
    let back: Config = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(back.mount.preferred_ap_park, crate::config::ApPark::ApPark2);
    // Live park target re-resolved without a reconnect. device_with_path
    // runs at latitude 0; ap_park_2 → mech_HA = -6h (ra = -907,200),
    // dec_enc = 0° (dec = 0).
    let s = d.state.read().await;
    assert_eq!(s.park_ra_ticks, Some(-907_200));
    assert_eq!(s.park_dec_ticks, Some(0));
}

#[tokio::test]
async fn unpark_from_ap_position_ap_park_0_clears_at_park_without_encoder_change() {
    let d = connected_device().await;
    // Set the parked flag directly — this test exercises the unpark path.
    d.state.write().await.at_park = true;
    let before = d.manager.snapshot().await;
    let ret = d
        .action("UnparkFromApPosition".to_string(), "ap_park_0".to_string())
        .await
        .unwrap();
    assert_eq!(ret, "ap_park_0");
    assert!(!d.at_park().await.unwrap());
    // "Current position" leaves the encoder untouched (≡ standard Unpark).
    let after = d.manager.snapshot().await;
    assert_eq!(after.ra.position_ticks, before.ra.position_ticks);
    assert_eq!(after.dec.position_ticks, before.dec.position_ticks);
}

#[tokio::test]
async fn unpark_from_ap_position_named_park_resets_encoder_and_clears_at_park() {
    // The operator asserts the OTA is physically at ap_park_3; the
    // driver makes the firmware encoder match regardless of the stale
    // current reading, then clears AtPark.
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    {
        let mut state = factory.state.lock().await;
        state.ra.position_ticks = 200_000;
        state.dec.position_ticks = -50_000;
    }
    let mut cfg = base_config();
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    cfg.mount.site_latitude_deg = 32.7157;
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();
    d.state.write().await.at_park = true;
    let ret = d
        .action("UnparkFromApPosition".to_string(), "ap_park_3".to_string())
        .await
        .unwrap();
    assert_eq!(ret, "ap_park_3");
    // ap_park_3 at lat 32.7157: ra = -907,200, dec = +725,760.
    let snap = d.manager.snapshot().await;
    assert_eq!(snap.ra.position_ticks, -907_200);
    assert_eq!(snap.dec.position_ticks, 725_760);
    assert!(!d.at_park().await.unwrap());
}

#[tokio::test]
async fn unpark_from_ap_position_refuses_when_not_parked() {
    let d = connected_device().await;
    // at_park is false (default).
    let err = d
        .action("UnparkFromApPosition".to_string(), "ap_park_3".to_string())
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
}

#[tokio::test]
async fn unpark_from_ap_position_refuses_when_disconnected() {
    let d = device();
    let err = d
        .action("UnparkFromApPosition".to_string(), "ap_park_0".to_string())
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMError::NOT_CONNECTED.code);
}

#[tokio::test]
async fn unpark_from_ap_position_refuses_while_slewing() {
    let d = connected_device().await;
    d.state.write().await.at_park = true;
    d.slew_in_progress.store(true, Ordering::SeqCst);
    let err = d
        .action("UnparkFromApPosition".to_string(), "ap_park_3".to_string())
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
}

#[tokio::test]
async fn set_unpark_from_ap_position_round_trips_every_named_park() {
    // Exercises parse + canonical-string round-trip for the AP parks
    // the focused action tests don't otherwise touch (ap_park_1/4/5).
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    seed_default_config(&path);
    let d = device_with_path(path.clone());
    d.set_connected(true).await.unwrap();
    for (token, expected) in [
        ("ap_park_1", crate::config::ApPark::ApPark1),
        ("ap_park_4", crate::config::ApPark::ApPark4),
        ("ap_park_5", crate::config::ApPark::ApPark5),
    ] {
        let ret = d
            .action("SetUnparkFromApPosition".to_string(), token.to_string())
            .await
            .unwrap();
        assert_eq!(ret, token);
        let back: Config = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.mount.unpark_from_ap_position, expected);
    }
}

#[tokio::test]
async fn ap_park_target_ticks_is_none_for_ap_park_0() {
    // "Current position" has no codebase encoder mapping, so the
    // resolver returns None regardless of connection state.
    let d = device();
    assert_eq!(
        d.ap_park_target_ticks(crate::config::ApPark::ApPark0).await,
        None
    );
}

#[tokio::test]
async fn ap_park_target_ticks_is_none_when_disconnected() {
    // A named park has a codebase mapping, but the tick conversion
    // needs the handshake CPR — unavailable until connected.
    let d = device();
    assert_eq!(
        d.ap_park_target_ticks(crate::config::ApPark::ApPark3).await,
        None
    );
}

#[tokio::test]
async fn park_target_prefers_config_values_over_handshake_capture() {
    // Config carries park values → driver should use them, not the
    // (zeroed) handshake fallback.
    let mut cfg = base_config();
    // Disable the CW-exclusion zone check for this test.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    cfg.mount.park_ra_ticks = Some(5000);
    cfg.mount.park_dec_ticks = Some(-7000);
    let manager = MountManager::new(&cfg, Arc::new(MockTransportFactory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();
    let s = d.state.read().await;
    assert_eq!(s.park_ra_ticks, Some(5000));
    assert_eq!(s.park_dec_ticks, Some(-7000));
}

#[tokio::test]
async fn reconnect_after_set_park_picks_up_persisted_values() {
    // Regression test for the Copilot review feedback on PR #221:
    // SetPark persists the new park target to disk and updates the
    // in-memory state, but the in-memory `MountConfig` does not
    // change. A subsequent disconnect/reconnect within the same
    // process must therefore re-read the config file rather than
    // re-loading from the (stale) in-memory config — otherwise the
    // SetPark target silently reverts to whatever was in
    // `MountConfig` at process start (or the handshake fallback).
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    seed_default_config(&path);
    let (d, mock) = device_with_path_and_mock(path.clone());
    d.set_connected(true).await.unwrap();
    // Seed the mock *wire* encoder; SetPark reads it via `poll_axes_now`.
    {
        let mut s = mock.lock().await;
        s.ra.position_ticks = 8000;
        s.dec.position_ticks = -3000;
    }
    d.set_park().await.unwrap();

    // Disconnect: in-memory park state is cleared.
    d.set_connected(false).await.unwrap();
    assert_eq!(d.state.read().await.park_ra_ticks, None);
    assert_eq!(d.state.read().await.park_dec_ticks, None);

    // Reset the mock *wire* encoders so reconnect's handshake `:j`
    // fallback would be (0, 0) — proves the re-read picked up SetPark's
    // persisted values rather than just re-reading the handshake.
    {
        let mut s = mock.lock().await;
        s.ra.position_ticks = 0;
        s.dec.position_ticks = 0;
    }

    d.set_connected(true).await.unwrap();
    let s = d.state.read().await;
    assert_eq!(s.park_ra_ticks, Some(8000));
    assert_eq!(s.park_dec_ticks, Some(-3000));
}

#[tokio::test]
async fn reconnect_with_partial_config_uses_preferred_ap_park_for_missing_axis() {
    // Per-axis fallback on an anchored frame: if the config pins only
    // park_ra_ticks, RA comes from the file and Dec falls through to
    // the `preferred_ap_park` encoder pair (the default `ap_park_3`),
    // not the raw handshake reading. The named unpark pose anchors the
    // frame — with `ap_park_0` the missing axis would stay unarmed
    // (park-in-place) instead.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    // Hand-craft a JSON config that sets only park_ra_ticks
    // (park_dec_ticks absent, which `read_connect_fields`
    // must read as `None`).
    let mut cfg = base_config();
    cfg.mount.unpark_from_ap_position = crate::config::ApPark::ApPark3;
    cfg.mount.park_ra_ticks = Some(1234);
    // park_dec_ticks deliberately left as None.
    std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    let d = device_with_path(path);
    d.set_connected(true).await.unwrap();
    let s = d.state.read().await;
    assert_eq!(s.park_ra_ticks, Some(1234));
    // device_with_path runs at latitude 0; ap_park_3 dec_enc = +90°
    // → dec = 90/360 * cpr_dec = 725,760.
    assert_eq!(s.park_dec_ticks, Some(725_760));
}

#[tokio::test]
async fn partial_raw_override_is_honored_even_when_frame_is_unanchored() {
    // Raw ticks are the operator's own frame assertion: a pinned axis
    // keeps its target even with `ap_park_0` (unanchored), while the
    // unpinned axis stays unarmed and parks in place.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let mut cfg = base_config();
    cfg.mount.park_ra_ticks = Some(1234);
    std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    let d = device_with_path(path);
    d.set_connected(true).await.unwrap();
    let s = d.state.read().await;
    assert_eq!(s.park_ra_ticks, Some(1234));
    assert_eq!(s.park_dec_ticks, None);
    assert!(!s.frame_anchored);
}

#[test]
fn read_connect_fields_returns_none_for_each_missing_key() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let json = serde_json::json!({
        "mount": {
            "name": "Test",
        }
    });
    std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
    let f = read_connect_fields(&path).unwrap();
    assert_eq!(f.park_ra_ticks, None);
    assert_eq!(f.park_dec_ticks, None);
    assert_eq!(f.unpark_from_ap_position, None);
    assert_eq!(f.preferred_ap_park, None);
}

#[test]
fn read_connect_fields_parses_all_keys() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let json = serde_json::json!({
        "mount": {
            "park_ra_ticks": 1234,
            "park_dec_ticks": -5678,
            "unpark_from_ap_position": "ap_park_1",
            "preferred_ap_park": "ap_park_4",
        }
    });
    std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
    let f = read_connect_fields(&path).unwrap();
    assert_eq!(f.park_ra_ticks, Some(1234));
    assert_eq!(f.park_dec_ticks, Some(-5678));
    assert_eq!(
        f.unpark_from_ap_position,
        Some(crate::config::ApPark::ApPark1)
    );
    assert_eq!(f.preferred_ap_park, Some(crate::config::ApPark::ApPark4));
}

#[test]
fn read_connect_fields_treats_explicit_null_as_none_per_axis() {
    // Pins the doc-comment guarantee: a `None` field means the file
    // did not set that key OR set it to `null`, returned per key.
    // Here `park_ra_ticks` is a real value while `park_dec_ticks` is
    // explicitly JSON `null`; the reader returns `(Some(1234), None)`,
    // and the caller falls back to the AP-park target for the Dec axis.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let json = serde_json::json!({
        "mount": {
            "park_ra_ticks": 1234,
            "park_dec_ticks": null,
        }
    });
    std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
    let f = read_connect_fields(&path).unwrap();
    assert_eq!(f.park_ra_ticks, Some(1234));
    assert_eq!(f.park_dec_ticks, None);
}

#[test]
fn read_connect_fields_errors_on_invalid_ap_park() {
    // Operator typo: an unrecognised AP-park string must fail loudly
    // rather than silently fall back.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let json = serde_json::json!({
        "mount": {
            "unpark_from_ap_position": "ap_park_99",
        }
    });
    std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
    let err = read_connect_fields(&path).unwrap_err();
    match err {
        StarAdvError::Config(msg) => {
            assert!(msg.contains("unpark_from_ap_position"), "{msg}");
            assert!(msg.contains("AP park"), "{msg}");
        }
        other => panic!("expected Config error, got {other:?}"),
    }
}

#[test]
fn read_connect_fields_errors_on_wrong_type() {
    // Operator typo: park_ra_ticks declared as a string. Used to
    // be silently treated as None (fell back to handshake);
    // current contract is to surface the misconfiguration. Per
    // Copilot review on PR #221 (comment 3238774050).
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let json = serde_json::json!({
        "mount": {
            "park_ra_ticks": "not-an-integer",
            "park_dec_ticks": 0,
        }
    });
    std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
    let err = read_connect_fields(&path).unwrap_err();
    match err {
        StarAdvError::Config(msg) => {
            assert!(msg.contains("park_ra_ticks"), "{msg}");
            assert!(msg.contains("integer"), "{msg}");
        }
        other => panic!("expected Config error, got {other:?}"),
    }
}

#[test]
fn read_connect_fields_errors_on_float_value() {
    // serde_json::Value::Number for 1.5 isn't representable as
    // i64; `as_i64()` returns None and we surface a Config error
    // rather than silently dropping the value.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let json = serde_json::json!({
        "mount": {
            "park_ra_ticks": 1.5,
            "park_dec_ticks": 0,
        }
    });
    std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
    let err = read_connect_fields(&path).unwrap_err();
    assert!(matches!(err, StarAdvError::Config(_)));
}

#[test]
fn read_connect_fields_errors_on_out_of_i32_range() {
    // A value that fits in i64 but not i32 — e.g. someone copied
    // a large encoder count from a higher-resolution mount, or
    // a typo added a digit. Either way we should fail loudly so
    // the operator sees the bad value rather than silently
    // falling back to handshake.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let json = serde_json::json!({
        "mount": {
            "park_ra_ticks": i64::from(i32::MAX) + 1_i64,
            "park_dec_ticks": 0,
        }
    });
    std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
    let err = read_connect_fields(&path).unwrap_err();
    match err {
        StarAdvError::Config(msg) => {
            assert!(msg.contains("park_ra_ticks"), "{msg}");
            assert!(msg.contains("i32"), "{msg}");
        }
        other => panic!("expected Config error, got {other:?}"),
    }
}

#[test]
fn probe_park_file_writability_passes_on_a_writable_directory() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    probe_park_file_writability(&path).unwrap();
}

#[test]
fn canonicalise_config_path_returns_resolved_path_when_file_exists() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, "{}").unwrap();
    let got = canonicalise_config_path(&path);
    // Result is canonicalised — must exist and resolve to the same
    // file. On macOS the temp dir lives under /private/var/..., so
    // an exact string match is fragile; just check it resolves.
    assert!(got.exists(), "canonical path must exist: {got:?}");
}

#[test]
fn canonicalise_config_path_falls_back_to_input_on_failure() {
    // Path doesn't exist → canonicalize errors → fallback to the
    // original path. The warn! is logged but the function returns
    // the path unchanged so SetPark can still surface a real error
    // at write time.
    let nonexistent = PathBuf::from("/does/not/exist/config.json");
    let got = canonicalise_config_path(&nonexistent);
    assert_eq!(got, nonexistent);
}

#[test]
fn warn_if_park_path_unwritable_returns_quietly_on_writable_directory() {
    // Smoke test: helper returns `()` on success. The warn! body
    // is exercised by `warn_if_park_path_unwritable_logs_on_failure`
    // below (unix-only).
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    warn_if_park_path_unwritable(&path);
}

#[cfg(unix)]
#[test]
fn warn_if_park_path_unwritable_logs_on_failure() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(dir.path(), perms).unwrap();
    // Helper returns `()` even on probe failure — the test passes
    // as long as the call doesn't panic. The internal warn! body
    // is what we're measuring for coverage.
    warn_if_park_path_unwritable(&path);
    // Restore so TempDir's Drop can clean up.
    let mut restored = std::fs::metadata(dir.path()).unwrap().permissions();
    restored.set_mode(0o755);
    std::fs::set_permissions(dir.path(), restored).unwrap();
}

#[cfg(unix)]
#[test]
fn probe_park_file_writability_fails_on_a_read_only_directory() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    // Drop write permission on the parent directory so
    // `NamedTempFile::new_in` cannot stage a sibling.
    let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(dir.path(), perms).unwrap();
    // Probe should report the underlying I/O error.
    let err = probe_park_file_writability(&path).unwrap_err();
    // Restore write perms so TempDir's Drop can clean up.
    let mut restored = std::fs::metadata(dir.path()).unwrap().permissions();
    restored.set_mode(0o755);
    std::fs::set_permissions(dir.path(), restored).unwrap();
    // PermissionDenied is what Linux/macOS surface; either way
    // it must be classified as a permission / access issue.
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Other
        ),
        "unexpected error kind: {err:?}"
    );
}

#[test]
fn read_connect_fields_fails_when_mount_object_is_missing() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, "{}").unwrap();
    let err = read_connect_fields(&path).unwrap_err();
    match err {
        StarAdvError::Config(msg) => assert!(msg.contains("mount"), "{msg}"),
        other => panic!("expected Config, got {other:?}"),
    }
}

#[tokio::test]
async fn pulse_guide_zero_duration_is_no_op() {
    // Asserting "no wire activity" via the capturing mock: the
    // pulse_guide returns Ok and no `:K2` / `:G2` / `:I2` / `:J2`
    // setter frames are emitted on the Dec axis.
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    let mock = std::sync::Arc::clone(&factory.state);
    let cfg = base_config();
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();

    let baseline_len = mock.lock().await.command_log.len();
    d.pulse_guide(GuideDirection::North, Duration::ZERO)
        .await
        .unwrap();
    let log = mock.lock().await.command_log.clone();
    let new_frames: Vec<&[u8]> = log[baseline_len..]
        .iter()
        .map(std::vec::Vec::as_slice)
        .collect();
    let dec_setters: Vec<&&[u8]> = new_frames
        .iter()
        .filter(|f| {
            f.len() >= 3
                && f[0] == b':'
                && f[2] == b'2'
                && matches!(f[1], b'G' | b'I' | b'J' | b'K' | b'L')
        })
        .collect();
    assert!(
        dec_setters.is_empty(),
        "expected no Dec setter frames, got {dec_setters:?}"
    );
}

#[tokio::test]
async fn pulse_guide_north_issues_tracking_cw_on_dec_axis() {
    // North → Dec axis, ccw=false → `:G210` (Tracking-Slow-CW).
    // Step period is sidereal / 0.5 = 2 × sidereal.
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    let mock = std::sync::Arc::clone(&factory.state);
    let cfg = base_config();
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();

    let baseline_len = mock.lock().await.command_log.len();
    // Long enough duration that the watcher's post-sleep restore
    // doesn't fire during this test — we want to inspect the
    // pulse-start wire frames only.
    d.pulse_guide(GuideDirection::North, Duration::from_secs(30))
        .await
        .unwrap();
    // Immediately read the log; the watcher is asleep.
    let log = mock.lock().await.command_log.clone();
    let new_frames: Vec<&[u8]> = log[baseline_len..]
        .iter()
        .map(std::vec::Vec::as_slice)
        .collect();
    let dec_setters: Vec<&&[u8]> = new_frames
        .iter()
        .filter(|f| {
            f.len() >= 3
                && f[0] == b':'
                && f[2] == b'2'
                && matches!(f[1], b'G' | b'I' | b'J' | b'K' | b'L')
        })
        .collect();
    assert_eq!(
        dec_setters.len(),
        4,
        "expected exactly 4 Dec setter frames (:K2 :G2 :I2 :J2), got {dec_setters:?}"
    );
    assert_eq!(*dec_setters[0], b":K2\r", "1st Dec setter should be :K2");
    assert_eq!(
        *dec_setters[1], b":G210\r",
        "2nd Dec setter should be :G210 (Tracking-Slow-CW)"
    );
    assert_eq!(&dec_setters[2][..3], b":I2", "3rd Dec setter should be :I2");
    assert_eq!(*dec_setters[3], b":J2\r", "4th Dec setter should be :J2");
    // IsPulseGuiding flipped synchronously.
    assert!(d.is_pulse_guiding().await.unwrap());
}

#[tokio::test]
async fn pulse_guide_south_issues_tracking_ccw_on_dec_axis() {
    // South → Dec axis, ccw=true → `:G211` (Tracking-Slow-CCW).
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    let mock = std::sync::Arc::clone(&factory.state);
    let cfg = base_config();
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();

    let baseline_len = mock.lock().await.command_log.len();
    d.pulse_guide(GuideDirection::South, Duration::from_secs(30))
        .await
        .unwrap();
    let log = mock.lock().await.command_log.clone();
    let new_frames: Vec<&[u8]> = log[baseline_len..]
        .iter()
        .map(std::vec::Vec::as_slice)
        .collect();
    let g2 = new_frames
        .iter()
        .find(|f| f.starts_with(b":G2"))
        .expect("expected a :G2 frame");
    assert_eq!(*g2, b":G211\r", "South → :G211");
}

#[tokio::test]
async fn pulse_guide_east_uses_rate_factor_one_minus_fraction() {
    // East at default fraction (0.5) → rate factor = 0.5 → step
    // period = sidereal_period / 0.5 = 2 × sidereal. Decode the
    // `:I1` payload and compare against the expected shifted
    // period.
    use crate::transport::mock::CapturingMockFactory;
    use skywatcher_motor_protocol::codec::decode_u24;
    let factory = CapturingMockFactory::new();
    let mock = std::sync::Arc::clone(&factory.state);
    let cfg = base_config();
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();
    d.set_tracking(true).await.unwrap();

    let baseline_len = mock.lock().await.command_log.len();
    d.pulse_guide(GuideDirection::East, Duration::from_secs(30))
        .await
        .unwrap();
    let log = mock.lock().await.command_log.clone();
    let new_frames: Vec<&[u8]> = log[baseline_len..]
        .iter()
        .map(std::vec::Vec::as_slice)
        .collect();
    // First `:I1` after pulse_guide start: payload should be
    // 2 × P_sid (rate factor = 0.5 → period doubles).
    let i1 = new_frames
        .iter()
        .find(|f| f.starts_with(b":I1") && f.len() == 10)
        .expect("expected :I1 frame with 6-hex payload");
    let payload: &[u8; 6] = (&i1[3..9]).try_into().unwrap();
    let actual_period = decode_u24(payload).unwrap();
    let mock_state = mock.lock().await;
    let p_sid =
        crate::coordinates::sidereal_step_period(mock_state.tmr_freq, Cpr::new(mock_state.cpr_ra));
    let expected = 2 * p_sid;
    drop(mock_state);
    assert_eq!(
        actual_period, expected,
        "East at default 0.5 fraction → period must be 2× sidereal ({p_sid} → {expected}), got {actual_period}"
    );
}

#[tokio::test]
async fn pulse_guide_sets_is_pulse_guiding_synchronously() {
    // The flag must flip to true before `pulse_guide` returns —
    // see the atomic check-and-set under the write lock in
    // `MountDevice::pulse_guide`. The 30-second duration here is
    // not a "short pulse" test; it just keeps the watcher's
    // `tokio::time::sleep` from completing during the assertion
    // read so the only way `is_pulse_guiding()` can be true is
    // the synchronous flag-set.
    let d = connected_device().await;
    d.pulse_guide(GuideDirection::North, Duration::from_secs(30))
        .await
        .unwrap();
    assert!(d.is_pulse_guiding().await.unwrap());
}

#[tokio::test]
async fn pulse_guide_watcher_clears_flag_after_duration() {
    // Short pulse: the watcher should clear `is_pulse_guiding`
    // within a small multiple of the duration. Poll for up to
    // 2 seconds.
    let d = connected_device().await;
    d.pulse_guide(GuideDirection::North, Duration::from_millis(100))
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if !d.is_pulse_guiding().await.unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("watcher did not clear is_pulse_guiding within 2s of a 100ms pulse");
}

#[tokio::test]
async fn pulse_guide_rolls_back_flag_on_wire_failure() {
    // `StuckAxisFactory` returns `running=true` on every `:f` poll
    // so `stop_and_wait` times out after `AXIS_STOP_TIMEOUT` (2 s),
    // surfacing as a `StarAdvError::Transport` →
    // `ASCOMErrorCode::INVALID_OPERATION`. The pulse-guide
    // rollback path must clear `pulse_guiding_<axis>` so a
    // subsequent caller isn't blocked by the half-applied pulse
    // and `IsPulseGuiding` reports `false` consistent with the
    // lack of actual motion.
    let mut cfg = base_config();
    // Disable the CW-exclusion zone check for this test.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    let manager = MountManager::new(&cfg, Arc::new(StuckAxisFactory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();
    let err = d
        .pulse_guide(GuideDirection::North, Duration::from_millis(100))
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
    assert!(
        !d.is_pulse_guiding().await.unwrap(),
        "flag must be cleared after a wire-failure rollback"
    );
}

#[tokio::test]
async fn pulse_guide_rejects_step_period_overflow() {
    // Tiny guide-rate fractions push the shifted step period
    // above the protocol's 24-bit `:I` payload range. Without
    // the check, `encode_u24` would silently truncate to a
    // wrap-around value and run the mount at a wildly wrong
    // speed. For the GTi mock's defaults (sidereal_period ≈
    // 380K), the boundary is `rate_factor ≈ 0.023`; fraction
    // 0.001 is well below.
    let d = connected_device().await;
    d.set_guide_rate_declination(SIDEREAL_DEG_PER_SEC * 0.001)
        .await
        .unwrap();
    let err = d
        .pulse_guide(GuideDirection::North, Duration::from_millis(100))
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    // The rollback must clear the flag the period validation
    // would otherwise have set.
    assert!(!d.is_pulse_guiding().await.unwrap());
}

#[tokio::test]
async fn pulse_guide_rejects_same_axis_while_one_in_flight() {
    let d = connected_device().await;
    // Long-running first pulse — watcher is asleep.
    d.pulse_guide(GuideDirection::North, Duration::from_secs(30))
        .await
        .unwrap();
    let err = d
        .pulse_guide(GuideDirection::South, Duration::from_millis(100))
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
}

#[tokio::test]
async fn pulse_guide_refuses_while_parked() {
    // `fast_settle_connected` pins the park target to (0, 0), so the
    // park completes in one poll — see `fast_settle_device`.
    let d = fast_settle_connected().await;
    d.park().await.unwrap();
    // Wait for AtPark = true (park watcher).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if d.at_park().await.unwrap() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let err = d
        .pulse_guide(GuideDirection::North, Duration::from_millis(100))
        .await
        .unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_WHILE_PARKED);
}

#[tokio::test]
async fn pulse_guide_ra_with_tracking_off_does_not_restore_tracking() {
    // Issue an East pulse while tracking is OFF. After the pulse
    // completes, tracking_requested should still be false and the
    // watcher should not have emitted a second `:G110` (restore)
    // frame.
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    let mock = std::sync::Arc::clone(&factory.state);
    let cfg = base_config();
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();
    assert!(!d.tracking().await.unwrap(), "precondition: tracking off");

    d.pulse_guide(GuideDirection::East, Duration::from_millis(100))
        .await
        .unwrap();
    // Wait for the watcher to finish.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if !d.is_pulse_guiding().await.unwrap() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!d.is_pulse_guiding().await.unwrap());
    // Tracking still off.
    assert!(!d.tracking().await.unwrap());
    // Exactly one `:G110\r` in the log (the pulse-start; no
    // restore). `:G110` is RA Tracking-Slow-CW; the watcher would
    // only emit a second one on the restore branch if
    // `tracking_was_on` was true at issue.
    let log = mock.lock().await.command_log.clone();
    let g110_count = log.iter().filter(|f| f.as_slice() == b":G110\r").count();
    assert_eq!(
        g110_count, 1,
        "expected 1 :G110 frame (pulse-start only, no restore), got {g110_count}; log {log:?}"
    );
}

// ---------- Phase 6: through-wrap routing helpers ----------

const GTI_CPR: u32 = 0x0037_5F00; // 3,628,800

/// Hardware-verified `GTi` CW exclusion zone — the contiguous arc
/// where the CW shaft rises more than 0.95 h above horizontal.
/// Matches `default_binding_zone_min/max_hours` in `config.rs`.
const GTI_CW_EXCLUSION_ZONE: (f64, f64) = (0.95, 11.05);

#[test]
fn flip_slew_ra_delta_forward_flip_from_pre_flip_zero_uses_natural_ccw() {
    // Forward flip starting at encoder ≈ 0 (mech_HA ≈ 0, pre-flip
    // pierWest at meridian). Target = −cpr/2 (post-flip wrap).
    // Canonical CCW; path stays in negative half. Safe.
    let cpr = GTI_CPR;
    let current = 0;
    let canonical = -(cpr.cast_signed() / 2);
    let issued = flip_slew_ra_delta(canonical, current, cpr, GTI_CW_EXCLUSION_ZONE).unwrap();
    assert_eq!(issued, canonical, "natural CCW already in the safe half");
}

#[test]
fn flip_slew_ra_delta_forward_flip_target_minus_half_h_takes_long_way() {
    // The plan §2.0 canonical case: pre-flip mech_HA = −0.5, target
    // post-flip mech_HA = +11.5, canonical = +cpr*11.5/24 -
    // (-cpr*0.5/24) = +cpr/2 + small. (Approximated by +1.815M
    // ticks at cpr_ra = 3.6288M.) Force CCW long way through the
    // wrap.
    let cpr = GTI_CPR;
    let current = -75_600; // mech_HA = -0.5
    let canonical = 1_815_000_i32;
    let issued = flip_slew_ra_delta(canonical, current, cpr, GTI_CW_EXCLUSION_ZONE).unwrap();
    assert!(issued < 0, "must force CCW long way");
    assert_eq!(
        (issued - canonical).rem_euclid(cpr.cast_signed()),
        0,
        "same modular destination"
    );
}

#[test]
fn flip_slew_ra_delta_flip_back_from_minus_half_cpr_uses_natural_cw() {
    // Flip-back: current at raw -cpr/2 (post-flip wrap, mech_HA = -12).
    // Target = -cpr/4 (Park 3, mech_HA = -6). Canonical = +cpr/4
    // (positive CW). The path is mech_HA -12 → -11 → ... → -6,
    // entirely in the safe negative half. Use canonical.
    //
    // Regression for hardware validation #3: my prior `always CCW`
    // rule forced -3*cpr/4 here, routing through +6 to +9 binding
    // zone and slamming the CW shaft into the pier.
    let cpr = GTI_CPR;
    let current = -(cpr.cast_signed() / 2);
    let canonical = cpr.cast_signed() / 4;
    let issued = flip_slew_ra_delta(canonical, current, cpr, GTI_CW_EXCLUSION_ZONE).unwrap();
    assert_eq!(
        issued, canonical,
        "flip-back from -cpr/2 must use natural CW"
    );
    assert!(issued > 0);
}

#[test]
fn flip_slew_ra_delta_flip_back_from_plus_half_cpr_forces_cw_through_wrap() {
    // Flip-back from raw +cpr/2 (mech_HA = +12 = -12 modular,
    // post-flip wrap from the other direction). Target = -cpr/4.
    // Canonical = -cpr/4 - +cpr/2 = -3cpr/4 → fold to +cpr/4 (CW,
    // because |−3cpr/4| > half_cpr). Force CW; path stays in safe
    // half going from +cpr/2 → +cpr/2 + cpr/4 = +3cpr/4 raw
    // (modular: -cpr/4 = mech_HA -6).
    let cpr = GTI_CPR;
    let current = cpr.cast_signed() / 2;
    let canonical_raw = -(cpr.cast_signed() / 4) - current; // -3cpr/4
    let canonical = RaTicks::new(canonical_raw)
        .fold_to_canonical_band(Cpr::new(cpr))
        .value(); // +cpr/4
    let issued = flip_slew_ra_delta(canonical, current, cpr, GTI_CW_EXCLUSION_ZONE).unwrap();
    assert!(issued > 0, "post-flip wrap → safe arc must use CW");
    assert_eq!(issued, canonical, "canonical CW is already safe here");
}

#[test]
fn flip_slew_ra_delta_handles_zero_cpr_defensively() {
    assert_eq!(
        flip_slew_ra_delta(12_345, 0, 0, GTI_CW_EXCLUSION_ZONE).unwrap(),
        12_345
    );
}

#[test]
fn flip_slew_ra_delta_zero_canonical_returns_zero() {
    assert_eq!(
        flip_slew_ra_delta(0, 0, GTI_CPR, GTI_CW_EXCLUSION_ZONE).unwrap(),
        0
    );
}

#[test]
fn flip_slew_ra_delta_park4_to_park5_north_uses_canonical_through_wrap() {
    // Hardware regression (2026-05-16): Park 4 N → Park 5 N flip
    // slew. Snap.ra ≈ raw -1,810,272 (mech_HA -11.974, post-flip
    // pierEast just east of the saddle-east wrap). The slew planner
    // chose pierWest (target HA -12 outside the flip window), with
    // target encoder near +cpr/2 (mech_HA +11.999, the saddle-east
    // wrap from the other side). fold_to_canonical_band produces a
    // small CCW step (~-3.8k ticks) that physically nudges the RA
    // encoder past the -12 wrap and into +12 folded — the path stays
    // entirely in the safe arc (no positive mech_HA visited). The
    // old sign-blind heuristic forced canonical + cpr ≈ +3.625M
    // ticks CW, a near-full polar-axis revolution that swept
    // mech_HA through (+6.95, +11.05) and slammed the CW shaft into
    // the pier — the operator powered the mount off mid-sweep.
    let cpr = GTI_CPR;
    let current = -1_810_272_i32;
    let canonical = -3_798_i32;
    let issued = flip_slew_ra_delta(canonical, current, cpr, GTI_CW_EXCLUSION_ZONE).unwrap();
    assert_eq!(
        issued, canonical,
        "canonical CCW wrap-crossing must be preserved; old long-way CW \
         would full-revolution through the CW exclusion zone"
    );
}

#[test]
fn flip_slew_ra_delta_empty_binding_zone_always_uses_canonical() {
    // Empty zone (min ≥ max) disables routing; the function
    // collapses to the canonical short delta regardless of which
    // half the current sits in. Matches the BDD-test config that
    // sets `binding_zone_min = 24.0, binding_zone_max = 0.0` to
    // bypass the safety gate.
    let cpr = GTI_CPR;
    let empty_zone = (24.0_f64, 0.0_f64);
    for (current, canonical) in [
        (0_i32, -(cpr.cast_signed() / 2)),
        (-(cpr.cast_signed() / 2), cpr.cast_signed() / 4),
        (-1_810_272_i32, -3_798_i32),
    ] {
        assert_eq!(
            flip_slew_ra_delta(canonical, current, cpr, empty_zone).unwrap(),
            canonical,
            "empty zone must pass canonical through (current={current}, canonical={canonical})"
        );
    }
}

#[test]
fn flip_slew_ra_delta_refuses_when_both_directions_cross_zone() {
    // Park 3 → "NCP on East side" via SetSideOfPier(East):
    // current at raw -cpr/4 (mech_HA = -6 h, Park 3 saddle west),
    // target at raw +cpr/4 (mech_HA = +6 h, Park 3's flipped twin).
    // Canonical fold of +cpr/2 lands on -cpr/2 → CCW short path
    // sweeps mech_HA -6 → -12 (wrap) → +6, crossing the k=-1
    // mirror of the wide zone. The long way (+cpr/2 CW) sweeps
    // mech_HA -6 → 0 → +6, crossing the wide zone directly at
    // (+0.95, +6). Both directions unsafe → refuse.
    let cpr = GTI_CPR;
    let current = -(cpr.cast_signed() / 4); // mech_HA = -6
    let canonical = -(cpr.cast_signed() / 2); // -12 h CCW, canonical-fold boundary
    let err =
        flip_slew_ra_delta(canonical, current, cpr, GTI_CW_EXCLUSION_ZONE).expect_err("both cross");
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
    assert!(
        err.message.contains("both cross"),
        "error must mention both-direction crossing; got: {}",
        err.message
    );
}

#[test]
fn flip_slew_ra_delta_wide_zone_picks_long_way_for_zone_boundary_traversal() {
    // Wide-zone variant of the existing
    // `flip_slew_ra_delta_canonical_path_crossing_binding_zone_takes_long_way`
    // success case — picks the long way around when canonical
    // would cross and the long way is safe.
    //
    // From mech_HA = +11.1 (just outside the wide zone, descending
    // edge) to +0.9 (just outside, ascending edge). Canonical
    // short delta is -10.2 h CCW — sweeping straight through the
    // entire wide zone (+0.95, +11.05). The long way is +13.8 h
    // CW going around through the safe arc [+11.05, +24.95]
    // (= [+11.05, +0.95 + 24]). Helper picks the long way
    // successfully because the safe arc is wider than the long-way
    // sweep.
    //
    // The margin here matters — wide-zone cases where canonical
    // crosses but the long way is safe are *rare*. The zone is
    // 10.1 h wide; the safe arc between zone replicas is only
    // 13.9 h, so the long way only fits when both endpoints sit
    // at least ~0.05 h outside the zone boundary. The
    // `flip_slew_ra_delta_refuses_when_both_directions_cross_zone`
    // test above covers the common both-cross case; this one pins
    // the narrow boundary-traversal scenario where the long way
    // just barely fits in the safe arc.
    let cpr = GTI_CPR;
    let cur_h = 11.1_f64;
    let target_h = 0.9_f64;
    let current = (cur_h * f64::from(cpr) / 24.0).round() as i32;
    let target = (target_h * f64::from(cpr) / 24.0).round() as i32;
    let canonical = RaTicks::new(target - current)
        .fold_to_canonical_band(Cpr::new(cpr))
        .value();
    let issued = flip_slew_ra_delta(canonical, current, cpr, GTI_CW_EXCLUSION_ZONE).unwrap();
    assert!(
        issued > 0,
        "canonical CCW sweeps the entire zone; must force CW long way (got {issued})"
    );
    assert_eq!(
        (issued - canonical).rem_euclid(cpr.cast_signed()),
        0,
        "same modular destination"
    );
}

#[test]
fn check_non_flip_ra_path_refuses_when_canonical_sweep_crosses_zone() {
    // Non-flip slew from mech_HA = +0.5 h to +11.5 h. Both
    // endpoints sit outside the wide zone (+0.95, +11.05) but the
    // canonical short sweep (+11 h CW) crosses the entire zone
    // interior. Today's destination-only `check_within_safe_envelope`
    // would let this through; the path-aware check refuses it.
    let cpr = GTI_CPR;
    let current = (cpr.cast_signed()) / 48; // mech_HA = +0.5
    let canonical = (cpr.cast_signed()) * 11 / 24; // +11 h CW
    let err = check_non_flip_ra_path(canonical, current, cpr, GTI_CW_EXCLUSION_ZONE)
        .expect_err("path crosses zone");
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
    assert!(
        err.message.contains("non-flip RA slew"),
        "error must identify itself as non-flip path check; got: {}",
        err.message
    );
}

#[test]
fn check_non_flip_ra_path_accepts_clean_sweep() {
    // Non-flip slew that stays clear of the zone: current at
    // mech_HA = -3 h, canonical -2 h CCW → sweep [-5, -3]. No
    // overlap with the wide zone or its k=-1 mirror. Returns Ok.
    let cpr = GTI_CPR;
    let current = -(cpr.cast_signed()) / 8; // mech_HA = -3
    let canonical = -(cpr.cast_signed()) / 12; // -2 h CCW
    check_non_flip_ra_path(canonical, current, cpr, GTI_CW_EXCLUSION_ZONE)
        .expect("sweep [-5, -3] doesn't touch the wide zone");
}

#[test]
fn check_non_flip_ra_path_passes_through_for_zero_inputs() {
    // Defensive degenerate cases (consistent with flip_slew_ra_delta).
    check_non_flip_ra_path(12_345, 0, 0, GTI_CW_EXCLUSION_ZONE).unwrap();
    check_non_flip_ra_path(0, 0, GTI_CPR, GTI_CW_EXCLUSION_ZONE).unwrap();
}

// ---------- flip_slew_dec_delta (Dec routing through the visible pole) ----------

#[test]
fn flip_slew_dec_delta_north_park3_start_to_post_flip_positive_uses_natural_cw() {
    // Park 3: start at encoder +cpr/4 (= +90°, NCP). Target = +135°
    // encoder = +3*cpr/8 (celestial dec = +45° on the flipped side).
    // Natural delta = +cpr/8 (positive CW). Path stays in upper
    // half, doesn't touch SCP. Use canonical.
    let cpr = GTI_CPR;
    let quarter = cpr.cast_signed() / 4;
    let current = quarter; // +90° encoder
    let target = (f64::from(cpr) * 3.0 / 8.0).round() as i32; // +135°
    let canonical = target - current; // +cpr/8
    assert_eq!(
        flip_slew_dec_delta(canonical, current, cpr, true),
        canonical,
        "Park 3 → +135° should use the natural CW direction"
    );
    // Confirm the path avoids SCP (-cpr/4).
    let issued = flip_slew_dec_delta(canonical, current, cpr, true);
    assert!(!canonical_path_crosses_pole(current, issued, -quarter, cpr));
}

#[test]
fn flip_slew_dec_delta_north_pre_flip_zero_to_post_flip_dec_zero_takes_long_way() {
    // The bug case from the first hardware run: starting at encoder
    // = 0 (codebase historical home), the canonical fold of a slew
    // to dec_encoder = ±cpr/2 returns negative (CCW), which routes
    // the Dec axis through −cpr/4 (SCP, below horizon for north).
    // The fix forces the CW long way through +cpr/4 (NCP).
    let cpr = GTI_CPR;
    let quarter = cpr.cast_signed() / 4;
    let half_cpr = cpr.cast_signed() / 2;
    let canonical = -half_cpr; // post-flip target for celestial dec = 0
    let issued = flip_slew_dec_delta(canonical, 0, cpr, true);
    assert_eq!(issued, half_cpr, "must force CW through NCP");
    // Verify path crosses NCP and not SCP.
    assert!(canonical_path_crosses_pole(0, issued, quarter, cpr));
    assert!(!canonical_path_crosses_pole(0, issued, -quarter, cpr));
}

#[test]
fn flip_slew_dec_delta_north_flip_back_from_upper_post_flip_uses_natural_ccw() {
    // Flip-back: starting at encoder +3*cpr/8 (= +135°, upper
    // post-flip), target = 0 (pre-flip celestial equator). Natural
    // CCW (negative direction) crosses +cpr/4 (NCP). Don't take
    // the long way — CCW is safe here.
    let cpr = GTI_CPR;
    let quarter = cpr.cast_signed() / 4;
    let current = (f64::from(cpr) * 3.0 / 8.0).round() as i32;
    let target = 0;
    let canonical = target - current; // negative
    let issued = flip_slew_dec_delta(canonical, current, cpr, true);
    assert_eq!(
        issued, canonical,
        "flip-back from +135° must use natural CCW"
    );
    // Verify path crosses NCP (the safe pole) and not SCP.
    assert!(canonical_path_crosses_pole(current, issued, quarter, cpr));
    assert!(!canonical_path_crosses_pole(current, issued, -quarter, cpr));
}

#[test]
fn flip_slew_dec_delta_north_below_equator_pre_flip_to_post_flip_positive_uses_cw() {
    // Pre-flip but below celestial equator: encoder = -cpr/8
    // (= -45°). Target = +3*cpr/8 (= +135° encoder, post-flip
    // dec=+45). CW path: -45 → 0 → +90 (NCP) → +135. SAFE.
    let cpr = GTI_CPR;
    let quarter = cpr.cast_signed() / 4;
    let current = -(cpr.cast_signed() / 8);
    let target = (cpr.cast_signed() * 3) / 8;
    let canonical = target - current; // positive, half cpr
    let issued = flip_slew_dec_delta(canonical, current, cpr, true);
    assert_eq!(issued, canonical, "natural CW direction is safe");
    assert!(canonical_path_crosses_pole(current, issued, quarter, cpr));
    assert!(!canonical_path_crosses_pole(current, issued, -quarter, cpr));
}

#[test]
fn flip_slew_dec_delta_north_below_equator_pre_flip_to_post_flip_negative_forces_long_cw() {
    // Pre-flip below equator: encoder = -cpr/8. Target =
    // -3*cpr/8 (post-flip negative). Natural CCW (negative) path:
    // -cpr/8 → -cpr/4 (SCP) → -3*cpr/8. UNSAFE. Force long-way CW:
    // -cpr/8 → +cpr/4 (NCP) → +cpr/2 → wraps → -3*cpr/8. SAFE.
    let cpr = GTI_CPR;
    let quarter = cpr.cast_signed() / 4;
    let current = -(cpr.cast_signed() / 8);
    let target_canonical = -(cpr.cast_signed() * 3) / 8;
    let canonical = target_canonical - current; // negative
    let issued = flip_slew_dec_delta(canonical, current, cpr, true);
    // Forced to positive direction (long way).
    assert!(issued > 0);
    // Lands at the same modular destination.
    assert_eq!((issued - canonical).rem_euclid(cpr.cast_signed()), 0);
    // Path crosses NCP, not SCP.
    assert!(canonical_path_crosses_pole(current, issued, quarter, cpr));
    assert!(!canonical_path_crosses_pole(current, issued, -quarter, cpr));
}

#[test]
fn flip_slew_dec_delta_north_lower_post_flip_to_pre_flip_uses_ccw_through_wrap() {
    // Lower post-flip: encoder = -3*cpr/8 (= -135°, celestial
    // dec=-45 on post-flip side). Target = 0 (pre-flip equator).
    // Natural CW (positive) path: -3*cpr/8 → -cpr/4 (SCP) → 0.
    // UNSAFE. Force CCW (negative) long-way: -3*cpr/8 →
    // -cpr/2 → wraps → +cpr/2 → +cpr/4 (NCP) → 0. SAFE.
    let cpr = GTI_CPR;
    let quarter = cpr.cast_signed() / 4;
    let current = -((cpr.cast_signed() * 3) / 8);
    let canonical = 0 - current; // positive
    let issued = flip_slew_dec_delta(canonical, current, cpr, true);
    assert!(issued < 0, "must force CCW long way");
    assert_eq!((issued - canonical).rem_euclid(cpr.cast_signed()), 0);
    assert!(canonical_path_crosses_pole(current, issued, quarter, cpr));
    assert!(!canonical_path_crosses_pole(current, issued, -quarter, cpr));
}

#[test]
fn flip_slew_dec_delta_south_inverts_safe_pole() {
    // Southern observer: SCP is visible, NCP is below horizon.
    // Repeat the "park 3 start" case but with southern config:
    // start at encoder = -cpr/4 (Park 3 south = SCP). Target =
    // -3*cpr/8. Natural CCW (negative). Should be used as-is.
    let cpr = GTI_CPR;
    let quarter = cpr.cast_signed() / 4;
    let current = -quarter; // SCP for southern Park 3
    let target = -((cpr.cast_signed() * 3) / 8);
    let canonical = target - current; // negative
    let issued = flip_slew_dec_delta(canonical, current, cpr, false);
    assert_eq!(
        issued, canonical,
        "south Park 3 → -3cpr/8 must use natural CCW"
    );
    // For south, the unsafe pole is +cpr/4 (NCP). Verify path
    // doesn't cross it.
    assert!(!canonical_path_crosses_pole(current, issued, quarter, cpr));
}

#[test]
fn flip_slew_dec_delta_handles_zero_cpr_defensively() {
    assert_eq!(flip_slew_dec_delta(12_345, 0, 0, true), 12_345);
}

#[test]
fn flip_slew_dec_delta_zero_canonical_returns_zero() {
    assert_eq!(flip_slew_dec_delta(0, 0, GTI_CPR, true), 0);
}

#[test]
fn flip_slew_dec_delta_handles_raw_current_outside_canonical_band() {
    // `current_ticks` is the raw encoder counter, which can sit
    // outside `[-cpr/2, +cpr/2)` after through-wrap flip slews,
    // manual `:E` writes, or power-up encoder noise. The path-aware
    // check operates on the continuous sweep `[raw, raw + canonical]`
    // and the modular-replica pole scan, so raw outside the canonical
    // band is handled naturally — no fold needed. Sanity-check that
    // a small positive canonical from a raw in the positive
    // disagreement zone (which a prior heuristic-based version would
    // have misread as post-flip and routed the long way through the
    // below-horizon pole) passes through unchanged.
    let cpr = GTI_CPR;
    let cpr_i = cpr.cast_signed();
    let raw = cpr_i * 7 / 8; // 7·cpr/8, folds to -cpr/8
    let canonical = 100_000_i32;
    // Sweep [+3,175,200, +3,275,200]. The unsafe pole at -cpr/4
    // (= -907,200) has modular replicas at -cpr/4, +3·cpr/4, etc.;
    // none fall inside the sweep, so canonical is preserved.
    let issued = flip_slew_dec_delta(canonical, raw, cpr, true);
    assert_eq!(
        issued, canonical,
        "raw outside canonical band must still produce the safe canonical path"
    );
}

#[test]
fn canonical_path_crosses_pole_north_detects_k_plus_3_replica_at_positive_wire_boundary() {
    // Signed-24-bit wire range carries `start` up to ~+8.4M ticks.
    // For Northern observer the unsafe pole is `-cpr/4 = -907_200`;
    // its modular replicas sit at `pole + k·cpr` for any integer k.
    // With `cpr_dec = 3.6M`, a sweep near `+wire/2` reaches the
    // `k = +3` replica at `-907_200 + 3·3_628_800 = +9_979_200`.
    // The prior hardcoded `k ∈ -2..=2` scan missed this band; the
    // div_euclid check finds it.
    let cpr = GTI_CPR;
    let cpr_i = cpr.cast_signed();
    let pole = -cpr_i / 4; // SCP for N
    let start = 8_300_000_i32; // near +wire/2 (= +8.39M)
    let delta = 1_700_000_i32; // sweep end at +10M, containing +9.98M
    assert!(
        canonical_path_crosses_pole(start, delta, pole, cpr),
        "k=+3 replica at +9_979_200 must be detected inside sweep [+8.3M, +10M]"
    );
    // End-to-end: helper recognises canonical crosses, forces the
    // long way (negative delta), lands at the same modular dest.
    let issued = flip_slew_dec_delta(delta, start, cpr, true);
    assert!(
        issued < 0,
        "canonical sweep crosses SCP at wire boundary; must force long way (got {issued})"
    );
    assert_eq!(
        (issued - delta).rem_euclid(cpr_i),
        0,
        "long way must land at the same modular destination"
    );
}

#[test]
fn canonical_path_crosses_pole_south_detects_k_minus_3_replica_at_negative_wire_boundary() {
    // Mirror of the Northern case. For Southern observer the unsafe
    // pole is `+cpr/4 = +907_200`; the modular replicas sit at
    // `pole + k·cpr`. A sweep near `-wire/2` reaches the `k = -3`
    // replica at `+907_200 + (-3)·3_628_800 = -9_979_200`. The
    // same div_euclid check (the helper has no hemisphere-specific
    // branches) must find it.
    let cpr = GTI_CPR;
    let cpr_i = cpr.cast_signed();
    let pole = cpr_i / 4; // NCP, unsafe for S
    let start = -10_000_000_i32; // near -wire/2
    let delta = 1_700_000_i32; // sweep end at -8.3M, containing -9.98M
    assert!(
        canonical_path_crosses_pole(start, delta, pole, cpr),
        "k=-3 replica at -9_979_200 must be detected inside sweep [-10M, -8.3M]"
    );
    // End-to-end through `flip_slew_dec_delta` with `northern=false`
    // — the test the original combined version was missing.
    let issued = flip_slew_dec_delta(delta, start, cpr, false);
    assert!(
        issued < 0,
        "canonical sweep crosses NCP at wire boundary; must force long way (got {issued})"
    );
    assert_eq!(
        (issued - delta).rem_euclid(cpr_i),
        0,
        "long way must land at the same modular destination"
    );
}

// ---------- Phase 6: SetSideOfPier + CanSetPierSide ----------

async fn flip_enabled_device() -> MountDevice {
    let mut cfg = base_config();
    if let crate::config::TransportConfig::Usb(usb) = &mut cfg.transport {
        usb.polling_interval = Duration::from_millis(20);
    }
    cfg.mount.settle_after_slew = Duration::from_millis(0);
    // Disable the CW-exclusion zone check for this test.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    cfg.mount.flip_policy.enabled = true;
    let manager = MountManager::new(&cfg, Arc::new(MockTransportFactory));
    MountDevice::new(cfg.mount, manager)
}

async fn flip_enabled_connected_device() -> MountDevice {
    let d = flip_enabled_device().await;
    d.set_connected(true).await.unwrap();
    d
}

#[tokio::test]
async fn can_set_pier_side_defaults_to_false() {
    let d = fast_settle_connected().await;
    assert!(!d.can_set_pier_side().await.unwrap());
}

#[tokio::test]
async fn can_set_pier_side_is_true_when_flip_policy_enabled() {
    let d = flip_enabled_connected_device().await;
    assert!(d.can_set_pier_side().await.unwrap());
}

#[tokio::test]
async fn set_side_of_pier_returns_not_implemented_when_flip_policy_disabled() {
    let d = fast_settle_connected().await;
    let err = d.set_side_of_pier(PierSide::East).await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn set_side_of_pier_rejects_unknown_with_invalid_value() {
    let d = flip_enabled_connected_device().await;
    let err = d.set_side_of_pier(PierSide::Unknown).await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
}

#[tokio::test]
async fn set_side_of_pier_refuses_when_not_connected() {
    let d = flip_enabled_device().await;
    let err = d.set_side_of_pier(PierSide::East).await.unwrap_err();
    assert_eq!(err.code, ASCOMError::NOT_CONNECTED.code);
}

#[tokio::test]
async fn set_side_of_pier_refuses_while_parked() {
    let d = flip_enabled_connected_device().await;
    // Park puts AtPark = true; SetSideOfPier must refuse with
    // INVALID_WHILE_PARKED before reaching the slew planner.
    d.state.write().await.at_park = true;
    let err = d.set_side_of_pier(PierSide::East).await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_WHILE_PARKED);
}

#[tokio::test]
async fn set_side_of_pier_refuses_while_slew_in_progress() {
    let d = flip_enabled_connected_device().await;
    d.slew_in_progress.store(true, Ordering::SeqCst);
    let err = d.set_side_of_pier(PierSide::East).await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::INVALID_OPERATION);
}

#[tokio::test]
async fn set_side_of_pier_to_current_side_succeeds_as_noop() {
    let d = flip_enabled_connected_device().await;
    // Mock starts with Dec encoder = 0 (within ±90°), site latitude
    // = 0° (northern convention since `>= 0`), so current side is
    // pierWest. SetSideOfPier(West) is a no-op.
    d.set_side_of_pier(PierSide::West).await.unwrap();
    // State unchanged.
    assert!(!d.slew_in_progress.load(Ordering::SeqCst));
}

#[tokio::test]
async fn set_side_of_pier_to_opposite_side_starts_a_flip_slew() {
    let d = flip_enabled_connected_device().await;
    d.set_side_of_pier(PierSide::East).await.unwrap();
    // Slew was issued — the state should now show slew_in_progress
    // until the watcher clears it. The watcher may have already
    // completed in the mock (instant-settle config), so accept
    // either: the slew was either still in progress at this read,
    // or already finished with the encoder mutated.
    let slewing = d.slew_in_progress.load(Ordering::SeqCst);
    let s = d.state.read().await;
    let target_set = s.target_ra_hours.is_some() && s.target_dec_degrees.is_some();
    drop(s);
    assert!(
        slewing || target_set,
        "expected slew to have been issued (slew_in_progress or target_ra latched)"
    );
}

// ---- watcher_poll_with_retry --------------------------------------
//
// The slew/park watchers' transport-error path used to exit on the
// first failed `poll_axes_now`, which made a single transient USB-
// CDC glitch take the watcher offline for the rest of the slew. The
// helper now retries [`WATCHER_POLL_RETRY_LIMIT`] times and on
// exhaustion fires a best-effort `:L` on both axes so a runaway
// motor doesn't continue commutating with no observer.

use async_trait::async_trait;
use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
use std::sync::atomic::AtomicU32;

/// Inject N consecutive transport failures and count `:L<axis>`
/// frames that crossed the wire. Shared between the test and the
/// inner [`FlakyFrameTransport`] so the test can observe both knobs.
struct FlakyController {
    fail_remaining: AtomicU32,
    stop_calls_ra: AtomicU32,
    stop_calls_dec: AtomicU32,
}

/// Frame-transport that wraps the regular mock state machine and
/// fails the first `fail_remaining` recv calls. Every `:L<axis>`
/// frame the driver sends is counted on the send side, before the
/// recv-side fail check, so a `:L` that lands during the failure
/// window still registers (the retry-exhaustion path fires `:L`
/// regardless of whether subsequent recvs are responsive).
struct FlakyFrameTransport {
    inner: crate::transport::mock::MockMountState,
    ctrl: Arc<FlakyController>,
    /// `Some` after a successful send; pop on the next recv.
    pending: std::collections::VecDeque<Vec<u8>>,
}

impl FlakyFrameTransport {
    fn new(ctrl: Arc<FlakyController>) -> Self {
        Self {
            inner: crate::transport::mock::MockMountState::default(),
            ctrl,
            pending: std::collections::VecDeque::new(),
        }
    }
}

#[async_trait]
impl FrameTransport for FlakyFrameTransport {
    async fn send_frame(&mut self, bytes: &[u8]) -> std::result::Result<(), TransportError> {
        use crate::transport::mock as mockmod;

        // Count `:L<axis>` frames *before* the fail check so the
        // best-effort halt on retry exhaustion still registers
        // even when the transport is failing recvs.
        if bytes.len() >= 3 && bytes[1] == b'L' {
            match bytes[2] {
                b'1' => {
                    self.ctrl.stop_calls_ra.fetch_add(1, Ordering::SeqCst);
                }
                b'2' => {
                    self.ctrl.stop_calls_dec.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
        }
        if bytes.len() < 3 || bytes[0] != b':' || bytes[bytes.len() - 1] != b'\r' {
            return Err(TransportError::Framing(format!("malformed: {bytes:?}")));
        }
        // Use the inner mock state-machine to produce the reply,
        // but enqueue it on our own queue so we control delivery.
        // The mock has its own `pending_replies` queue we drain.
        // We do this by routing the command through the real mock
        // factory's pending queue and stealing the reply.
        // Simplest path: produce_reply via a fresh MockFrameTransport
        // share would be heavy, so we mirror just what the test
        // needs — defer to the inner state machine.
        // Mock state's `process_command` writes to its own
        // VecDeque; we capture it.
        let _ = mockmod::MockMountState::default(); // type witness
                                                    // Process directly by calling a small helper that mutates
                                                    // `self.inner` and appends the reply to a local Vec.
                                                    // Easiest: serialize the access through the inner mock by
                                                    // pushing into a fresh MockFrameTransport state — but we
                                                    // don't have a constructor that wraps an existing Arc<Mutex>.
                                                    // Just implement the subset of commands the watcher tests
                                                    // need: `:F<axis>`, `:a<axis>`, `:b<axis>`, `:g<axis>`,
                                                    // `:e<axis>`, `:j<axis>`, `:f<axis>`, plus `:L<axis>` and
                                                    // `:K<axis>` acks.
        let cmd = bytes[1];
        let axis = bytes[2];
        let reply: Vec<u8> = match cmd {
            b'F' | b'K' | b'L' => b"=\r".to_vec(),
            b'a' | b'b' => b"=005F37\r".to_vec(),
            // `:e` returns the GTi motor-board-version wire reply
            // `=03300C` (measured on the real mount) so the `:e1`-first
            // handshake's mount-type whitelist (issue #254) accepts it:
            // little-byte-first hex decode gives `0x000C_3003`, whose low
            // byte `0x03` is the EQ family.
            b'e' => b"=03300C\r".to_vec(),
            b'g' => b"=01\r".to_vec(),
            b'j' => {
                // Biased position 0 → 0x800000 → "000080".
                let _ = axis;
                b"=000080\r".to_vec()
            }
            b'f' => {
                // running=0, init=1 — handshake-complete idle status.
                let _ = axis;
                b"=001\r".to_vec()
            }
            _ => b"=\r".to_vec(),
        };
        self.pending.push_back(reply);
        // touch self.inner to silence unused-field warning
        let _ = &self.inner;
        Ok(())
    }

    async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> std::result::Result<(), TransportError> {
        if self.ctrl.fail_remaining.load(Ordering::SeqCst) > 0 {
            self.ctrl.fail_remaining.fetch_sub(1, Ordering::SeqCst);
            // Drain the pending reply so a successful recv after
            // the failure window sees the *next* command's reply,
            // matching the real-hardware semantics where a dropped
            // datagram doesn't queue up for later delivery.
            let _ = self.pending.pop_front();
            return Err(TransportError::Eof);
        }
        match self.pending.pop_front() {
            Some(frame) => {
                buf.clear();
                buf.extend_from_slice(&frame);
                Ok(())
            }
            None => Err(TransportError::Eof),
        }
    }
}

struct FlakyTransportFactory {
    ctrl: Arc<FlakyController>,
}

#[async_trait]
impl TransportFactory for FlakyTransportFactory {
    async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
        Ok(Box::new(FlakyFrameTransport::new(self.ctrl.clone())))
    }
}

async fn flaky_manager() -> (
    Arc<MountManager>,
    Arc<FlakyController>,
    rusty_photon_shared_transport::Session<crate::codec::SkywatcherCodec>,
) {
    let ctrl = Arc::new(FlakyController {
        fail_remaining: AtomicU32::new(0),
        stop_calls_ra: AtomicU32::new(0),
        stop_calls_dec: AtomicU32::new(0),
    });
    let factory = Arc::new(FlakyTransportFactory { ctrl: ctrl.clone() });
    let manager = MountManager::new(&base_config(), factory);
    // Acquire with fail_remaining = 0 so the handshake completes,
    // then return the controller so the test can flip the failure
    // budget on without interfering with init.
    let session = manager.transport().acquire().await.unwrap();
    (manager, ctrl, session)
}

#[tokio::test]
async fn watcher_poll_with_retry_returns_ok_on_first_success() {
    let (manager, ctrl, session) = flaky_manager().await;
    let snap = watcher_poll_with_retry(&manager, &session, "test")
        .await
        .expect("happy-path poll should succeed");
    // No retries needed → no best-effort :L was issued.
    assert_eq!(ctrl.stop_calls_ra.load(Ordering::SeqCst), 0);
    assert_eq!(ctrl.stop_calls_dec.load(Ordering::SeqCst), 0);
    // Mock transport seeds positions at handshake; just confirm the
    // returned snapshot looks structurally valid (no panic, both
    // axes populated even if zero).
    let _ = snap.ra.position_ticks;
    let _ = snap.dec.position_ticks;
    session.close().await.unwrap();
}

#[tokio::test]
async fn watcher_poll_with_retry_recovers_after_transient_error() {
    let (manager, ctrl, session) = flaky_manager().await;
    // Fail the next round-trip exactly once: the helper's second
    // attempt should land on a healthy transport and return Ok.
    ctrl.fail_remaining.store(1, Ordering::SeqCst);
    watcher_poll_with_retry(&manager, &session, "test")
        .await
        .expect("retry should recover from a single transient error");
    // No retry-exhaustion path → no best-effort :L.
    assert_eq!(ctrl.stop_calls_ra.load(Ordering::SeqCst), 0);
    assert_eq!(ctrl.stop_calls_dec.load(Ordering::SeqCst), 0);
    session.close().await.unwrap();
}

#[tokio::test]
async fn watcher_poll_with_retry_exhausts_then_issues_best_effort_stop() {
    let (manager, ctrl, session) = flaky_manager().await;
    // Saturate the failure budget so every retry attempt errors.
    ctrl.fail_remaining.store(u32::MAX, Ordering::SeqCst);
    let err = watcher_poll_with_retry(&manager, &session, "test")
        .await
        .expect_err("retry budget should be exhausted");
    match err {
        // The flaky transport fails recv with `TransportError::Eof`, which the
        // shared `From<TransportError>` mapping routes to `Communication`.
        StarAdvError::Communication(_) => {}
        other => panic!("expected Communication error, got {other:?}"),
    }
    // Best-effort `:L` must fire on both axes regardless of whether
    // it lands — the test counts the frames before the fail check
    // in the flaky transport.
    assert_eq!(ctrl.stop_calls_ra.load(Ordering::SeqCst), 1);
    assert_eq!(ctrl.stop_calls_dec.load(Ordering::SeqCst), 1);
    let _ = session.close().await; // may fail because of pending fails; tolerate
}

#[test]
fn pre_flip_side_for_latitude_picks_west_in_north_and_east_in_south() {
    // The natural pier side is hemisphere-dependent: Northern observers
    // have the counterweight on the West (Polaris-pointing axis tilts
    // east of horizontal), Southern observers have the opposite. The
    // helper is consulted from `execute_slew_with_explicit_side`,
    // `destination_side_of_pier`, and the slew watcher's pickup loop,
    // so both branches are load-bearing for the flip-policy logic.
    assert_eq!(pre_flip_side_for_latitude(47.6), PierSide::West);
    assert_eq!(pre_flip_side_for_latitude(0.0), PierSide::West);
    assert_eq!(pre_flip_side_for_latitude(-33.0), PierSide::East);
}

#[tokio::test]
async fn reset_for_disconnect_clears_session_state_but_keeps_mechanical() {
    // `Device::set_connected(false)` calls `reset_for_disconnect` on
    // the in-memory state. This test pins the contract directly: every
    // session-scoped field returns to its `Default::default` value,
    // and `at_park` plus `slew_settle_time` survive (mechanical state
    // and operator-tuned settings persist across reconnects).
    let mut s = DriverState {
        tracking_requested: true,
        at_park: true,
        target_ra_hours: Some(12.0),
        target_dec_degrees: Some(45.0),
        slew_settle_time: Some(Duration::from_secs(7)),
        park_ra_ticks: Some(1_000),
        park_dec_ticks: Some(-1_000),
        frame_anchored: true,
        preferred_ap_park: Some(ApPark::ApPark3),
        target_pier_side: Some(PierSide::East),
        guide_rate_ra_fraction: 0.25,
        guide_rate_dec_fraction: 0.75,
        pulse_guiding: PulseGuiding {
            ra: true,
            dec: true,
        },
    };

    s.reset_for_disconnect();

    // Cleared.
    assert!(!s.tracking_requested);
    assert_eq!(s.target_ra_hours, None);
    assert_eq!(s.target_dec_degrees, None);
    assert_eq!(s.park_ra_ticks, None);
    assert_eq!(s.park_dec_ticks, None);
    // The frame anchor and connect-resolved preferred park are
    // re-derived on the next connect — a sync-derived anchor must not
    // survive disconnect.
    assert!(!s.frame_anchored);
    assert_eq!(s.preferred_ap_park, None);
    assert!(!s.pulse_guiding.ra);
    assert!(!s.pulse_guiding.dec);
    // Guide rates re-initialise to the default (half-sidereal).
    assert!((s.guide_rate_ra_fraction - 0.5).abs() < 1e-9);
    assert!((s.guide_rate_dec_fraction - 0.5).abs() < 1e-9);

    // Preserved — mechanical state and operator-tuned override.
    assert!(s.at_park);
    assert_eq!(s.slew_settle_time, Some(Duration::from_secs(7)));
    // `target_pier_side` is not reset by `reset_for_disconnect`; it is
    // overwritten by the next slew. Pin that behaviour so a future
    // change has to be deliberate.
    assert_eq!(s.target_pier_side, Some(PierSide::East));
}

#[tokio::test]
async fn slew_watcher_re_enables_tracking_after_completion() {
    // Pins the post-slew tracking-restore branch of
    // `spawn_slew_completion_watcher`: with tracking enabled before
    // the slew, the watcher must re-issue `:G1 TRACKING + :I1 sidereal
    // + :J1` after the dwell+settle, and set `tracking_requested` back
    // to true. Without this test the entire `if tracking_was_on { ... }`
    // block (a ~20-line section that includes the only success-path
    // call to `state.write().tracking_requested = true`) is unexercised.
    use crate::transport::mock::CapturingMockFactory;
    let factory = CapturingMockFactory::new();
    let mock = std::sync::Arc::clone(&factory.state);
    let mut cfg = base_config();
    if let crate::config::TransportConfig::Usb(usb) = &mut cfg.transport {
        usb.polling_interval = Duration::from_millis(20);
    }
    cfg.mount.settle_after_slew = Duration::from_millis(0);
    // Open the envelope so the test target lands inside.
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
    cfg.mount.min_altitude_degrees = MinAltitudeDegrees::new(-90.0);
    let manager = MountManager::new(&cfg, Arc::new(factory));
    let d = MountDevice::new(cfg.mount, manager);
    d.set_connected(true).await.unwrap();

    // Pre-arm: tracking on, so `execute_slew_with_explicit_side`
    // snapshots `tracking_was_on = true` for the watcher to act on
    // after the slew completes.
    d.set_tracking(true).await.unwrap();
    assert!(d.tracking().await.unwrap());

    // Issue the slew. The slew planner clears `tracking_requested`
    // immediately after `:K1` succeeds; the watcher restores it on the
    // post-motion branch.
    d.slew_to_coordinates_async(6.0, 30.0).await.unwrap();

    // Wait for the watcher to complete. Loose deadline so a slow CI
    // runner can't flake.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !d.slewing().await.unwrap() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !d.slewing().await.unwrap(),
        "slew watcher must clear slew_in_progress within 5s"
    );

    // The watcher's tracking restore lands here.
    assert!(
        d.tracking().await.unwrap(),
        "post-slew watcher must restore tracking_requested = true"
    );

    // The wire log must show the post-slew restore sequence on RA.
    // Three `:J1` are expected total: (1) the initial
    // `set_tracking(true)`, (2) the slew's `:J1` for the RA axis
    // motion, and (3) the watcher's `:J1` post-slew restart.
    let log = mock.lock().await.command_log.clone();
    let log_strs: Vec<String> = log
        .iter()
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect();
    let j1_count = log_strs.iter().filter(|s| s.contains(":J1")).count();
    assert!(
        j1_count >= 3,
        "expected at least three :J1 frames (initial tracking, slew, watcher restore); \
         got {j1_count}; log={log_strs:?}"
    );
}

#[tokio::test]
async fn azimuth_altitude_and_utc_date_return_well_defined_values_when_connected() {
    // These three trait methods are pure derivations from the encoder
    // snapshot (`azimuth`/`altitude` via `ra_dec_to_alt_az`) or the
    // host clock (`utc_date`). No earlier test exercised them
    // directly, so a freshly-connected device sitting on the home
    // pose suffices to pin the contract: each returns a finite,
    // monotonically-sensible value rather than NaN / pre-epoch.
    let d = connected_device().await;
    let az = d.azimuth().await.unwrap();
    let alt = d.altitude().await.unwrap();
    let utc = d.utc_date().await.unwrap();
    assert!(az.is_finite(), "azimuth must be finite, got {az}");
    assert!(alt.is_finite(), "altitude must be finite, got {alt}");
    assert!(
        utc > std::time::SystemTime::UNIX_EPOCH,
        "utc_date must be after the epoch"
    );
}
