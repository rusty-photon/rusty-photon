//! Steps for park.feature.

use crate::world::StarAdventurerWorld;
use cucumber::{given, then, when};
use skywatcher_motor_protocol::codec::encode_position;
use std::time::Duration;

#[given(
    expr = "a star-adventurer service configured with park_ra_ticks {int} and park_dec_ticks {int}"
)]
async fn configured_with_park_ticks(world: &mut StarAdventurerWorld, park_ra: i32, park_dec: i32) {
    world.config_mut().mount.park_ra_ticks = Some(park_ra);
    world.config_mut().mount.park_dec_ticks = Some(park_dec);
    world.start_service().await;
}

#[when("I park the mount")]
async fn park_mount(world: &mut StarAdventurerWorld) {
    world.mount().park().await.unwrap();
    // ASCOM Park is async-shaped in this driver: park() returns once
    // the goto motor commands have been issued; the watcher flips
    // `AtPark` after observing both axes settled at the park target.
    // Scenarios chaining off "I park the mount" expect AtPark to be
    // true by the next step — wait for the watcher to land it, with
    // a deadline matching `the device is parked` in slew_steps.rs.
    // Windows + macOS CI lost this race against the watcher on PR
    // #244 while Linux happened to win; the explicit wait makes the
    // step deterministic across platforms.
    wait_for_at_park(world).await;
}

/// `Park()` without waiting for the watcher: the park is left in flight
/// for the next step to act on.
#[when("I start parking the mount")]
async fn start_parking_mount(world: &mut StarAdventurerWorld) {
    world.mount().park().await.unwrap();
}

#[when("I try to park the mount")]
async fn try_park_mount(world: &mut StarAdventurerWorld) {
    match world.mount().park().await {
        Ok(()) => {
            world.clear_error();
            wait_for_at_park(world).await;
        }
        Err(e) => world.record_error(e),
    }
}

/// Poll `AtPark` until it flips to `true` or a 5-second deadline
/// elapses. Matches the pattern in `slew_steps::device_is_parked`.
async fn wait_for_at_park(world: &mut StarAdventurerWorld) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if world.mount().at_park().await.unwrap_or(false) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("park watcher did not set AtPark within 5s");
}

#[when("I unpark the mount")]
async fn unpark_mount(world: &mut StarAdventurerWorld) {
    world.mount().unpark().await.unwrap();
}

#[when("I set the park position")]
async fn set_park_position(world: &mut StarAdventurerWorld) {
    world.mount().set_park().await.unwrap();
}

#[when("I try to set the park position")]
async fn try_set_park_position(world: &mut StarAdventurerWorld) {
    match world.mount().set_park().await {
        Ok(()) => world.clear_error(),
        Err(e) => world.record_error(e),
    }
}

