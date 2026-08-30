//! BDD step definitions for target-store progress derivation
//! (`target_store_progress.feature`, rp.md § Target Store → Progress
//! derivation).
//!
//! Progress is derived from the frames on disk, so these scenarios seed
//! real files into the running rp's `session.data_directory` and let the
//! scan find them — no camera and no capture involved. The FITS files
//! are empty: the scan reads filenames, never pixels. Sidecars are
//! written only where a scenario needs a grading verdict.

use cucumber::gherkin::Step;
use cucumber::{given, then};
use serde_json::Value;

use crate::steps::target_store_crud_steps::{start_target_store_rp, write_target_store_config};
use crate::steps::tool_steps::ensure_mcp_client;
use crate::world::RpWorld;

/// The documented default `file_naming_pattern` (rp.md § Persistence) —
/// the same constant `capture_target_linkage_steps.rs` uses, repeated
/// rather than shared so each suite's feature file states the pattern
/// its paths are written against.
const DEFAULT_PATTERN: &str =
    "{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_fpos_{filter_position}_{sensor_temp}";

// ---------------------------------------------------------------------------
// Given — configuration
// ---------------------------------------------------------------------------

#[given(
    expr = "rp is running with a target store, filter roster {string}, and frame naming configured"
)]
async fn rp_running_with_store_and_naming(world: &mut RpWorld, roster: String) {
    let filters: Vec<String> = roster.split(',').map(|s| s.trim().to_string()).collect();
    world.file_naming_pattern = Some(DEFAULT_PATTERN.to_string());
    write_target_store_config(world, Some(filters));
    start_target_store_rp(world).await;
}

/// No filter roster — a wheel-less rig, the only setup on which an
/// unfiltered goal passes `add_target`'s roster validation.
#[given("rp is running with a target store and frame naming configured")]
async fn rp_running_with_store_and_naming_no_roster(world: &mut RpWorld) {
    world.file_naming_pattern = Some(DEFAULT_PATTERN.to_string());
    write_target_store_config(world, None);
    start_target_store_rp(world).await;
}

/// The OmniSim/mount bootstrap's counterpart (`planner.feature`), which
/// builds its config through `RpConfigBuilder` rather than
/// `write_target_store_config`. Pins a data directory so the seeded
/// frames land where the running rp actually looks.
#[given("rp is configured with frame naming")]
fn configured_with_frame_naming(world: &mut RpWorld) {
    world.file_naming_pattern = Some(DEFAULT_PATTERN.to_string());
    // Leave an existing pin alone: `startup_recovery.feature` combines
    // this with its own "data_directory is pinned to a fresh tempdir"
    // step, and re-pinning here would point the seeded frames at a
    // directory the restarted rp never reads.
    if world.pinned_data_directory.is_none() {
        let dir = tempfile::tempdir().expect("create temp data directory");
        world.pinned_data_directory = Some(dir.path().to_string_lossy().into_owned());
        world.pinned_data_dir_holder = Some(dir);
    }
}

#[given(expr = "rp is configured with target-store default grading max_hfr_pixels {float}")]
fn default_grading_hfr(world: &mut RpWorld, max_hfr_pixels: f64) {
    merge_target_store_config(
        world,
        serde_json::json!({ "default_grading": { "max_hfr_pixels": max_hfr_pixels } }),
    );
}

#[given(
    expr = "rp is configured with target-store default grading max_hfr_pixels {float} and min_star_count {int}"
)]
fn default_grading_hfr_and_stars(world: &mut RpWorld, max_hfr_pixels: f64, min_star_count: u32) {
    merge_target_store_config(
        world,
        serde_json::json!({
            "default_grading": {
                "max_hfr_pixels": max_hfr_pixels,
                "min_star_count": min_star_count
            }
        }),
    );
}

/// Merge one key into `world.target_store_config` rather than replacing
/// it, so a scenario can stack this Given with the altitude-default one
/// from `target_store_planner_steps.rs`.
fn merge_target_store_config(world: &mut RpWorld, addition: Value) {
    let mut config = world
        .target_store_config
        .take()
        .unwrap_or_else(|| serde_json::json!({}));
    for (key, value) in addition.as_object().expect("addition must be an object") {
        config[key] = value.clone();
    }
    world.target_store_config = Some(config);
}

#[given(expr = "the MCP client has set its grading max_hfr_pixels to {float}")]
async fn set_target_grading(world: &mut RpWorld, max_hfr_pixels: f64) {
    ensure_mcp_client(world).await;
    let slug = world
        .last_target_slug
        .clone()
        .expect("no target added yet — this step edits the most recently added target");
    let result = world
        .mcp()
        .call_tool(
            "update_target",
            serde_json::json!({
                "slug": slug,
                "grading": { "max_hfr_pixels": max_hfr_pixels }
            }),
        )
        .await;
    result
        .as_ref()
        .unwrap_or_else(|e| panic!("fixture update_target(grading) failed: {e}"));
    world.last_tool_result = Some(result);
}

