//! Steps for `altitude_floor.feature`.
//!
//! Apparent altitude is a function of the target's *hour angle*, not
//! its RA, so these steps address targets by HA and compute
//! `RA = LST − HA` at run time (folded into `[0, 24)`). The few tens
//! of milliseconds of LST drift between the step computing the RA and
//! the driver re-reading LST shift the effective HA by ~1e-6 h — far
//! below the ≥ 1° altitude margins the feature's scenarios use.

use crate::world::StarAdventurerWorld;
use cucumber::{given, then, when};
use star_adventurer_gti::config::MinAltitudeDegrees;
use star_adventurer_gti::coordinates::local_sidereal_time_hours;
use std::time::SystemTime;

#[given(
    expr = "a star-adventurer service configured with site latitude {float} degrees and minimum target altitude {float} degrees"
)]
async fn configured_with_latitude_and_floor(
    world: &mut StarAdventurerWorld,
    latitude_deg: f64,
    floor_deg: f64,
) {
    world.config_mut().mount.site_latitude_deg = latitude_deg;
    world.config_mut().mount.min_altitude_degrees =
        MinAltitudeDegrees::try_new(floor_deg).expect("floor within [-90, 90] in feature file");
    world.start_service().await;
}

/// RA (hours, folded to `[0, 24)`) that places a target at
/// `ha_hours` for the configured site right now.
///
/// Shared with `pier_side_selection_steps`, whose scenarios address
/// targets by hour angle for the same reason: the quantity under test
/// (`mech_HA`) is an hour angle, and a hardcoded RA would tie it to
/// the wallclock.
pub fn ra_for_hour_angle(world: &StarAdventurerWorld, ha_hours: f64) -> f64 {
    let lon = world
        .config
        .as_ref()
        .expect("config built before slew/sync steps")
        .mount
        .site_longitude_deg;
    let lst = local_sidereal_time_hours(SystemTime::now(), lon)
        .expect("LST for current wallclock")
        .value();
    (lst - ha_hours).rem_euclid(24.0)
}

#[when(
    expr = "I slew asynchronously to a target at hour angle {float} hours and Dec {float} degrees"
)]
async fn slew_async_to_hour_angle(world: &mut StarAdventurerWorld, ha: f64, dec: f64) {
    let ra = ra_for_hour_angle(world, ha);
    world
        .mount()
        .slew_to_coordinates_async(ra, dec)
        .await
        .expect("slew should be accepted in this scenario");
}

#[when(
    expr = "I try to slew asynchronously to a target at hour angle {float} hours and Dec {float} degrees"
)]
async fn try_slew_async_to_hour_angle(world: &mut StarAdventurerWorld, ha: f64, dec: f64) {
    let ra = ra_for_hour_angle(world, ha);
    match world.mount().slew_to_coordinates_async(ra, dec).await {
        Ok(()) => world.clear_error(),
        Err(e) => world.record_error(e),
    }
}

#[when(expr = "I try to sync to a target at hour angle {float} hours and Dec {float} degrees")]
async fn try_sync_to_hour_angle(world: &mut StarAdventurerWorld, ha: f64, dec: f64) {
    let ra = ra_for_hour_angle(world, ha);
    match world.mount().sync_to_coordinates(ra, dec).await {
        Ok(()) => world.clear_error(),
        Err(e) => world.record_error(e),
    }
}

#[then(expr = "the error message should mention {string}")]
async fn error_message_should_mention(world: &mut StarAdventurerWorld, needle: String) {
    let msg = world.last_error.as_ref().expect("no error captured");
    assert!(
        msg.contains(&needle),
        "expected error message containing {needle:?}, got {msg:?}"
    );
}
