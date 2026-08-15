//! Steps for slew.feature.

use crate::world::{CommandLogTimeout, StarAdventurerWorld};
use cucumber::gherkin::Step;
use cucumber::{given, then, when};
use std::time::Duration;

#[given(expr = "a star-adventurer service configured with a {int} second post-slew settle")]
async fn configured_with_post_slew_settle(world: &mut StarAdventurerWorld, secs: u64) {
    // A long settle pins `Slewing == true` open after the mock reaches its
    // goto target, so scenarios asserting in-flight visibility can't race
    // the completion watcher on a loaded runner. The settle never actually
    // elapses — the scenario tears the service down first.
    world.config_mut().mount.settle_after_slew = Duration::from_secs(secs);
    world.start_service().await;
}

#[given("the device is parked")]
async fn device_is_parked(world: &mut StarAdventurerWorld) {
    // Connect, park, wait for the watcher to set AtPark = true.
    world.mount().set_connected(true).await.unwrap();
    world.mount().park().await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if world.mount().at_park().await.unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("park watcher did not set AtPark within 5s");
}

#[when(expr = "I slew asynchronously to RA {float} hours and Dec {float} degrees")]
async fn slew_async_to(world: &mut StarAdventurerWorld, ra: f64, dec: f64) {
    world
        .mount()
        .slew_to_coordinates_async(ra, dec)
        .await
        .unwrap();
}

#[when(expr = "I try to slew asynchronously to RA {float} hours and Dec {float} degrees")]
async fn try_slew_async_to(world: &mut StarAdventurerWorld, ra: f64, dec: f64) {
    match world.mount().slew_to_coordinates_async(ra, dec).await {
        Ok(()) => world.clear_error(),
        Err(e) => world.record_error(e),
    }
}

#[when("I slew to the stored target")]
async fn slew_to_target(world: &mut StarAdventurerWorld) {
    world.mount().slew_to_target_async().await.unwrap();
}

#[when("I try to slew to the stored target")]
async fn try_slew_to_target(world: &mut StarAdventurerWorld) {
    match world.mount().slew_to_target_async().await {
        Ok(()) => world.clear_error(),
        Err(e) => world.record_error(e),
    }
}

#[when(expr = "I set TargetRightAscension to {float} hours")]
async fn set_target_ra(world: &mut StarAdventurerWorld, hours: f64) {
    world
        .mount()
        .set_target_right_ascension(hours)
        .await
        .unwrap();
}

#[when(expr = "I set TargetDeclination to {float} degrees")]
async fn set_target_dec(world: &mut StarAdventurerWorld, deg: f64) {
    world.mount().set_target_declination(deg).await.unwrap();
}

#[given("the mount reports both axes stopped in goto mode")]
async fn axes_stopped_in_goto(world: &mut StarAdventurerWorld) {
    seed_axes_stopped_in_goto(world).await;
}

#[when("the mount reports both axes stopped in goto mode")]
async fn when_axes_stopped_in_goto(world: &mut StarAdventurerWorld) {
    seed_axes_stopped_in_goto(world).await;
}

async fn seed_axes_stopped_in_goto(world: &mut StarAdventurerWorld) {
    // Tightly-timed scenarios that need the slew to "have just
    // finished" use the World's seed queue so failures surface as
    // assertion errors rather than silently leaving the mount in the
    // wrong state.
    world.queue_seed("ra_running", false.into()).await;
    world.queue_seed("dec_running", false.into()).await;
    world.queue_seed("ra_goto", true.into()).await;
    world.queue_seed("dec_goto", true.into()).await;
}

#[then("the mount should have received commands matching:")]
async fn commands_matching(world: &mut StarAdventurerWorld, step: &Step) {
    use regex::Regex;
    let table = step.table.as_ref().expect("expected a data table");
    let log = world.command_log().await;
    let mut log_idx = 0usize;
    for row in table.rows.iter().skip(1) {
        let pattern = format!("^{}\r?$", row[0].trim());
        let re = Regex::new(&pattern).expect("invalid regex in feature file");
        let mut found = false;
        while log_idx < log.len() {
            if re.is_match(&log[log_idx]) {
                found = true;
                log_idx += 1;
                break;
            }
            log_idx += 1;
        }
        assert!(
            found,
            "expected log entry matching {pattern:?}; saw {log:?}"
        );
    }
}

#[then(expr = "TargetRightAscension should be {float} hours within {float}")]
async fn target_ra_should_be(world: &mut StarAdventurerWorld, expected: f64, tolerance: f64) {
    let actual = world.mount().target_right_ascension().await.unwrap();
    assert!((actual - expected).abs() < tolerance);
}

#[then(expr = "TargetDeclination should be {float} degrees within {float}")]
async fn target_dec_should_be(world: &mut StarAdventurerWorld, expected: f64, tolerance: f64) {
    let actual = world.mount().target_declination().await.unwrap();
    assert!((actual - expected).abs() < tolerance);
}

