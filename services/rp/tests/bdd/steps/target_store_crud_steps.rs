//! BDD step definitions for the target store's CRUD MCP tools
//! (`target_store_crud.feature`, rp.md § Target Store — *(planned, P1)*,
//! not yet implemented; scenarios are tagged `@wip`).
//!
//! These scenarios never touch `OmniSim`: `add_target`/`get_target`/
//! `list_targets`/`update_target`/`delete_target` are pure plan-data
//! operations, so rp is spawned directly from a scenario-private temp
//! config on port 0 (the `setup_pages.feature` pattern), skipping the
//! simulator entirely. `write_target_store_config` and
//! `start_target_store_rp` are `pub(crate)` and reused by the sibling
//! `target_store_goals_steps.rs` and `target_store_progress_steps.rs`.

use cucumber::{given, then, when};
use serde_json::Value;

use bdd_infra::ServiceHandle;

use crate::steps::tool_steps::ensure_mcp_client;
use crate::world::RpWorld;

// ---------------------------------------------------------------------------
// Shared config/launch helpers (reused by goals/progress/rest step files)
// ---------------------------------------------------------------------------

/// Write a scenario-private rp config for target-store scenarios: no
/// `OmniSim`, port 0, an optional filter-wheel roster (declared but never
/// connected — goal filter-roster validation reads the configured
/// `filters[]` list, not live device state, per rp.md § Target Store).
/// Picks up `world.target_store_config` (set by a `Given rp is
/// configured with default target goals:`-style step) as the
/// `target_store` config block, when present — mirrors how `RpWorld::build_config`
/// applies the same field for the OmniSim-backed bootstrap path, so a
/// scenario can set the override before either "rp is running ..."
/// Given step.
pub fn write_target_store_config(world: &mut RpWorld, filters: Option<Vec<String>>) {
    let dir = tempfile::tempdir().expect("create temp dir for rp config");
    let mut equipment = serde_json::json!({});
    if let Some(filters) = filters {
        equipment["filter_wheels"] = serde_json::json!([{
            "id": "main-fw",
            // Unreachable on purpose — these scenarios never connect the
            // wheel; only the configured filter roster matters.
            "alpaca_url": "http://127.0.0.1:1",
            "device_number": 0,
            "filters": filters
        }]);
    }
    let mut config = serde_json::json!({
        "session": { "data_directory": dir.path().join("data").to_string_lossy() },
        "equipment": equipment,
        "server": { "port": 0, "bind_address": "127.0.0.1" }
    });
    if let Some(target_store) = &world.target_store_config {
        config["target_store"] = target_store.clone();
    }
    // Progress scenarios need the naming templates configured — they are
    // what the frame scan parses the on-disk layout with (rp.md §
    // Progress derivation). `directory_pattern` is left unset so the
    // documented default applies, exactly as in production.
    if let Some(pattern) = &world.file_naming_pattern {
        config["session"]["file_naming_pattern"] = Value::String(pattern.clone());
    }
    if let Some(pattern) = &world.directory_pattern {
        config["session"]["directory_pattern"] = Value::String(pattern.clone());
    }
    let path = dir.path().join("rp.json");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&config).expect("serialize rp config"),
    )
    .expect("write rp config file");
    world.config_rest_path = Some(path);
    world.config_rest_dir = Some(dir);
}

pub async fn start_target_store_rp(world: &mut RpWorld) {
    let path = world
        .config_rest_path
        .clone()
        .expect("no target-store config written — call write_target_store_config first");
    world.rp = Some(
        ServiceHandle::start(env!("CARGO_PKG_NAME"), path.to_str().expect("utf-8 path")).await,
    );
    assert!(
        world.wait_for_rp_healthy().await,
        "rp did not become healthy within timeout"
    );
}

/// Call `add_target`, panicking with the tool error on failure (a Given
/// fixture step — testing.md §3.3: fail fast on setup problems).
/// Remembers the returned slug on `world.last_target_slug`.
pub async fn add_target_fixture(world: &mut RpWorld, args: Value) {
    ensure_mcp_client(world).await;
    let result = world.mcp().call_tool("add_target", args).await;
    let value = result
        .as_ref()
        .unwrap_or_else(|e| panic!("fixture add_target failed: {e}"));
    world.last_target_slug = value.get("slug").and_then(|v| v.as_str()).map(String::from);
    world.last_tool_result = Some(result);
}

