//! BDD step definitions for target acquisition goals and filter-roster
//! validation (`target_store_goals.feature`, rp.md § Target Store —
//! *(planned, P1)*, not yet implemented; scenarios are tagged `@wip`).

use cucumber::gherkin::Step;
use cucumber::{given, then, when};
use serde_json::Value;

use crate::steps::target_store_crud_steps::{start_target_store_rp, write_target_store_config};
use crate::steps::tool_steps::ensure_mcp_client;
use crate::world::RpWorld;

fn names(list: &str) -> Vec<String> {
    list.split(',').map(|s| s.trim().to_string()).collect()
}

/// Parses a `| filter | binning | exposure_duration | desired_count |` Gherkin
/// table into the JSON shape `add_target`/`set_goals` accept for their
/// `goals[]` parameter. `pub(crate)` so `target_store_planner_steps.rs`
/// reuses it for the planner-feature always-visible-with-goals seeds.
pub fn goals_from_table(step: &Step) -> Vec<Value> {
    let table = step
        .table
        .as_ref()
        .expect("step requires a `| filter | binning | exposure_duration | desired_count |` table");
    let mut rows = table.rows.iter();
    let header = rows.next().expect("goals table must have a header");
    assert_eq!(
        header.as_slice(),
        ["filter", "binning", "exposure_duration", "desired_count"],
        "goals table header"
    );
    rows.map(|row| {
        serde_json::json!({
            "filter": row[0],
            "binning": row[1],
            "exposure_duration": row[2],
            "desired_count": row[3].parse::<u32>().expect("desired_count must parse as u32"),
        })
    })
    .collect()
}

// ---------------------------------------------------------------------------
// Given
// ---------------------------------------------------------------------------

/// Two wheels and an imaging train over a camera, none of them reachable:
/// the target context is read from the config, not from the devices.
#[given(
    expr = "rp is running with the filter wheels {string} and {string} and the imaging train {string} at default position angle {float}"
)]
async fn rp_with_wheels_and_train(
    world: &mut RpWorld,
    first: String,
    second: String,
    train: String,
    angle: f64,
) {
    write_target_store_config(world, Some(names(&first)));
    let path = world
        .config_rest_path
        .clone()
        .expect("the target-store config was written");
    let mut config: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read rp config"))
            .expect("parse rp config");
    let equipment = &mut config["equipment"];
    equipment["filter_wheels"]
        .as_array_mut()
        .expect("the config has a wheel")
        .push(serde_json::json!({
            "id": "side-fw",
            "alpaca_url": "http://127.0.0.1:1",
            "device_number": 1,
            "filters": names(&second)
        }));
    equipment["cameras"] = serde_json::json!([
        { "id": "main-cam", "alpaca_url": "http://127.0.0.1:1", "device_number": 0 }
    ]);
    equipment["optical_trains"] = serde_json::json!([
        { "id": train, "devices": ["main-cam"], "default_position_angle_degrees": angle }
    ]);
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&config).expect("serialize rp config"),
    )
    .expect("write rp config");
    start_target_store_rp(world).await;
}

#[given(expr = "rp is configured with default target goals:")]
fn default_target_goals(world: &mut RpWorld, step: &Step) {
    let goals = goals_from_table(step);
    let targets_block = world
        .target_store_config
        .get_or_insert_with(|| serde_json::json!({}));
    targets_block["default_goals"] = Value::Array(goals);
}

#[given(expr = "the MCP client has set its goals to:")]
async fn set_goals_fixture(world: &mut RpWorld, step: &Step) {
    let goals = goals_from_table(step);
    let slug = world
        .last_target_slug
        .clone()
        .expect("no remembered target slug — add a target fixture step first");
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "set_goals",
            serde_json::json!({ "slug": slug, "goals": goals }),
        )
        .await;
    result.unwrap_or_else(|e| panic!("fixture set_goals failed: {e}"));
}

// ---------------------------------------------------------------------------
// When
// ---------------------------------------------------------------------------

#[when(expr = "the MCP client calls \"set_goals\" for slug {string} with goals:")]
async fn call_set_goals(world: &mut RpWorld, slug: String, step: &Step) {
    let goals = goals_from_table(step);
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "set_goals",
            serde_json::json!({ "slug": slug, "goals": goals }),
        )
        .await;
    world.last_tool_result = Some(result);
}

#[when(
    expr = "the MCP client calls \"add_target\" with display_name {string} ra_hours {float} dec_degrees {float} and goals:"
)]
async fn call_add_target_with_goals(
    world: &mut RpWorld,
    display_name: String,
    ra_hours: f64,
    dec_degrees: f64,
    step: &Step,
) {
    let goals = goals_from_table(step);
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool(
            "add_target",
            serde_json::json!({
                "display_name": display_name,
                "ra_hours": ra_hours,
                "dec_degrees": dec_degrees,
                "goals": goals
            }),
        )
        .await;
    if let Ok(ref v) = result {
        world.last_target_slug = v.get("slug").and_then(|s| s.as_str()).map(String::from);
    }
    world.last_tool_result = Some(result);
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

#[when("the MCP client calls \"get_target_context\"")]
async fn call_get_target_context(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("get_target_context", serde_json::json!({}))
        .await;
    world.last_tool_result = Some(result);
}

fn target_context(world: &RpWorld) -> &Value {
    world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("expected tool call to succeed")
}

#[then(expr = "the target context names the filters {string}")]
fn target_context_names_filters(world: &mut RpWorld, list: String) {
    let context = target_context(world);
    assert_eq!(
        context["filters"],
        serde_json::json!(names(&list)),
        "{context}"
    );
}

#[then(expr = "the target context gives the train {string} the default position angle {float}")]
fn target_context_train_angle(world: &mut RpWorld, train: String, angle: f64) {
    let context = target_context(world);
    let entry = context["trains"]
        .as_array()
        .and_then(|trains| trains.iter().find(|t| t["train_id"] == train.as_str()))
        .unwrap_or_else(|| panic!("no train {train} in {context}"));
    assert_eq!(
        entry["default_position_angle_degrees"],
        serde_json::json!(angle),
        "{context}"
    );
}

#[then(expr = "the fetched target should have exactly these goals:")]
fn fetched_target_has_goals(world: &mut RpWorld, step: &Step) {
    let expected = goals_from_table(step);
    let payload = world
        .last_tool_result
        .as_ref()
        .expect("no tool result")
        .as_ref()
        .expect("expected tool call to succeed");
    let target = payload
        .get("target")
        .unwrap_or_else(|| panic!("tool result missing `target`: {payload}"));
    let actual = target
        .get("goals")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("target payload missing `goals` array: {target}"));
    assert_eq!(actual, &expected, "target goals");
}

// "the tool error message should mention {string}" is registered
// globally in ephemeris_steps.rs. Reused here, not redefined.
