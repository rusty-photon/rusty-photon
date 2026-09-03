//! BDD step definitions for what survives an rp restart (rp.md § What
//! Survives an rp Restart): crashing rp and restarting it, so the
//! scenario can assert that derived progress comes back from the frames
//! on disk — rp itself persists no run state and re-invokes nobody.

use cucumber::when;

use crate::steps::tool_steps::start_rp;
use crate::world::RpWorld;

#[when("rp is killed")]
async fn rp_is_killed(world: &mut RpWorld) {
    // Drop the clients bound to the dying process first — they cannot
    // survive the port change across the respawn anyway.
    world.mcp_client = None;
    world.sse_client = None;
    world.rp.as_mut().expect("rp is not running").kill().await;
    world.rp = None;
}

#[when("rp is restarted after the crash")]
async fn rp_is_restarted_after_crash(world: &mut RpWorld) {
    assert!(
        world.rp.is_none(),
        "rp is still running — kill it before restarting"
    );
    start_rp(world).await;
}
