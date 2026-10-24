//! Steps more than one feature uses: the rp the BFF reads, the BFF pointed
//! at it, and the checks on a rendered page.

use cucumber::{given, then};

use crate::dom;
use crate::snapshot;
use crate::world::UiWorld;

#[given("a running rp orchestrator with an empty roster")]
async fn running_rp_empty_roster(world: &mut UiWorld) {
    world.start_rp_with_empty_roster().await;
}

#[given("a BFF pointed at rp")]
async fn bff_pointed_at_rp(world: &mut UiWorld) {
    world.start_bff_with_rp().await;
}

#[given("a BFF pointed at an unreachable rp")]
async fn bff_unreachable_rp(world: &mut UiWorld) {
    world.start_bff_with_unreachable_rp().await;
}

#[then(regex = r#"^the page shows an error banner mentioning "([^"]+)"$"#)]
fn error_banner(world: &mut UiWorld, needle: String) {
    assert!(
        dom::text_contains(&world.last_body, ".banner.error", &needle),
        "no error banner mentioning {needle:?}:\n{}",
        world.last_body
    );
}

#[then(regex = r#"^the rendered output matches the "([\w-]+)" snapshot$"#)]
fn matches_snapshot(world: &mut UiWorld, name: String) {
    snapshot::assert_html(&name, &world.last_body);
}
