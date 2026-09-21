//! Steps for `pier_side_selection.feature`.
//!
//! The CW exclusion zone is expressed in `mech_HA`, so these scenarios
//! address targets by *hour angle* and let the step compute
//! `RA = LST − HA` at run time (the shared helper
//! [`super::altitude_floor_steps::ra_for_hour_angle`]). A hardcoded RA
//! would put the target at whatever `mech_HA` the wallclock happened to
//! produce, which is exactly why the BDD baseline config disables the
//! zone.

use ascom_alpaca::api::telescope::PierSide;
use cucumber::{given, then, when};
use star_adventurer_gti::config::{ActiveZone, CwExclusionZone};
use std::time::{Duration, Instant};

use crate::world::{StarAdventurerWorld, DEBUG_RETRY_WINDOW};

use super::altitude_floor_steps::ra_for_hour_angle;

/// The **Dec-axis** CPR (different from the RA axis on the `GTi`); the
/// scenarios that use these steps pin the pair to these mock defaults.
const GTI_CPR_DEC: u32 = 0x002C_4C00;

#[given(
    expr = "a star-adventurer service configured with flip_policy enabled, site latitude {float} degrees and a CW exclusion zone of {float} to {float} hours"
)]
async fn configured_with_zone(
    world: &mut StarAdventurerWorld,
    latitude_deg: f64,
    zone_min: f64,
    zone_max: f64,
) {
    world.config_mut().mount.flip_policy.enabled = true;
    world.config_mut().mount.site_latitude_deg = latitude_deg;
    world.config_mut().mount.cw_exclusion_zone = CwExclusionZone::Active(
        ActiveZone::try_new(zone_min, zone_max).expect("zone bounds valid in feature file"),
    );
    world.start_service().await;
}

/// Wait for the driver's snapshot to carry the seeded Dec encoder
/// before a scenario leans on which side the mount is on.
///
/// `SideOfPier`, `DestinationSideOfPier`, sync and the slew planner all
/// read the same background-poll snapshot, which lags a freshly
/// connected mount's encoders by up to one `polling_interval`. Stating
/// the starting side as a step both waits for it and documents the
/// precondition, instead of leaving the scenario to race the poll loop.
#[when(expr = "the mount has settled on pier side {word}")]
async fn mount_has_settled_on_side(world: &mut StarAdventurerWorld, label: String) {
    let want = match label.as_str() {
        "East" => PierSide::East,
        "West" => PierSide::West,
        other => panic!("unknown PierSide name: {other}"),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = PierSide::Unknown;
    while Instant::now() < deadline {
        last = world
            .mount()
            .side_of_pier()
            .await
            .expect("SideOfPier readable once connected");
        if last == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("mount never settled on pier side {want:?} (last read {last:?})");
}

#[when(
    expr = "I read DestinationSideOfPier for a target at hour angle {float} hours and Dec {float} degrees"
)]
async fn read_destination_side_for_hour_angle(world: &mut StarAdventurerWorld, ha: f64, dec: f64) {
    let ra = ra_for_hour_angle(world, ha);
    let side = world
        .mount()
        .destination_side_of_pier(ra, dec)
        .await
        .expect("DestinationSideOfPier should succeed for this scenario");
    world.record_destination_pier_side(side);
}

#[when(expr = "I sync to a target at hour angle {float} hours and Dec {float} degrees")]
async fn sync_to_hour_angle(world: &mut StarAdventurerWorld, ha: f64, dec: f64) {
    let ra = ra_for_hour_angle(world, ha);
    world
        .mount()
        .sync_to_coordinates(ra, dec)
        .await
        .expect("sync should be accepted in this scenario");
}

#[then(
    expr = "the Dec encoder position written to the wire should be {float} degrees within {float}"
)]
async fn dec_position_written_to_wire(
    world: &mut StarAdventurerWorld,
    expected_deg: f64,
    tolerance: f64,
) {
    use skywatcher_motor_protocol::codec::decode_position;

    // `:E2` is the Dec-axis position write. Take the newest one: the
    // scenario's sync is the last thing to have issued one.
    let log = world
        .wait_for_command_log(DEBUG_RETRY_WINDOW, |log| {
            log.iter().any(|c| c.starts_with(":E2"))
        })
        .await
        .unwrap_or_else(|e| panic!("no :E2 frame on the wire: {e:?}"));
    let frame = log
        .iter()
        .rev()
        .find(|c| c.starts_with(":E2"))
        .expect("the wait above accepted the log on this predicate");
    // `:E<axis><6 hex bytes>\r` — 10 bytes total.
    assert_eq!(frame.len(), 10, "malformed :E2 frame {frame:?}");
    let payload: &[u8; 6] = frame.as_bytes()[3..9]
        .try_into()
        .expect("six payload bytes");
    let ticks = decode_position(payload).expect("valid :E2 payload");
    let degrees = f64::from(ticks) * 360.0 / f64::from(GTI_CPR_DEC);
    assert!(
        (degrees - expected_deg).abs() < tolerance,
        "expected Dec encoder write at {expected_deg}° ± {tolerance}, got {degrees}° \
         (ticks {ticks}, frame {frame:?})"
    );
}