// ---------------------------------------------------------------------------
// Given
// ---------------------------------------------------------------------------

#[given("rp is running with a target store")]
async fn rp_running_with_target_store(world: &mut RpWorld) {
    write_target_store_config(world, None);
    start_target_store_rp(world).await;
}

#[given(expr = "rp is running with a target store and filter roster {string}")]
async fn rp_running_with_target_store_and_roster(world: &mut RpWorld, roster: String) {
    let filters: Vec<String> = roster.split(',').map(|s| s.trim().to_string()).collect();
    write_target_store_config(world, Some(filters));
    start_target_store_rp(world).await;
}

#[given(
    expr = "the MCP client has added a target named {string} at ra_hours {float} dec_degrees {float}"
)]
async fn added_target(world: &mut RpWorld, display_name: String, ra_hours: f64, dec_degrees: f64) {
    add_target_fixture(
        world,
        serde_json::json!({
            "display_name": display_name,
            "ra_hours": ra_hours,
            "dec_degrees": dec_degrees
        }),
    )
    .await;
}

#[given(
    expr = "the MCP client has added an inactive target named {string} at ra_hours {float} dec_degrees {float}"
)]
async fn added_inactive_target(
    world: &mut RpWorld,
    display_name: String,
    ra_hours: f64,
    dec_degrees: f64,
) {
    add_target_fixture(
        world,
        serde_json::json!({
            "display_name": display_name,
            "ra_hours": ra_hours,
            "dec_degrees": dec_degrees,
            "active": false
        }),
    )
    .await;
}

#[given(expr = "the MCP client has added catalog target {string}")]
async fn added_catalog_target(world: &mut RpWorld, catalog_ref: String) {
    add_target_fixture(world, serde_json::json!({ "catalog_ref": catalog_ref })).await;
}

/// A framed (offset) import of a catalog object — coordinates supplied
/// explicitly rather than resolved, standing in for a P3-bridge-style
/// precise framing that differs from the catalog centroid (Decision 3's
/// same-`catalog_ref`-but-different-coordinates protection).
#[given(
    expr = "the MCP client has added a target with catalog_ref {string} ra_hours {float} dec_degrees {float}"
)]
async fn added_framed_catalog_target(
    world: &mut RpWorld,
    catalog_ref: String,
    ra_hours: f64,
    dec_degrees: f64,
) {
    add_target_fixture(
        world,
        serde_json::json!({
            "catalog_ref": catalog_ref,
            "ra_hours": ra_hours,
            "dec_degrees": dec_degrees
        }),
    )
    .await;
}

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

#[when(
    expr = "the MCP client calls \"add_target\" with display_name {string} ra_hours {float} dec_degrees {float}"
)]
async fn call_add_target(
    world: &mut RpWorld,
    display_name: String,
    ra_hours: f64,
    dec_degrees: f64,
) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "add_target",
            serde_json::json!({
                "display_name": display_name,
                "ra_hours": ra_hours,
                "dec_degrees": dec_degrees
            }),
        )
        .await;
    remember_slug_and_result(world, result);
}

#[when(expr = "the MCP client calls \"add_target\" with catalog_ref {string}")]
async fn call_add_target_catalog(world: &mut RpWorld, catalog_ref: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "add_target",
            serde_json::json!({ "catalog_ref": catalog_ref }),
        )
        .await;
    remember_slug_and_result(world, result);
}

#[when(expr = "the MCP client calls \"add_target\" with catalog_ref {string} and notes {string}")]
async fn call_add_target_catalog_with_notes(
    world: &mut RpWorld,
    catalog_ref: String,
    notes: String,
) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "add_target",
            serde_json::json!({ "catalog_ref": catalog_ref, "notes": notes }),
        )
        .await;
    remember_slug_and_result(world, result);
}

