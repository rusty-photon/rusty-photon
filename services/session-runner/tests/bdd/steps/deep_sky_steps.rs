//! BDD step definitions for the shipped deep-sky workflow document
//! (design § `deep_sky.json`): the dispatch loop against rp's real
//! planner, target switching on visibility, the refocus and
//! meridian-flip trigger overlay, and safety resume with
//! re-acquisition.
//!
//! The planner evaluates real ephemeris at wall-clock now, so these
//! steps compute the observing site to fit the clock
//! (`bdd_infra::rp_harness::ComputedSky`): an equatorial site at the
//! anti-solar longitude is always in deep astronomical night, and
//! celestial-equator targets placed by hour angle sink at a constant
//! ≈ 0.25°/minute — which makes "the first target drops below its
//! floor N seconds from now" exact. The dawn scenario flips the
//! trick: a site 45° west of the sub-solar longitude has a risen,
//! still-climbing morning Sun at any moment, so the planner declares
//! the night over. The simulated mount is taught the
//! same site (rp hard-errors on mount connect when the mount's
//! reported site disagrees with config) and synced onto the first
//! target so every document slew stays sub-degree (`OmniSim` slews at
//! real-mount speed).

use std::time::Duration;

use cucumber::gherkin::Step;
use cucumber::{given, then, when};

use bdd_infra::rp_harness::{
    CannedGuiding, CannedWcs, ComputedSky, GuiderConfig, GuiderStub, GuiderStubBehavior, IcrsCoord,
    OmniSimHandle, OpticalTrainConfig, PlateSolverConfig, PlateSolverStub, StubBehavior,
    TrainAutoFocusConfig,
};

use crate::steps::infrastructure::{
    ensure_omnisim, stage_run, start_rp_service, start_session_runner_service,
};
use crate::world::SessionRunnerWorld;

use crate::steps::observation::{await_blackboard_counter, frame_budget, timeout_report};

// ---------------------------------------------------------------------------
// Given steps: the computed night sky
// ---------------------------------------------------------------------------

/// A `desired_count` large enough that the scenario's frame budget is hit
/// first: the store requires a finite goal, so a legacy count-less "plan
/// drives the duration, but never exhausts" entry is emulated this way.
const NO_FINITE_GOAL: u32 = 1000;

#[given("an observing site where it is astronomical night with one planner target")]
async fn night_sky_with_one_target(world: &mut SessionRunnerWorld) {
    push_one_night_target(world, Vec::new());
}

#[given(
    expr = "an observing site where it is astronomical night with one planner target whose \
            exposure plan is a single unfiltered {int}-second frame"
)]
async fn night_sky_with_one_planned_target(world: &mut SessionRunnerWorld, seconds: u64) {
    push_one_night_target(world, vec![unfiltered_goal(seconds, NO_FINITE_GOAL)]);
}

#[given(
    expr = "an observing site where it is astronomical night with one planner target whose \
            integration goal is {int} unfiltered {int}-second frames"
)]
async fn night_sky_with_one_goal_target(world: &mut SessionRunnerWorld, count: u32, seconds: u64) {
    push_one_night_target(world, vec![unfiltered_goal(seconds, count)]);
}

/// One unfiltered goal at `desired_count` × `seconds`s, in the JSON shape
/// `add_target` accepts for its `goals[]` parameter.
fn unfiltered_goal(seconds: u64, desired_count: u32) -> serde_json::Value {
    serde_json::json!({
        "filter": "",
        "binning": "1x1",
        "exposure_duration": format!("{seconds}s"),
        "desired_count": desired_count
    })
}

/// Build the `add_target` args for a night target: computed coordinates,
/// an optional per-target altitude floor, and optional goals. Staged on
/// `pending_store_targets` and seeded into rp's redb store post-boot by
/// `start_rp_service`.
fn night_target_args(
    name: &str,
    coord: &IcrsCoord,
    min_altitude_degrees: Option<f64>,
    goals: Vec<serde_json::Value>,
) -> serde_json::Value {
    let mut args = serde_json::json!({
        "display_name": name,
        "ra_hours": coord.ra_hours,
        "dec_degrees": coord.dec_degrees,
    });
    if let Some(floor) = min_altitude_degrees {
        args["scheduling"] = serde_json::json!({ "min_altitude_degrees": floor });
    }
    if !goals.is_empty() {
        args["goals"] = serde_json::Value::Array(goals);
    }
    args
}

