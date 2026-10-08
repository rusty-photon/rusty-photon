//! Steps for sync.feature.

#![allow(unused_variables)]

use crate::world::StarAdventurerWorld;
use cucumber::{given, when};

#[when(expr = "I sync to RA {float} hours and Dec {float} degrees")]
async fn sync_to(world: &mut StarAdventurerWorld, ra: f64, dec: f64) {
    world.mount().sync_to_coordinates(ra, dec).await.unwrap();
}

#[when(expr = "I try to sync to RA {float} hours and Dec {float} degrees")]
async fn try_sync_to(world: &mut StarAdventurerWorld, ra: f64, dec: f64) {
    match world.mount().sync_to_coordinates(ra, dec).await {
        Ok(()) => world.clear_error(),
        Err(e) => world.record_error(e),
    }
}

#[when("I sync to the stored target")]
async fn sync_to_target(world: &mut StarAdventurerWorld) {
    world.mount().sync_to_target().await.unwrap();
}

#[when("I try to sync to the stored target")]
async fn try_sync_to_target(world: &mut StarAdventurerWorld) {
    match world.mount().sync_to_target().await {
        Ok(()) => world.clear_error(),
        Err(e) => world.record_error(e),
    }
}

#[given("the mount reports the Dec axis running in tracking mode")]
async fn dec_axis_running_in_tracking_mode(world: &mut StarAdventurerWorld) {
    // How the `GTi` reports an axis coasting to a stop after a goto's `:K`
    // or `:L`: running, in tracking mode. The driver has set no tracking
    // rate on Dec, so it is neither tracking nor guiding.
    world.queue_seed("dec_running", true.into()).await;
    world.queue_seed("dec_goto", false.into()).await;
    world.queue_seed("ra_initialized", true.into()).await;
    world.queue_seed("dec_initialized", true.into()).await;
}