#[when(expr = "the MCP client calls \"get_target\" for slug {string}")]
async fn call_get_target(world: &mut RpWorld, slug: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("get_target", serde_json::json!({ "slug": slug }))
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client fetches the target it just added")]
async fn call_get_last_added_target(world: &mut RpWorld) {
    let slug = world
        .last_target_slug
        .clone()
        .expect("no remembered target slug — add a target fixture step first");
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("get_target", serde_json::json!({ "slug": slug }))
        .await;
    world.last_tool_result = Some(result);
}

#[when("the MCP client calls \"list_targets\"")]
async fn call_list_targets(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("list_targets", serde_json::json!({}))
        .await;
    record_target_list(world, result);
}

#[when(expr = "the MCP client calls \"list_targets\" with active_only {word}")]
async fn call_list_targets_active_only(world: &mut RpWorld, active_only: String) {
    ensure_mcp_client(world).await;
    let active_only: bool = active_only
        .parse()
        .expect("active_only must be true or false");
    let result = world
        .mcp()
        .call_tool(
            "list_targets",
            serde_json::json!({ "active_only": active_only }),
        )
        .await;
    record_target_list(world, result);
}

#[when(
    expr = "the MCP client calls \"update_target\" for slug {string} setting display_name {string}"
)]
async fn call_update_target_display_name(world: &mut RpWorld, slug: String, display_name: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "update_target",
            serde_json::json!({ "slug": slug, "display_name": display_name }),
        )
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"update_target\" for slug {string} setting active {word}")]
async fn call_update_target_active(world: &mut RpWorld, slug: String, active: String) {
    ensure_mcp_client(world).await;
    let active: bool = active.parse().expect("active must be true or false");
    let result = world
        .mcp()
        .call_tool(
            "update_target",
            serde_json::json!({ "slug": slug, "active": active }),
        )
        .await;
    world.last_tool_result = Some(result);
}

#[when(
    expr = "the MCP client calls \"add_target\" with display_name {string} ra_hours {float} dec_degrees {float} and position angle {float}"
)]
async fn call_add_target_with_angle(
    world: &mut RpWorld,
    display_name: String,
    ra_hours: f64,
    dec_degrees: f64,
    angle: f64,
) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "add_target",
            serde_json::json!({
                "display_name": display_name,
                "ra_hours": ra_hours,
                "dec_degrees": dec_degrees,
                "position_angle_degrees": angle
            }),
        )
        .await;
    remember_slug_and_result(world, result);
}

#[when(
    expr = "the MCP client calls \"update_target\" for slug {string} setting position angle {float}"
)]
async fn call_update_target_position_angle(world: &mut RpWorld, slug: String, angle: f64) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "update_target",
            serde_json::json!({ "slug": slug, "position_angle_degrees": angle }),
        )
        .await;
    world.last_tool_result = Some(result);
}

// The explicit-null clearing form (rp.md § Target Store → Position
// angle): `position_angle_degrees: null` is "back to inherit", distinct
// from omitting the field (leave untouched).
#[when(
    expr = "the MCP client calls \"update_target\" for slug {string} clearing the position angle"
)]
async fn call_update_target_clear_position_angle(world: &mut RpWorld, slug: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "update_target",
            serde_json::json!({ "slug": slug, "position_angle_degrees": null }),
        )
        .await;
    world.last_tool_result = Some(result);
}

#[when(expr = "the MCP client calls \"delete_target\" for slug {string}")]
async fn call_delete_target(world: &mut RpWorld, slug: String) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("delete_target", serde_json::json!({ "slug": slug }))
        .await;
    world.last_tool_result = Some(result);
}

fn remember_slug_and_result(world: &mut RpWorld, result: Result<Value, String>) {
    if let Ok(ref v) = result {
        world.last_target_slug = v.get("slug").and_then(|s| s.as_str()).map(String::from);
    }
    world.last_tool_result = Some(result);
}

