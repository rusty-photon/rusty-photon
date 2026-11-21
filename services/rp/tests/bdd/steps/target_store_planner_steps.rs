//! BDD step definitions for the planner's altitude gating of
//! target-store targets (`target_store_planner.feature`, rp.md § Decision
//! Logic (inside `get_next_target`)).
//!
//! Unlike the other `target_store_*.feature` suites these scenarios boot
//! rp the ordinary OmniSim/mount way (`tool_steps::start_rp`), matching
//! `planner.feature`'s convention, since they exercise `get_next_target`
//! against a real site/ephemeris — `write_target_store_config`'s
//! lightweight no-OmniSim launcher is for the pure plan-data CRUD
//! suites that don't need a mount or site.

use cucumber::gherkin::Step;
use cucumber::given;

use crate::steps::target_store_crud_steps::add_target_fixture;
use crate::steps::target_store_goals_steps::goals_from_table;
use crate::world::RpWorld;

#[given(expr = "rp is configured with a target-store default minimum altitude of {float} degrees")]
fn default_min_altitude(world: &mut RpWorld, degrees: f64) {
    world.target_store_config = Some(serde_json::json!({
        "default_scheduling": { "min_altitude_degrees": degrees }
    }));
}

// -----------------------------------------------------------------------
// Always-/never-visible seeds for the `planner` feature. A store target
// pinned at `scheduling.min_altitude_degrees` -90 survives altitude
// elimination at any wall-clock (so `get_next_target` is deterministic
// without a computed sky); +90 is eliminated at any wall-clock (forcing
// the no-survivors branch the dusk/dawn scenarios pin). Coordinates are
// ra 0 / dec 0 — an exact transit tie between two such targets, so the
// progress tie-breaker decides. Seeded post-boot via the `add_target`
// MCP tool: targets live in the store, not in config.
// -----------------------------------------------------------------------

#[given(expr = "the MCP client has added the always-visible target {string}")]
async fn added_always_visible_target(world: &mut RpWorld, display_name: String) {
    add_target_fixture(
        world,
        serde_json::json!({
            "display_name": display_name,
            "ra_hours": 0.0,
            "dec_degrees": 0.0,
            "scheduling": { "min_altitude_degrees": -90.0 }
        }),
    )
    .await;
}

#[given(expr = "the MCP client has added the always-visible target {string} with goals:")]
async fn added_always_visible_target_with_goals(
    world: &mut RpWorld,
    display_name: String,
    step: &Step,
) {
    let goals = goals_from_table(step);
    add_target_fixture(
        world,
        serde_json::json!({
            "display_name": display_name,
            "ra_hours": 0.0,
            "dec_degrees": 0.0,
            "scheduling": { "min_altitude_degrees": -90.0 },
            "goals": goals
        }),
    )
    .await;
}

#[given(expr = "the MCP client has added the never-visible target {string}")]
async fn added_never_visible_target(world: &mut RpWorld, display_name: String) {
    add_target_fixture(
        world,
        serde_json::json!({
            "display_name": display_name,
            "ra_hours": 0.0,
            "dec_degrees": 0.0,
            "scheduling": { "min_altitude_degrees": 90.0 }
        }),
    )
    .await;
}

#[given(
    expr = "the MCP client has added the always-visible targets {string} and {string}, each \
            wanting {int} unfiltered {int}-second frames"
)]
async fn added_two_always_visible_targets_with_goals(
    world: &mut RpWorld,
    first: String,
    second: String,
    count: u32,
    duration_secs: u32,
) {
    for name in [first, second] {
        add_target_fixture(
            world,
            serde_json::json!({
                "display_name": name,
                "ra_hours": 0.0,
                "dec_degrees": 0.0,
                "scheduling": { "min_altitude_degrees": -90.0 },
                "goals": [{
                    "filter": "",
                    "binning": "1x1",
                    "exposure_duration": format!("{duration_secs}s"),
                    "desired_count": count
                }]
            }),
        )
        .await;
    }
}

#[given(
    expr = "the MCP client has added a target named {string} at ra_hours {float} dec_degrees {float} with min_altitude_degrees {float}"
)]
async fn added_target_with_altitude_floor(
    world: &mut RpWorld,
    display_name: String,
    ra_hours: f64,
    dec_degrees: f64,
    min_altitude_degrees: f64,
) {
    add_target_fixture(
        world,
        serde_json::json!({
            "display_name": display_name,
            "ra_hours": ra_hours,
            "dec_degrees": dec_degrees,
            "scheduling": { "min_altitude_degrees": min_altitude_degrees }
        }),
    )
    .await;
}

// "the MCP client has added a target named {string} at ra_hours {float}
// dec_degrees {float}" (no altitude override) is reused from
// target_store_crud_steps.rs; "the MCP client calls \"get_next_target\""
// and "the result reason should be {string}" from ephemeris_steps.rs.
