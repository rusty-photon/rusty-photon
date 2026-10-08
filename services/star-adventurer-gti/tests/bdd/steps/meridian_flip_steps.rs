//! Steps for `meridian_flip.feature`.
//!
//! Most pier-side and connection steps are shared with
//! `side_of_pier_steps.rs` and `connection_steps.rs`; this file only
//! adds the steps unique to the meridian-flip behaviour
//! (`CanSetPierSide` read, parametric `SetSideOfPier` setter, abort
//! step).

#![allow(unused_variables)]

use crate::world::StarAdventurerWorld;
use ascom_alpaca::api::telescope::PierSide;
use cucumber::{then, when};

#[when(expr = "I set SideOfPier to {word}")]
async fn set_side_of_pier_to(world: &mut StarAdventurerWorld, label: String) {
    world
        .mount()
        .set_side_of_pier(pier_side_from_label(&label))
        .await
        .expect("SetSideOfPier should succeed in this scenario");
}

#[when(expr = "I try to set SideOfPier to {word}")]
async fn try_set_side_of_pier_to(world: &mut StarAdventurerWorld, label: String) {
    match world
        .mount()
        .set_side_of_pier(pier_side_from_label(&label))
        .await
    {
        Ok(()) => world.clear_error(),
        Err(e) => world.record_error(e),
    }
}

#[then(expr = "CanSetPierSide should be {word}")]
async fn can_set_pier_side_should_be(world: &mut StarAdventurerWorld, expected: String) {
    let want = match expected.as_str() {
        "true" => true,
        "false" => false,
        other => panic!("expected 'true' or 'false', got {other}"),
    };
    let got = world.mount().can_set_pier_side().await.unwrap();
    assert_eq!(got, want, "CanSetPierSide mismatch");
}

fn pier_side_from_label(label: &str) -> PierSide {
    match label {
        "East" => PierSide::East,
        "West" => PierSide::West,
        "Unknown" => PierSide::Unknown,
        other => panic!("unknown PierSide label: {other}"),
    }
}