#[when("the mount reports both axes stopped at encoder 0")]
async fn mount_reports_axes_stopped_at_zero(world: &mut StarAdventurerWorld) {
    // Force the post-slew shape via the World's seed queue. The mock
    // simulator already advances to encoder 0 once park's :S1<0> /
    // :S2<0> arrive, but tightly-timed scenarios may race the
    // watcher's first poll; the explicit seed is a deterministic
    // override.
    world.queue_seed("ra_ticks", 0.into()).await;
    world.queue_seed("dec_ticks", 0.into()).await;
    world.queue_seed("ra_running", false.into()).await;
    world.queue_seed("dec_running", false.into()).await;
    // After seeding the wire-side state, wait for the park watcher
    // to observe it and flip `AtPark`. The step's name claims a
    // *driver-visible* post-condition (the mount has been observed
    // stopped at encoder 0), so the test reads more honestly when
    // it actually waits for that to land. macOS in particular
    // races the watcher behind the next step on tightly-paced
    // scenarios like "Park is idempotent" (caught by PR #200 CI on
    // macos-latest). Don't panic if the timeout expires — other
    // scenarios assert the AtPark flip with their own explicit
    // `Then AtPark should eventually be true within N seconds`,
    // and we'd rather their scenario-specific message fire than
    // this generic one.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if world.mount().at_park().await.unwrap() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[then("AtPark should be false")]
async fn at_park_false(world: &mut StarAdventurerWorld) {
    assert!(!world.mount().at_park().await.unwrap());
}

#[then("AtPark should be true")]
async fn at_park_true(world: &mut StarAdventurerWorld) {
    assert!(world.mount().at_park().await.unwrap());
}

/// Watch `AtPark` across the whole window rather than sampling it once at
/// the end: a flip to `true` anywhere inside it is the failure, and a
/// longer window can only make it easier to see (docs/skills/testing.md
/// §6.9).
#[then(expr = "AtPark should stay false for {int} seconds")]
async fn at_park_stays_false(world: &mut StarAdventurerWorld, secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        assert!(
            !world.mount().at_park().await.unwrap(),
            "AtPark turned true within {secs} seconds"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[then(expr = "AtPark should eventually be true within {int} seconds")]
async fn at_park_eventually_true(world: &mut StarAdventurerWorld, secs: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if world.mount().at_park().await.unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("AtPark did not become true within {secs} seconds");
}

#[then("the mount should have received command :K1 before any :S1")]
async fn k1_before_s1(world: &mut StarAdventurerWorld) {
    let log = world.command_log().await;
    let first_k1 = log.iter().position(|c| c == ":K1\r");
    let first_s1 = log.iter().position(|c| c.starts_with(":S1"));
    match (first_k1, first_s1) {
        (Some(k), Some(s)) => assert!(
            k < s,
            ":K1 (index {k}) must precede first :S1 (index {s}); log {log:?}"
        ),
        (Some(_), None) => {
            // No :S1 yet — the park sequence may still be in flight.
            // Treat absence of :S1 as a pass; later assertions cover
            // its arrival.
        }
        _ => panic!(":K1 not seen in log {log:?}"),
    }
}

#[then(expr = "the mount should have received a :S1 command targeting encoder {int}")]
async fn s1_targeting_encoder(world: &mut StarAdventurerWorld, ticks: i32) {
    let log = world.command_log().await;
    let bytes = encode_position(ticks).expect("encode_position");
    let want = format!(":S1{}\r", std::str::from_utf8(&bytes).unwrap());
    assert!(
        log.iter().any(|c| c == &want),
        "expected {want:?} in log {log:?}"
    );
}

#[then(expr = "the mount should have received a :S2 command targeting encoder {int}")]
async fn s2_targeting_encoder(world: &mut StarAdventurerWorld, ticks: i32) {
    let log = world.command_log().await;
    let bytes = encode_position(ticks).expect("encode_position");
    let want = format!(":S2{}\r", std::str::from_utf8(&bytes).unwrap());
    assert!(
        log.iter().any(|c| c == &want),
        "expected {want:?} in log {log:?}"
    );
}

#[then("the mount should not have received a second :S1 command")]
async fn no_second_s1(world: &mut StarAdventurerWorld) {
    let log = world.command_log().await;
    let s1_count = log.iter().filter(|c| c.starts_with(":S1")).count();
    assert!(s1_count <= 1, ":S1 issued {s1_count} times; log {log:?}");
}

#[then("the mount should not have received any goto command")]
async fn no_goto_commands(world: &mut StarAdventurerWorld) {
    // A goto cannot happen without a `:S<axis>` target write, so the
    // absence of `:S1` / `:S2` proves no slew was commanded on either
    // axis (`:G` alone also precedes tracking, `:K` is a stop).
    let log = world.command_log().await;
    let gotos: Vec<_> = log
        .iter()
        .filter(|c| c.starts_with(":S1") || c.starts_with(":S2"))
        .collect();
    assert!(
        gotos.is_empty(),
        "expected no goto target commands, saw {gotos:?}; log {log:?}"
    );
}

#[then(expr = "the persisted config should have park_ra_ticks {int} and park_dec_ticks {int}")]
async fn persisted_config_has_park_values(
    world: &mut StarAdventurerWorld,
    want_ra: i32,
    want_dec: i32,
) {
    let cfg = world.read_persisted_config();
    assert_eq!(
        cfg.mount.park_ra_ticks,
        Some(want_ra),
        "park_ra_ticks mismatch"
    );
    assert_eq!(
        cfg.mount.park_dec_ticks,
        Some(want_dec),
        "park_dec_ticks mismatch"
    );
}