fn record_target_list(world: &mut RpWorld, result: Result<Value, String>) {
    if let Ok(ref v) = result {
        world.last_target_list = v.get("targets").and_then(|t| t.as_array()).cloned();
    }
    world.last_tool_result = Some(result);
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

// "the tool call should succeed" is registered globally in
// cover_calibrator_steps.rs; "the tool call should fail" in
// catalog_steps.rs. Reused here, not redefined.

#[then("the target result should be created")]
fn target_result_created(world: &mut RpWorld) {
    let created = target_result(world)
        .get("created")
        .and_then(serde_json::Value::as_bool)
        .expect("add_target result missing `created`");
    assert!(
        created,
        "expected a newly created target, got an in-place update"
    );
}

#[then("the target result should be an in-place update")]
fn target_result_updated(world: &mut RpWorld) {
    let created = target_result(world)
        .get("created")
        .and_then(serde_json::Value::as_bool)
        .expect("add_target result missing `created`");
    assert!(
        !created,
        "expected an in-place update, got a newly created target"
    );
}

#[then(expr = "the target slug should be {string}")]
fn target_slug_should_be(world: &mut RpWorld, expected: String) {
    let slug = target_result(world)
        .get("slug")
        .and_then(|v| v.as_str())
        .expect("tool result missing `slug`");
    assert_eq!(slug, expected.as_str(), "target slug");
}

#[then(expr = "the target result deleted should be {word}")]
fn target_result_deleted(world: &mut RpWorld, expected: String) {
    let expected: bool = expected.parse().expect("expected must be true or false");
    let deleted = target_result(world)
        .get("deleted")
        .and_then(serde_json::Value::as_bool)
        .expect("delete_target result missing `deleted`");
    assert_eq!(deleted, expected, "delete_target `deleted` field");
}

#[then(expr = "the fetched target should have display_name {string}")]
fn fetched_target_display_name(world: &mut RpWorld, expected: String) {
    let name = target_field(world, "display_name");
    assert_eq!(
        name.as_str(),
        Some(expected.as_str()),
        "target display_name"
    );
}

#[then("the fetched target should be active")]
fn fetched_target_active(world: &mut RpWorld) {
    let active = target_field(world, "active");
    assert_eq!(active.as_bool(), Some(true), "expected target to be active");
}

#[then(expr = "the fetched target position angle should be {float}")]
fn fetched_target_position_angle(world: &mut RpWorld, expected: f64) {
    let angle = target_field(world, "position_angle_degrees")
        .as_f64()
        .expect("position_angle_degrees is not a number");
    assert!(
        (angle - expected).abs() < f64::EPSILON,
        "expected position_angle_degrees={expected}, got {angle}"
    );
}

#[then("the fetched target position angle should be null")]
fn fetched_target_position_angle_null(world: &mut RpWorld) {
    let angle = target_field(world, "position_angle_degrees");
    assert!(
        angle.is_null(),
        "expected position_angle_degrees=null, got {angle}"
    );
}

#[then(expr = "the fetched target slug should still be {string}")]
fn fetched_target_slug_unchanged(world: &mut RpWorld, expected: String) {
    let slug = target_field(world, "slug");
    assert_eq!(slug.as_str(), Some(expected.as_str()), "target slug");
}

#[then(expr = "list_targets should report exactly {int} target(s)")]
fn list_targets_count(world: &mut RpWorld, expected: usize) {
    let list = world
        .last_target_list
        .as_ref()
        .expect("no target list — call list_targets first");
    assert_eq!(list.len(), expected, "list_targets count, got: {list:?}");
}

#[then(expr = "the target list should contain exactly {string}")]
fn target_list_contains_exactly(world: &mut RpWorld, expected_slug: String) {
    let list = world
        .last_target_list
        .as_ref()
        .expect("no target list — call list_targets first");
    let slugs: Vec<&str> = list
        .iter()
        .filter_map(|t| t.get("slug").and_then(|s| s.as_str()))
        .collect();
    assert_eq!(slugs, vec![expected_slug.as_str()], "list_targets slugs");
}

// --- Helpers ---

/// The last tool result's payload, unwrapped — used by assertions that
/// check the direct return value of `add_target`/`delete_target`/etc.
fn target_result(world: &RpWorld) -> &Value {
    world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("expected tool call to succeed")
}

/// Reads a field off the nested `target` object every target-returning
/// tool result carries (`add_target`/`get_target`/`update_target`/
/// `set_goals` all return a top-level `target` field per rp.md §
/// Target Store → Target MCP tools).
fn target_field<'a>(world: &'a RpWorld, field: &str) -> &'a Value {
    let payload = target_result(world);
    let target = payload
        .get("target")
        .unwrap_or_else(|| panic!("tool result missing `target`: {payload}"));
    target
        .get(field)
        .unwrap_or_else(|| panic!("target payload missing field {field:?}: {payload}"))
}