fn push_one_night_target(world: &mut SessionRunnerWorld, goals: Vec<serde_json::Value>) {
    let sky = ComputedSky::night_at(chrono::Utc::now());
    // Half an hour past transit: ≈ 82.5° altitude and sinking, next
    // meridian crossing ≈ 23.5 sidereal hours away — no flip pressure,
    // viable for hours against the planner-wide 20° floor.
    let target = sky.target_at_hour_angle(0.5);
    world.site = Some((sky.latitude_degrees(), sky.longitude_degrees()));
    world
        .pending_store_targets
        .push(night_target_args("primary", &target, None, goals));
    world.night_targets.push(target);
}

#[given(
    expr = "an observing site where it is astronomical night and the first of two planner \
            targets sinks below its floor after {int} seconds"
)]
async fn night_sky_with_sinking_target(world: &mut SessionRunnerWorld, seconds: i64) {
    let sky = ComputedSky::night_at(chrono::Utc::now());
    // Both targets descend in the west near 45° altitude, 0.05 h of
    // hour angle (0.75° of sky) apart so the switch slew is quick. They
    // fall inside the transit tie band, so the planner breaks the tie by
    // store (slug) order — "primary" sorts before "secondary", so the
    // sinker is acquired first (and the mount is pre-synced onto it). Its
    // floor is the altitude it will have `seconds` from now, which is when
    // the planner drops it and the backup (on the planner-wide 20° floor,
    // ≈ 90 minutes of margin) takes over.
    let sinker = sky.target_at_hour_angle(3.0);
    let backup = sky.target_at_hour_angle(3.05);
    let floor = sky.altitude_degrees_in(sinker, seconds);
    world.site = Some((sky.latitude_degrees(), sky.longitude_degrees()));
    world.pending_store_targets.push(night_target_args(
        "primary",
        &sinker,
        Some(floor),
        Vec::new(),
    ));
    world
        .pending_store_targets
        .push(night_target_args("secondary", &backup, None, Vec::new()));
    world.night_targets.push(sinker);
    world.night_targets.push(backup);
}

#[given(
    "an observing site where the morning sun has risen and one planner target sits below \
     its floor"
)]
async fn morning_sky_with_one_unviable_target(world: &mut SessionRunnerWorld) {
    let sky = ComputedSky::morning_at(chrono::Utc::now());
    // High in the sky (so the mount can sync onto it) but pinned below
    // its own floor: no altitude reaches 90°, so the planner never
    // recommends it and must fall through to the sky gating — a bright
    // rising Sun, i.e. end_of_session.
    let target = sky.target_at_hour_angle(0.5);
    world.site = Some((sky.latitude_degrees(), sky.longitude_degrees()));
    world.pending_store_targets.push(night_target_args(
        "unreachable",
        &target,
        Some(90.0),
        Vec::new(),
    ));
    world.night_targets.push(target);
}

// `pub`: the sky-flat suite re-points the same machinery at the zenith
// spot it pushed as its only computed target.
#[given("the simulated mount matches the site and points at the first target")]
pub async fn mount_matches_site_and_target(world: &mut SessionRunnerWorld) {
    ensure_omnisim(world).await;
    let (lat, lon) = world
        .site
        .expect("compute the observing site before configuring the mount");
    let first = *world
        .night_targets
        .first()
        .expect("compute the planner targets before configuring the mount");
    // Capture the site before overwriting it — it is a profile setting
    // the per-scenario restart does not reset, so the after-hook puts
    // it back for whatever suite reuses this OmniSim next.
    world.original_telescope_site = Some(
        OmniSimHandle::get_telescope_site()
            .await
            .expect("failed to read the simulated mount's site"),
    );
    OmniSimHandle::set_telescope_site(lat, lon)
        .await
        .expect("failed to set the simulated mount's site");
    // OmniSim requires tracking for SyncToCoordinates; the sync
    // teleports the mount's coordinate frame onto the target without
    // physical motion, so the document's acquisition slew is
    // effectively zero-distance.
    OmniSimHandle::set_telescope_tracking(true)
        .await
        .expect("failed to enable the simulated mount's tracking");
    OmniSimHandle::sync_telescope_to(first.ra_hours, first.dec_degrees)
        .await
        .expect("failed to sync the simulated mount onto the first target");
}

