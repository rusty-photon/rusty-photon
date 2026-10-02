//! Steps for `tracking_safety.feature`.

use std::time::Duration;

use crate::world::StarAdventurerWorld;
use cucumber::{given, then, when};
use star_adventurer_gti::{ActiveZone, CwExclusionZone, TrackingGuardMarginHours};

/// `GTi` RA-axis counts-per-revolution (`0x375F00`) — the value the mock
/// seeds and the driver caches at handshake. Used to convert a
/// human-readable `mech_HA` into the encoder tick value the
/// `/debug/v1/mock-state` seed endpoint expects:
/// `ticks = mech_HA × CPR / 24`. mech-HA lives on the RA axis; the Dec
/// axis has its own, smaller CPR (`0x2C4C00`) not used here.
const GTI_CPR: f64 = 3_628_800.0;

#[given(
    expr = "a star-adventurer service with the CW exclusion zone from {float} to {float} hours"
)]
async fn configured_zone(world: &mut StarAdventurerWorld, min_hours: f64, max_hours: f64) {
    let cfg = world.config_mut();
    let zone = ActiveZone::try_new(min_hours, max_hours)
        .expect("feature files specify a valid CW exclusion zone");
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Active(zone);
}

#[given("a star-adventurer service with the CW exclusion zone disabled")]
async fn configured_zone_disabled(world: &mut StarAdventurerWorld) {
    // Disabling is explicit: `CwExclusionZone::Disabled` (JSON `null`),
    // which `bounds()` maps to an empty interval so the slew planner and
    // the guard both treat it as "no zone".
    let cfg = world.config_mut();
    cfg.mount.cw_exclusion_zone = CwExclusionZone::Disabled;
}

#[given(expr = "a tracking-guard margin of {float} hours")]
async fn configured_margin(world: &mut StarAdventurerWorld, margin_hours: f64) {
    world.config_mut().mount.tracking_guard_margin_hours =
        TrackingGuardMarginHours::try_new(margin_hours)
            .expect("feature files specify a valid tracking-guard margin");
}

#[given(expr = "the RA encoder is at mechanical HA {float} hours")]
#[when(expr = "the RA encoder is at mechanical HA {float} hours")]
async fn ra_encoder_at_mech_ha(world: &mut StarAdventurerWorld, mech_ha: f64) {
    let ticks = (mech_ha * GTI_CPR / 24.0).round() as i32;
    world.queue_seed("ra_ticks", ticks.into()).await;
}

#[then(expr = "the mount should stop tracking within {int} ms")]
async fn stop_tracking_within(world: &mut StarAdventurerWorld, ms: u64) {
    let deadline = std::time::Instant::now() + Duration::from_millis(ms);
    while std::time::Instant::now() < deadline {
        if !world.mount().tracking().await.unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the tracking guard did not stop tracking within {ms} ms");
}

#[then(expr = "the mount should still be tracking after {int} ms")]
async fn still_tracking_after(world: &mut StarAdventurerWorld, ms: u64) {
    // Give the guard several poll cycles to (wrongly) fire; it must not.
    tokio::time::sleep(Duration::from_millis(ms)).await;
    assert!(
        world.mount().tracking().await.unwrap(),
        "tracking was stopped after {ms} ms but should have stayed engaged"
    );
}
