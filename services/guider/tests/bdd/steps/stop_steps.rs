//! Steps of `stop.feature`: `guiding/stop`, and what it leaves of the
//! simulated camera's exposure.

use cucumber::{given, then, when};

use crate::world::GuiderWorld;

#[given("a client asks the guider to stop")]
#[when("a client asks the guider to stop")]
async fn ask_to_stop(world: &mut GuiderWorld) {
    world.post("/api/v1/guiding/stop", None).await;
}

#[when("the guider is told to shut down")]
async fn told_to_shut_down(world: &mut GuiderWorld) {
    world
        .guider
        .as_mut()
        .expect("the guider is running")
        .stop()
        .await;
}

#[then(regex = r"^the simulated camera has received (\d+) aborts?$")]
fn aborts(world: &mut GuiderWorld, count: usize) {
    assert_eq!(world.sim().aborts(), count, "the camera's aborts");
}

#[then(regex = r"^the simulated camera has started (\d+) exposures?$")]
fn exposures(world: &mut GuiderWorld, count: usize) {
    assert_eq!(
        world.sim().exposures().len(),
        count,
        "the camera's exposures"
    );
}

#[then("the simulated camera's last exposure has ended")]
fn last_exposure_ended(world: &mut GuiderWorld) {
    let answered_at = world
        .answered_at
        .expect("the scenario's request was answered");
    let exposure = world
        .sim()
        .exposures()
        .pop()
        .expect("the camera has started an exposure");
    let end = exposure.at.after(exposure.duration);
    assert!(
        end <= answered_at,
        "the request was answered at {answered_at:?}, before the exposure's end at {end:?}"
    );
}

#[then(
    regex = r#"^the guider's stats report the state "([a-z_]+)" once the simulated camera's last exposure has ended$"#
)]
async fn state_once_exposure_ended(world: &mut GuiderWorld, state: String) {
    world
        .wait_for(
            "/api/v1/guiding/stats",
            &format!("the state {state}"),
            |status, body| status == 200 && body["state"] == state.as_str(),
        )
        .await;
    let seen_at = world.sim().now();
    let exposure = world
        .sim()
        .exposures()
        .pop()
        .expect("the camera has started an exposure");
    let end = exposure.at.after(exposure.duration);
    assert!(
        end <= seen_at,
        "the guider was {state} at {seen_at:?}, before its exposure's end at {end:?}"
    );
}