// ---------------------------------------------------------------------------
// Given steps: the guider stub
// ---------------------------------------------------------------------------

/// A guider stub whose every endpoint canned-succeeds with a static
/// active loop — the guided-cadence scenario only needs start, dither,
/// and stop to answer.
#[given("a stub guider accepting guide commands")]
async fn stub_guider_accepting_commands(world: &mut SessionRunnerWorld) {
    start_guider_stub(world, CannedGuiding::default()).await;
}

/// A guider stub in lifecycle mode: inactive (no frames, no HFD
/// script consumption) until the document's `start_guiding` lands, so
/// rp's focus watch fires its events only during the run — after the
/// engine's event intake is open.
#[given(expr = "a lifecycle stub guider with the HFD script {string}")]
async fn lifecycle_stub_guider(world: &mut SessionRunnerWorld, script: String) {
    let metrics_hfd_script: Vec<f64> = script
        .split(',')
        .map(|s| s.trim().parse().expect("HFD script entries are numbers"))
        .collect();
    start_guider_stub(
        world,
        CannedGuiding {
            lifecycle: true,
            metrics_hfd_script,
            ..CannedGuiding::default()
        },
    )
    .await;
}

async fn start_guider_stub(world: &mut SessionRunnerWorld, canned: CannedGuiding) {
    let stub = GuiderStub::start(GuiderStubBehavior::Canned(canned)).await;
    let mut guider = GuiderConfig::url_only(stub.url.clone());
    // The document's dither call carries no amount: rp falls back to
    // the guiding config's dither_pixels, which is rig geometry and
    // belongs in rp's config, not in workflow parameters.
    guider.dither_pixels = Some(1.5);
    world.guider = Some(guider);
    world.guider_stub = Some(stub);
}

/// rp's Guide Focus Watch config (`equipment.mount.guiding.focus_watch`).
/// Cooldown is left long so a degradation fires exactly once.
#[given(
    expr = "the stub guider has a focus watch of window {int}, poll interval {string}, and escalation deadline {string}"
)]
async fn stub_guider_focus_watch(
    world: &mut SessionRunnerWorld,
    window: i64,
    poll_interval: String,
    escalation_deadline: String,
) {
    let guider = world
        .guider
        .as_mut()
        .expect("add the stub guider before its focus watch");
    guider.focus_watch = Some(serde_json::json!({
        "window": window,
        "degrade_ratio": 1.25,
        "cooldown": "10m",
        "escalation_deadline": escalation_deadline,
        "poll_interval": poll_interval,
    }));
}

/// The guiding train the watch events name: the simulator's focuser as
/// its terminal focuser (the metric sweep moves it) and an offline
/// guide camera (the metric sweep never captures, so the roster entry
/// only has to exist).
#[given(
    expr = "a guiding train {string} on the simulator's focuser with a metric auto_focus block"
)]
async fn guiding_train_on_simulator_focuser(world: &mut SessionRunnerWorld, train_id: String) {
    ensure_omnisim(world).await;
    world.focusers.push(bdd_infra::rp_harness::FocuserConfig {
        id: "guide-focuser".to_string(),
        alpaca_url: world.omnisim_url(),
        device_number: 0,
        min_position: None,
        max_position: None,
    });
    world.cameras.push(bdd_infra::rp_harness::CameraConfig {
        id: "guide-cam".to_string(),
        alpaca_url: "not-a-url".to_string(),
        device_number: 0,
        cooler_targets_c: Vec::new(),
    });
    world.optical_trains.push(OpticalTrainConfig {
        id: train_id,
        purpose: Some("guiding".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices: vec!["guide-focuser".to_string(), "guide-cam".to_string()],
        auto_focus: Some(TrainAutoFocusConfig {
            duration: None,
            step_size: 50,
            half_width: 100,
            min_area: None,
            max_area: None,
            frames_per_step: Some(2),
        }),
    });
}

#[given("a stub plate solver echoing the first target")]
async fn stub_plate_solver_echoing_first_target(world: &mut SessionRunnerWorld) {
    let first = *world
        .night_targets
        .first()
        .expect("compute the planner targets before starting the plate-solver stub");
    // Solved center == target center (WCS is degrees on the wire), so
    // center_on_target converges on its first iteration.
    let stub = PlateSolverStub::start(StubBehavior::Canned(CannedWcs {
        ra_center: first.ra_hours * 15.0,
        dec_center: first.dec_degrees,
        pixel_scale_arcsec: 1.05,
        rotation_deg: 0.0,
        solver: "stub-astap-1.0".to_string(),
        wcs_matrix: None,
    }))
    .await;
    world.plate_solver = Some(PlateSolverConfig {
        url: stub.url.clone(),
        timeout: None,
        default_search_radius_deg: None,
    });
    world.plate_solver_stub = Some(stub);
}

// ---------------------------------------------------------------------------
// Given steps: process topology
// ---------------------------------------------------------------------------

#[given(
    expr = "rp is running with a camera, a mount, and session-runner running the {string} \
            workflow with parameters:"
)]
async fn rp_with_camera_mount_and_workflow(
    world: &mut SessionRunnerWorld,
    workflow: String,
    step: &Step,
) {
    configure_deep_sky_equipment(world, false).await;
    stage_deep_sky(world, &workflow, step);
    start_rp_service(world).await;
    start_session_runner_service(world).await;
}