#[then(
    expr = "the slew target on the wire should correspond to RA {float} hours and Dec {float} degrees"
)]
async fn wire_slew_target(world: &mut StarAdventurerWorld, _ra: f64, dec: f64) {
    // The **Dec-axis** CPR (different from RA on the GTi).
    const GTI_CPR_DEC: u32 = 0x002C_4C00;

    // Decode the Dec axis target — this comparison doesn't depend on
    // LST so it's deterministic in BDD. The RA target involves the
    // `lst_at_slew_time` which we can't pin until clock injection is
    // wired in (the unit test
    // `coordinates::tests::ra_ticks_round_trip_through_mechanical_ha`
    // pins the ticks ↔ RA math regardless), so for RA we still only
    // assert that *some* `:H1` frame appears.
    //
    // INDI-style slews use `:H<axis><|delta|>` (unsigned magnitude) +
    // a `:G<axis>` that carries the direction (CCW) bit. The mock
    // starts each axis at encoder 0, so for Dec 45° the magnitude is
    // exactly the encoder ticks for 45° and the direction is CW.
    use skywatcher_motor_protocol::codec::decode_u24;
    let log = world.command_log().await;
    let h1 = log
        .iter()
        .find(|c| c.starts_with(":H1") && c.ends_with('\r'))
        .unwrap_or_else(|| panic!("no :H1 in log {log:?}"));
    // We assert against the *slew-issue* `:H2`, which is always the
    // first `:H2` in the log: it carries the magnitude of the full
    // slew delta (encoder ticks for 45° from the mock's encoder-0
    // start). Pickup re-issues, if any, emit subsequent `:H2`
    // frames carrying only the residual delta (sub-arcsecond on
    // the mock); pairing against those would falsely report a
    // near-zero Dec target.
    let h2 = log
        .iter()
        .find(|c| c.starts_with(":H2") && c.ends_with('\r'))
        .unwrap_or_else(|| panic!("no :H2 in log {log:?}"));
    // :H<axis><6 hex bytes>\r — 10 bytes total.
    assert_eq!(h2.len(), 10, "malformed :H2 frame {h2:?}");
    let payload: &[u8; 6] = (&h2.as_bytes()[3..9])
        .try_into()
        .expect("six payload bytes");
    let dec_magnitude = decode_u24(payload).expect("valid :H2 payload");
    // Recover the direction bit from the `:G2` that goes with the
    // slew-issue `:H2`. The INDI sequence is
    // `:L2 → :G2 → :I2 → :H2 → :M2 → :J2` per axis, so `:G2` is
    // the third frame before `:H2`, not the immediately-preceding
    // one (`:I2` sits between them). Use `rfind` over the prefix
    // ending at `h2`'s index: that locates the closest preceding
    // `:G2`, which is the `:G2` from the same slew-issue burst
    // regardless of how many earlier slews or handshakes ran.
    let h2_idx = log
        .iter()
        .position(|c| c == h2)
        .expect(":H2 must be in the log we just found");
    let g2 = log[..h2_idx]
        .iter()
        .rfind(|c| c.starts_with(":G2") && c.ends_with('\r'))
        .unwrap_or_else(|| panic!("no :G2 before the matched :H2 in log {log:?}"));
    // :G<axis><DB1_nibble><DB2_nibble>\r — 6 bytes total. Per the
    // Sky-Watcher motor-controller spec §5 each DB is one hex
    // nibble (4 bits), not a full byte; see
    // `skywatcher_motor_protocol::MotionMode::to_wire_bytes` for
    // the canonical encoding. DB2 bit 0 = CCW.
    assert_eq!(g2.len(), 6, "malformed :G2 frame {g2:?}");
    let db2 = u8::from_str_radix(g2.get(4..5).expect("6-byte :G2 frame"), 16).expect("valid hex");
    let signed_ticks: i64 = if db2 & 0x1 != 0 {
        -i64::from(dec_magnitude)
    } else {
        i64::from(dec_magnitude)
    };

    // Convert wire ticks back to degrees and compare against the
    // requested Dec, using the Dec-axis CPR.
    let dec_actual = (signed_ticks as f64) * 360.0 / f64::from(GTI_CPR_DEC);
    let tol = 0.5; // 0.5° matches the BDD scenario's ±round-trip slop
    assert!(
        (dec_actual - dec).abs() < tol,
        "Dec target {dec_actual:.4}° differs from requested {dec:.4}° by > {tol}°; \
         :H1={h1:?} :H2={h2:?} :G2={g2:?}"
    );
}

#[then(expr = "the mount should eventually receive a tracking-mode :G1 within {int} seconds")]
async fn mount_eventually_tracking_g1(world: &mut StarAdventurerWorld, secs: u64) {
    // A tracking-mode `:G1` per the Sky-Watcher spec §5 has the
    // **first** payload nibble (DB1) bit-0 set (`Tracking`, vs. Goto).
    // The wire form is `:G1<DB1><DB2>\r` — see
    // `skywatcher_motor_protocol::MotionMode` for the full encoding.
    match world
        .wait_for_command_log(Duration::from_secs(secs), |log| {
            log.iter().any(|c| is_tracking_g1(c))
        })
        .await
    {
        Ok(_) => {}
        Err(CommandLogTimeout::NoMatch(log)) => {
            panic!("no tracking-mode :G1 within {secs}s; log {log:?}")
        }
        Err(CommandLogTimeout::FetchFailed(e)) => {
            panic!("could not read the command log while waiting for a tracking-mode :G1: {e}")
        }
    }
}

#[then("the mount should not receive a tracking-mode :G1")]
async fn mount_should_not_tracking_g1(world: &mut StarAdventurerWorld) {
    // Wait briefly to let any pending watcher iteration fire, then
    // assert no tracking-mode :G1 appeared.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = world.command_log().await;
    assert!(
        !log.iter().any(|c| is_tracking_g1(c)),
        "found tracking-mode :G1; log {log:?}"
    );
}

/// Returns `true` if `frame` is a `:G1<DB1><DB2>\r` command whose
/// DB1 nibble has bit 0 set — i.e., a tracking-mode switch on the
/// RA axis per the Sky-Watcher spec §5.
fn is_tracking_g1(frame: &str) -> bool {
    let bytes = frame.as_bytes();
    if bytes.len() < 6 || &bytes[..3] != b":G1" {
        return false;
    }
    let Some(db1) = hex_digit(bytes[3]) else {
        return false;
    };
    db1 & 0x1 != 0
}

const fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
