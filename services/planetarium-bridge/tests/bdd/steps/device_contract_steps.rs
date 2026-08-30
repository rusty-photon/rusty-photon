//! Steps for `device_contract.feature`: the capability matrix, Target*
//! propagation, the simulated-slew lifecycle, and site/UTC writes.

use std::time::{Duration, Instant, SystemTime};

use ascom_alpaca::api::telescope::{GuideDirection, TelescopeAxis};
use cucumber::{then, when};

use crate::world::{BridgeWorld, POLL_INTERVAL, WAIT_BUDGET};

const RA_TOLERANCE_HOURS: f64 = 0.001;
const DEC_TOLERANCE_DEG: f64 = 0.01;

/// One dispatch for the whole capability matrix: read the named member and
/// compare its canonical rendering — the matrix in the feature file is the
/// contract, this step just carries it out.
#[then(expr = "{word} should read {word}")]
async fn member_reads(world: &mut BridgeWorld, member: String, expected: String) {
    let t = world.telescope();
    let actual = match member.as_str() {
        "AlignmentMode" => format!("{:?}", t.alignment_mode().await.unwrap()),
        "EquatorialSystem" => format!("{:?}", t.equatorial_system().await.unwrap()),
        "CanSlew" => t.can_slew().await.unwrap().to_string(),
        "CanSlewAsync" => t.can_slew_async().await.unwrap().to_string(),
        "CanSync" => t.can_sync().await.unwrap().to_string(),
        "CanSyncAltAz" => t.can_sync_alt_az().await.unwrap().to_string(),
        "CanSlewAltAz" => t.can_slew_alt_az().await.unwrap().to_string(),
        "CanSlewAltAzAsync" => t.can_slew_alt_az_async().await.unwrap().to_string(),
        "CanPark" => t.can_park().await.unwrap().to_string(),
        "CanUnpark" => t.can_unpark().await.unwrap().to_string(),
        "CanFindHome" => t.can_find_home().await.unwrap().to_string(),
        "CanSetPark" => t.can_set_park().await.unwrap().to_string(),
        "AtPark" => t.at_park().await.unwrap().to_string(),
        "CanMoveAxis" => {
            // "false" means no axis is movable, so all three must agree.
            let mut movable = false;
            for axis in [
                TelescopeAxis::Primary,
                TelescopeAxis::Secondary,
                TelescopeAxis::Tertiary,
            ] {
                movable |= t.can_move_axis(axis).await.unwrap();
            }
            movable.to_string()
        }
        "CanPulseGuide" => t.can_pulse_guide().await.unwrap().to_string(),
        "CanSetGuideRates" => t.can_set_guide_rates().await.unwrap().to_string(),
        "CanSetTracking" => t.can_set_tracking().await.unwrap().to_string(),
        "CanSetDeclinationRate" => t.can_set_declination_rate().await.unwrap().to_string(),
        "CanSetRightAscensionRate" => t.can_set_right_ascension_rate().await.unwrap().to_string(),
        "Tracking" => t.tracking().await.unwrap().to_string(),
        "Slewing" => t.slewing().await.unwrap().to_string(),
        other => panic!("unknown telescope member: {other}"),
    };
    assert_eq!(
        actual, expected,
        "{member} read {actual}, expected {expected}"
    );
}

#[then(expr = "the device name should be {string}")]
async fn device_name(world: &mut BridgeWorld, expected: String) {
    let name = world.telescope().name().await.unwrap();
    assert_eq!(name, expected);
}

#[then(expr = "the device description should contain {string}")]
async fn device_description_contains(world: &mut BridgeWorld, needle: String) {
    let description = world.telescope().description().await.unwrap();
    assert!(
        description.contains(&needle),
        "description {description:?} does not contain {needle:?}"
    );
}

