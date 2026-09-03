//! BDD step definitions for the shipped sky-flat workflow document
//! (design § `sky_flat.json`): zenith pointing computed from the live
//! LST, per-filter twilight flats with per-frame exposure rescaling.
//!
//! `OmniSim`'s image content does not track exposure duration, so these
//! scenarios pin the end-to-end plumbing only — the registration's 0.5
//! target fraction and 1.0 tolerance make every simulated frame land
//! in-band deterministically (a median can never stray more than 100%
//! from half of `max_adu`). The adaptation math is pinned by the
//! engine's exec tests against scripted medians.

use cucumber::gherkin::Step;
use cucumber::given;

use bdd_infra::rp_harness::ComputedSky;

use crate::steps::infrastructure::{
    ensure_camera, ensure_filter_wheel, ensure_mount, ensure_omnisim, stage_run, start_rp_service,
    start_session_runner_service,
};
use crate::world::SessionRunnerWorld;

#[given("an observing site where it is astronomical night")]
async fn night_sky_without_targets(world: &mut SessionRunnerWorld) {
    let sky = ComputedSky::night_at(chrono::Utc::now());
    world.site = Some((sky.latitude_degrees(), sky.longitude_degrees()));
    // The zenith spot the document will compute at run time (RA = LST,
    // dec = the computed site's latitude, 0), kept as the scenario's
    // only computed target so the mount-sync step lands next to it.
    world.night_targets.push(sky.target_at_hour_angle(0.0));
}

#[given("the simulated mount matches the site and points at the zenith")]
async fn mount_matches_site_and_zenith(world: &mut SessionRunnerWorld) {
    // The zenith spot is the scenario's first (and only) computed
    // target, so the deep-sky mount step does exactly the right thing.
    crate::steps::deep_sky_steps::mount_matches_site_and_target(world).await;
}

#[given(
    expr = "rp is running with a camera, a mount, a filter wheel, and session-runner running \
            the {string} workflow with parameters:"
)]
async fn rp_with_camera_mount_filter_wheel_and_workflow(
    world: &mut SessionRunnerWorld,
    workflow: String,
    step: &Step,
) {
    configure_sky_flat_equipment(world).await;
    stage_sky_flat(world, &workflow, step);
    start_rp_service(world).await;
    start_session_runner_service(world).await;
}

/// The sky-flat equipment set: one camera, the singular mount, and one
/// filter wheel, all on `OmniSim` device 0.
async fn configure_sky_flat_equipment(world: &mut SessionRunnerWorld) {
    ensure_omnisim(world).await;
    ensure_camera(world);
    ensure_mount(world);
    ensure_filter_wheel(world);
}

/// Register the shipped `sky_flat` document as the orchestrator's
/// workflow: the fixed device ids, the `filters` array from the
/// scenario's declared flat plan, and the scenario table's parameters
/// on top (`| name | value |` rows, coerced like the deep-sky suite's).
fn stage_sky_flat(world: &mut SessionRunnerWorld, workflow: &str, step: &Step) {
    let filters: Vec<serde_json::Value> = world
        .flat_plan
        .iter()
        .map(|(name, count)| serde_json::json!({ "name": name, "count": count }))
        .collect();
    assert!(
        !filters.is_empty(),
        "declare the flat plan first (Given a flat plan of …)"
    );
    let mut parameters = serde_json::json!({
        "camera_id": "main-cam",
        "filter_wheel_id": "main-fw",
        "filters": filters,
    });
    let table = step
        .table
        .as_ref()
        .expect("step requires a `| name | value |` parameters table");
    for row in &table.rows {
        assert_eq!(
            row.len(),
            2,
            "parameter row must be `| name | value |`: {row:?}"
        );
        let (name, value) = (&row[0], &row[1]);
        parameters[name] = crate::steps::deep_sky_steps::coerce_parameter(value);
    }
    stage_run(world, workflow, Some(parameters));
}