// ---------------------------------------------------------------------------
// Given — on-disk frames
// ---------------------------------------------------------------------------

/// Seeds `| path | sidecar |` rows under the running rp's data
/// directory. `path` is data-directory-relative and uses `/` separators
/// (joined per-component so the test works on Windows). `sidecar` is
/// one of:
///
/// - `absent` — write the `.fits` file only, no sidecar at all;
/// - `no-grading` — write a sidecar whose `sections` carries no
///   `grading` key;
/// - a JSON object — write it as the sidecar's `sections.grading`.
#[given(expr = "the data directory contains these frames:")]
fn data_directory_contains_frames(world: &mut RpWorld, step: &Step) {
    let data_dir = world.data_directory();
    let table = step
        .table
        .as_ref()
        .expect("step requires a `| path | sidecar |` table");
    let mut rows = table.rows.iter();
    let header = rows.next().expect("frames table must have a header");
    assert_eq!(
        header.as_slice(),
        ["path", "sidecar"],
        "frames table header"
    );

    for row in rows {
        let relative = row[0].trim();
        let sidecar = row[1].trim();
        let mut path = data_dir.clone();
        for component in relative.split('/') {
            path.push(component);
        }
        let parent = path.parent().expect("frame path must have a parent");
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("create frame directory {}: {e}", parent.display()));
        // Empty on purpose: the scan reads filenames, never pixels.
        std::fs::write(&path, b"")
            .unwrap_or_else(|e| panic!("write frame {}: {e}", path.display()));

        let sections = match sidecar {
            "absent" => continue,
            "no-grading" => serde_json::json!({}),
            json => serde_json::json!({
                "grading": serde_json::from_str::<Value>(json)
                    .unwrap_or_else(|e| panic!("sidecar column {json:?} is not JSON: {e}"))
            }),
        };
        let sidecar_path = path.with_extension("json");
        std::fs::write(
            &sidecar_path,
            serde_json::to_string(&serde_json::json!({ "sections": sections }))
                .expect("serialize sidecar"),
        )
        .unwrap_or_else(|e| panic!("write sidecar {}: {e}", sidecar_path.display()));
    }
}

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

// `get_session_progress`, `record_exposure ... filter ...`, and
// `get_target_status for target ...` are all registered globally in
// ephemeris_steps.rs. Reused here, not redefined — cucumber matches step
// text across every steps file in the binary, so a second definition
// (or a `{string}`-generic one that also matches those texts) would be
// an ambiguous-match error rather than a harmless duplicate.

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

#[then(expr = "the reported progress should be exactly:")]
fn reported_progress_exactly(world: &mut RpWorld, step: &Step) {
    let expected = progress_rows_from_table(step);
    let payload = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("expected tool call to succeed");
    let actual = payload
        .get("progress")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("result missing `progress` array: {payload}"));
    assert_eq!(actual, &expected, "derived progress");
}

#[then(expr = "the progress for target {string} should be exactly:")]
fn progress_for_target_exactly(world: &mut RpWorld, target_slug: String, step: &Step) {
    let expected = progress_rows_from_table(step);
    let payload = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("expected tool call to succeed");
    let progress = payload
        .get("progress")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| {
            panic!("get_session_progress result missing `progress` object: {payload}")
        });
    let actual = progress
        .get(&target_slug)
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| {
            panic!("get_session_progress missing target {target_slug:?}: {progress:?}")
        });
    assert_eq!(actual, &expected, "progress for target {target_slug}");
}

fn progress_rows_from_table(step: &Step) -> Vec<Value> {
    let table = step.table.as_ref().expect(
        "step requires a `| filter | binning | exposure_duration | good | total | desired_count |` table",
    );
    let mut rows = table.rows.iter();
    let header = rows.next().expect("progress table must have a header");
    assert_eq!(
        header.as_slice(),
        [
            "filter",
            "binning",
            "exposure_duration",
            "good",
            "total",
            "desired_count"
        ],
        "progress table header"
    );
    rows.map(|row| {
        serde_json::json!({
            "filter": row[0],
            "binning": row[1],
            "exposure_duration": row[2],
            "good": row[3].parse::<u32>().expect("good must parse as u32"),
            "total": row[4].parse::<u32>().expect("total must parse as u32"),
            "desired_count": row[5].parse::<u32>().expect("desired_count must parse as u32"),
        })
    })
    .collect()
}