#[when("I try to read SideOfPier")]
async fn try_side_of_pier(world: &mut BridgeWorld) {
    match world.telescope().side_of_pier().await {
        Ok(_) => world.last_error_code = None,
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[then("the primary axis should advertise no axis rates")]
async fn no_axis_rates(world: &mut BridgeWorld) {
    let rates = world
        .telescope()
        .axis_rates(TelescopeAxis::Primary)
        .await
        .unwrap();
    assert!(rates.is_empty(), "expected no axis rates, got {rates:?}");
}

#[then(expr = "UTCDate should read within {int} seconds of the harness clock")]
async fn utc_date_within(world: &mut BridgeWorld, seconds: u64) {
    let device_utc = world.telescope().utc_date().await.unwrap();
    let drift = SystemTime::now()
        .duration_since(device_utc)
        .unwrap_or_else(|e| e.duration());
    assert!(
        drift <= Duration::from_secs(seconds),
        "UTCDate drifted {drift:?} from the harness clock"
    );
}

#[when("I try to set the device UTC date")]
async fn try_set_utc_date(world: &mut BridgeWorld) {
    match world.telescope().set_utc_date(SystemTime::now()).await {
        Ok(()) => world.last_error_code = None,
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

// --- Slew lifecycle ---

#[when(expr = "the client starts an async slew to ra {float} hours dec {float} degrees")]
async fn start_async_slew(world: &mut BridgeWorld, ra_hours: f64, dec_degrees: f64) {
    world
        .telescope()
        .slew_to_coordinates_async(ra_hours, dec_degrees)
        .await
        .expect("async slew was rejected");
}

#[when(expr = "the client tries an async slew to ra {float} hours dec {float} degrees")]
async fn try_async_slew(world: &mut BridgeWorld, ra_hours: f64, dec_degrees: f64) {
    match world
        .telescope()
        .slew_to_coordinates_async(ra_hours, dec_degrees)
        .await
    {
        Ok(()) => world.last_error_code = None,
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}

#[when(expr = "the client slews blocking to ra {float} hours dec {float} degrees")]
async fn blocking_slew(world: &mut BridgeWorld, ra_hours: f64, dec_degrees: f64) {
    world
        .telescope()
        .slew_to_coordinates(ra_hours, dec_degrees)
        .await
        .expect("blocking slew was rejected");
}

#[when(expr = "the client sets TargetRightAscension to {float} hours")]
async fn set_target_ra(world: &mut BridgeWorld, ra_hours: f64) {
    world
        .telescope()
        .set_target_right_ascension(ra_hours)
        .await
        .expect("TargetRightAscension write was rejected");
}

#[when(expr = "the client sets TargetDeclination to {float} degrees")]
async fn set_target_dec(world: &mut BridgeWorld, dec_degrees: f64) {
    world
        .telescope()
        .set_target_declination(dec_degrees)
        .await
        .expect("TargetDeclination write was rejected");
}

#[when("the client slews to target asynchronously")]
async fn slew_to_target_async(world: &mut BridgeWorld) {
    world
        .telescope()
        .slew_to_target_async()
        .await
        .expect("SlewToTargetAsync was rejected");
}

#[when("the client aligns to target")]
async fn aligns_to_target(world: &mut BridgeWorld) {
    match world.telescope().sync_to_target().await {
        Ok(()) => world.last_error_code = None,
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
    assert_eq!(world.last_error_code, None, "SyncToTarget was rejected");
}

#[when("the client aborts the slew")]
async fn abort_slew(world: &mut BridgeWorld) {
    world
        .telescope()
        .abort_slew()
        .await
        .expect("AbortSlew was rejected");
}

/// Poll `Slewing` until the simulated motion has converged.
///
/// A failed read counts as "not yet" rather than a verdict. The device's
/// `Slewing` is infallible server-side, so an `Err` here is the transport:
/// the whole suite shares one poll loop, and a loopback request can stall
/// for seconds when it is starved (testing.md 5.7, 5.9). Keeping the last
/// error lets the timeout say which of two unrelated bugs it hit — a mount
/// that never converged, or an endpoint that never answered.
async fn await_convergence(world: &BridgeWorld) {
    let t = world.telescope();
    let start = Instant::now();
    let mut last_error = None;
    while start.elapsed() < WAIT_BUDGET {
        match t.slewing().await {
            Ok(false) => return,
            Ok(true) => last_error = None,
            Err(e) => last_error = Some(e),
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    let elapsed = start.elapsed();
    match last_error {
        Some(e) => panic!(
            "Slewing was unreadable over the last of {elapsed:?} (budget {WAIT_BUDGET:?}): {e:?}"
        ),
        None => panic!("Slewing still read true after {elapsed:?} (budget {WAIT_BUDGET:?})"),
    }
}

/// Wait out any simulated convergence, plus a short settle so an import that
/// a buggy slew path might fire would have arrived before the Then step runs.
#[when("all simulated slews have completed")]
async fn slews_completed(world: &mut BridgeWorld) {
    await_convergence(world).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[then("Slewing should read false once the simulated slew converges")]
async fn slewing_false_on_convergence(world: &mut BridgeWorld) {
    await_convergence(world).await;
}

#[then(expr = "the reported position should be ra {float} hours dec {float} degrees")]
async fn reported_position(world: &mut BridgeWorld, ra_hours: f64, dec_degrees: f64) {
    let t = world.telescope();
    let actual_ra = t.right_ascension().await.unwrap();
    let actual_dec = t.declination().await.unwrap();
    assert!(
        (actual_ra - ra_hours).abs() < RA_TOLERANCE_HOURS,
        "reported RA {actual_ra}h, expected {ra_hours}h"
    );
    assert!(
        (actual_dec - dec_degrees).abs() < DEC_TOLERANCE_DEG,
        "reported Dec {actual_dec} deg, expected {dec_degrees} deg"
    );
}

#[then(expr = "TargetRightAscension should read {float} hours")]
async fn target_ra_reads(world: &mut BridgeWorld, ra_hours: f64) {
    let actual = world.telescope().target_right_ascension().await.unwrap();
    assert!(
        (actual - ra_hours).abs() < RA_TOLERANCE_HOURS,
        "TargetRightAscension read {actual}h, expected {ra_hours}h"
    );
}

#[then(expr = "TargetDeclination should read {float} degrees")]
async fn target_dec_reads(world: &mut BridgeWorld, dec_degrees: f64) {
    let actual = world.telescope().target_declination().await.unwrap();
    assert!(
        (actual - dec_degrees).abs() < DEC_TOLERANCE_DEG,
        "TargetDeclination read {actual} deg, expected {dec_degrees} deg"
    );
}

// --- Site writes ---

#[when(expr = "the client sets SiteLatitude to {float} degrees")]
async fn set_site_latitude(world: &mut BridgeWorld, latitude: f64) {
    world
        .telescope()
        .set_site_latitude(latitude)
        .await
        .expect("SiteLatitude write was rejected");
}

#[when(expr = "the client sets SiteLongitude to {float} degrees")]
async fn set_site_longitude(world: &mut BridgeWorld, longitude: f64) {
    world
        .telescope()
        .set_site_longitude(longitude)
        .await
        .expect("SiteLongitude write was rejected");
}

#[then(expr = "SiteLatitude should read {float} degrees")]
async fn site_latitude_reads(world: &mut BridgeWorld, latitude: f64) {
    let actual = world.telescope().site_latitude().await.unwrap();
    assert!(
        (actual - latitude).abs() < 1e-9,
        "SiteLatitude read {actual}, expected {latitude}"
    );
}

#[then(expr = "SiteLongitude should read {float} degrees")]
async fn site_longitude_reads(world: &mut BridgeWorld, longitude: f64) {
    let actual = world.telescope().site_longitude().await.unwrap();
    assert!(
        (actual - longitude).abs() < 1e-9,
        "SiteLongitude read {actual}, expected {longitude}"
    );
}

// --- Declared-false verbs ---

#[when(expr = "I try to invoke {word}")]
async fn try_invoke(world: &mut BridgeWorld, verb: String) {
    let t = world.telescope();
    let result = match verb.as_str() {
        "Park" => t.park().await,
        "Unpark" => t.unpark().await,
        "FindHome" => t.find_home().await,
        "SetPark" => t.set_park().await,
        "MoveAxis" => t.move_axis(TelescopeAxis::Primary, 1.0).await,
        "PulseGuide" => {
            t.pulse_guide(GuideDirection::North, Duration::from_millis(100))
                .await
        }
        "SetTracking" => t.set_tracking(false).await,
        "SetDeclinationRate" => t.set_declination_rate(0.1).await,
        "SetRightAscensionRate" => t.set_right_ascension_rate(0.1).await,
        "SyncToAltAz" => t.sync_to_alt_az(180.0, 45.0).await,
        "SlewToAltAz" => t.slew_to_alt_az(180.0, 45.0).await,
        "SlewToAltAzAsync" => t.slew_to_alt_az_async(180.0, 45.0).await,
        other => panic!("unknown telescope verb: {other}"),
    };
    match result {
        Ok(()) => world.last_error_code = None,
        Err(e) => world.last_error_code = Some(e.code.raw()),
    }
}
