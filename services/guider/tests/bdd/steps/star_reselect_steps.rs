//! Steps of `star_reselect.feature`: `star/reselect`, and the slews that
//! take the stars out of the frame and bring them back.

use std::time::Duration;

use cucumber::{given, then, when};
use guide_simulator::{PierSide, SkyOffset};

use crate::world::GuiderWorld;

/// How long a scripted slew takes.
const SLEW: Duration = Duration::from_secs(1);

#[when("a client asks the guider to reselect the guide stars")]
async fn ask_to_reselect(world: &mut GuiderWorld) {
    world.post("/api/v1/star/reselect", None).await;
}

#[when(regex = r"^a client asks the guider to reselect the guide stars with the body (.+)$")]
async fn ask_to_reselect_with(world: &mut GuiderWorld, body: String) {
    world.post("/api/v1/star/reselect", Some(&body)).await;
}

#[given(regex = r"^the simulated mount has slewed 1° (east|west) and stopped$")]
#[then(regex = r"^the simulated mount slews 1° (east|west) and stops$")]
async fn slew(world: &mut GuiderWorld, direction: String) {
    let east = if direction == "east" {
        3_600.0
    } else {
        -3_600.0
    };
    let sim = world.sim();
    let end = sim.now().after(SLEW);
    sim.slew(SkyOffset { east, north: 0.0 }, SLEW, PierSide::East)
        .expect("slew the simulated mount");
    // The guider's next frame after one that started once the slew ended
    // shows the field where the slew left it.
    world
        .wait_until("an exposure started after the slew", || {
            world
                .sim()
                .exposures()
                .last()
                .is_some_and(|exposure| exposure.at >= end)
        })
        .await;
}
