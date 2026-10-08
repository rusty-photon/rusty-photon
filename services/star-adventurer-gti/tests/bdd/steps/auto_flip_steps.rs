//! Steps for `auto_flip.feature`.
//!
//! The encoder-seeding, tracking, and command-log steps are shared with
//! `tracking_safety_steps.rs`, `tracking_steps.rs`, and
//! `connection_steps.rs`; this file only adds the auto-flip config
//! seeds and the flip-completion assertions.

use std::time::Duration;

use crate::world::StarAdventurerWorld;
use ascom_alpaca::api::telescope::PierSide;
use cucumber::{given, then};

#[given(expr = "auto-flip during tracking at meridian offset {float} hours")]
async fn auto_flip_at_offset(world: &mut StarAdventurerWorld, offset_hours: f64) {
    let auto_flip = &mut world.config_mut().mount.auto_flip;
    auto_flip.enabled = true;
    auto_flip.meridian_offset_hours = offset_hours;
}

#[then(expr = "the mount should be tracking on pier side {word} within {int} seconds")]
async fn tracking_on_pier_side_within(world: &mut StarAdventurerWorld, side: String, secs: u64) {
    let want = match side.as_str() {
        "East" => PierSide::East,
        "West" => PierSide::West,
        other => panic!("unknown PierSide label: {other}"),
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    let mut last = (PierSide::Unknown, false);
    while std::time::Instant::now() < deadline {
        let side_now = world.mount().side_of_pier().await.unwrap();
        let tracking = world.mount().tracking().await.unwrap();
        last = (side_now, tracking);
        if side_now == want && tracking {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "mount did not resume tracking on pier side {want:?} within {secs}s; \
         last observed (side, tracking) = {last:?}"
    );
}

#[then(expr = "the mount should not have received command {word}")]
async fn mount_did_not_receive_command(world: &mut StarAdventurerWorld, command: String) {
    // Point-in-time absence check: scenarios order this after a
    // "still be tracking after N ms" / "stop tracking within N ms"
    // wait, which gives any wrongly-triggered flip time to reach the
    // wire first.
    let want = format!("{command}\r");
    let log = world.command_log().await;
    assert!(
        !log.iter().any(|c| c == &want),
        "expected wire frame {command:?} to be absent, but found it; log size {}",
        log.len()
    );
}