#[given(
    expr = "rp is running with a camera, a mount, a focuser, and session-runner running the \
            {string} workflow with parameters:"
)]
async fn rp_with_camera_mount_focuser_and_workflow(
    world: &mut SessionRunnerWorld,
    workflow: String,
    step: &Step,
) {
    configure_deep_sky_equipment(world, true).await;
    stage_deep_sky(world, &workflow, step);
    start_rp_service(world).await;
    start_session_runner_service(world).await;
}

// ---------------------------------------------------------------------------
// When steps
// ---------------------------------------------------------------------------

#[when(expr = "the deep-sky session has captured at least {int} frames")]
async fn deep_sky_captured_frames(world: &mut SessionRunnerWorld, frames: u64) {
    await_blackboard_counter(world, "total_frames", frames).await;
}

// ---------------------------------------------------------------------------
// Then steps
// ---------------------------------------------------------------------------

#[then(expr = "a second {string} event should be observed within {int} seconds")]
async fn second_event_observed_within(
    world: &mut SessionRunnerWorld,
    event_type: String,
    seconds: u64,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let count = sse_event_seqs(world, &event_type).await.len();
        if count >= 2 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "expected a second '{event_type}' event within {seconds}s, saw {count}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[then(expr = "at least {int} {string} event(s) should precede the second {string} on the stream")]
async fn events_precede_second(
    world: &mut SessionRunnerWorld,
    minimum: usize,
    needle: String,
    boundary: String,
) {
    let boundary_seq = second_seq(world, &boundary).await;
    let count = sse_event_seqs(world, &needle)
        .await
        .into_iter()
        .filter(|seq| *seq < boundary_seq)
        .count();
    assert!(
        count >= minimum,
        "expected at least {minimum} '{needle}' event(s) before the second '{boundary}' \
         (seq {boundary_seq}), saw {count}"
    );
}

#[then(expr = "at least {int} {string} event(s) should follow the second {string} on the stream")]
async fn events_follow_second(
    world: &mut SessionRunnerWorld,
    minimum: usize,
    needle: String,
    boundary: String,
) {
    let boundary_seq = second_seq(world, &boundary).await;
    // Each awaited event is one more frame's worth of real work, so this
    // waits on the same pipeline the frame-count steps do.
    let budget = frame_budget(world, minimum as u64);
    let started = std::time::Instant::now();
    loop {
        let count = sse_event_seqs(world, &needle)
            .await
            .into_iter()
            .filter(|seq| *seq > boundary_seq)
            .count();
        if count >= minimum {
            return;
        }
        let elapsed = started.elapsed();
        if elapsed >= budget {
            let headline = format!(
                "expected at least {minimum} '{needle}' event(s) after the second \
                 '{boundary}' (seq {boundary_seq}), saw {count}"
            );
            panic!(
                "{}",
                timeout_report(world, &headline, budget, elapsed).await
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[then(expr = "the SSE stream should show at least {int} {string} events")]
async fn sse_shows_at_least(world: &mut SessionRunnerWorld, minimum: usize, event_type: String) {
    let count = crate::steps::trigger_steps::settled_event_count(world, &event_type, minimum).await;
    assert!(
        count >= minimum,
        "expected at least {minimum} '{event_type}' events on the SSE stream, saw {count}"
    );
}

#[then(expr = "the stub guider should have received exactly {int} {string} request(s)")]
async fn stub_guider_received_exactly(
    world: &mut SessionRunnerWorld,
    expected: usize,
    path_suffix: String,
) {
    let stub = world
        .guider_stub
        .as_ref()
        .expect("no guider stub — add the 'a stub guider …' step");
    let requests = stub.requests_to(&path_suffix).await;
    assert_eq!(
        requests.len(),
        expected,
        "expected exactly {expected} request(s) to '{path_suffix}', saw {}",
        requests.len()
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The deep-sky equipment set: one camera and the singular mount —
/// plus, for the refocus scenarios, a focuser and the imaging train's
/// `auto_focus` block (the document is train-addressed, so sweep
/// geometry lives on the train, not in parameters). No filter wheel —
/// the scenarios leave the document's `filter` parameter at its empty
/// default and give their planner targets unfiltered exposure plans
/// (if any), so `set_filter` never runs.
async fn configure_deep_sky_equipment(world: &mut SessionRunnerWorld, with_focuser: bool) {
    ensure_omnisim(world).await;
    crate::steps::infrastructure::ensure_camera(world);
    crate::steps::infrastructure::ensure_mount(world);
    let (devices, auto_focus) = if with_focuser {
        crate::steps::infrastructure::ensure_focuser(world);
        (
            vec!["main-focuser".to_string(), "main-cam".to_string()],
            Some(TrainAutoFocusConfig {
                duration: Some("100ms".to_string()),
                step_size: 100,
                half_width: 200,
                min_area: Some(5),
                max_area: Some(65536),
                frames_per_step: None,
            }),
        )
    } else {
        (vec!["main-cam".to_string()], None)
    };
    world.optical_trains.push(OpticalTrainConfig {
        id: "main".to_string(),
        purpose: Some("imaging".to_string()),
        focal_length_mm: None,
        default_position_angle_degrees: None,
        devices,
        auto_focus,
    });
}

/// Register the shipped `deep_sky` document as the orchestrator's
/// workflow, with the scenario table's parameters on top of the fixed
/// imaging-train id. Table rows are `| name | value |` (no header);
/// values are coerced in order: boolean, integer, number, else string
/// (so humantime durations like `500ms` stay strings).
fn stage_deep_sky(world: &mut SessionRunnerWorld, workflow: &str, step: &Step) {
    let mut parameters = serde_json::json!({ "train_id": "main" });
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
        parameters[name] = coerce_parameter(value);
    }
    stage_run(world, workflow, Some(parameters));
}

pub fn coerce_parameter(value: &str) -> serde_json::Value {
    if let Ok(b) = value.parse::<bool>() {
        return serde_json::json!(b);
    }
    if let Ok(i) = value.parse::<i64>() {
        return serde_json::json!(i);
    }
    if let Ok(f) = value.parse::<f64>() {
        // Rust's f64 parser accepts "NaN"/"inf", which JSON cannot
        // represent (`json!` would map them to null — a silently wrong
        // parameter). Only finite values are numbers; anything else
        // falls through as the literal string, which the engine's
        // parameter type-check then rejects loudly.
        if f.is_finite() {
            return serde_json::json!(f);
        }
    }
    serde_json::json!(value)
}

/// Stream-sequence ids of every SSE frame of the given event type, in
/// arrival order.
async fn sse_event_seqs(world: &SessionRunnerWorld, event_type: &str) -> Vec<u64> {
    let client = world
        .sse_client
        .as_ref()
        .expect("no SSE client — add the 'an SSE client is watching' step");
    client
        .frames()
        .await
        .iter()
        .filter(|f| f.event_type().as_deref() == Some(event_type))
        .map(|f| {
            f.id.unwrap_or_else(|| panic!("a '{event_type}' frame carries no stream sequence id"))
        })
        .collect()
}

/// The stream-sequence id of the second event of the given type —
/// asserted present (add the "a second … should be observed" step
/// first when the second occurrence needs waiting for).
async fn second_seq(world: &SessionRunnerWorld, event_type: &str) -> u64 {
    let seqs = sse_event_seqs(world, event_type).await;
    assert!(
        seqs.len() >= 2,
        "expected at least two '{event_type}' events on the stream, saw {}",
        seqs.len()
    );
    seqs[1]
}
