//! BDD step definitions for the `measure_stars` MCP tool.
//!
//! Shared steps live in `tool_steps.rs` (`capture`, `lists available
//! tools`, `the tool call should return an error`, `the error message
//! should contain ...`). The exposure-document fetch step
//! (`I fetch the exposure document for the captured document_id`) and the
//! generic section-presence assertions live in `measure_basic_steps.rs`
//! and are reused here -- all four imaging tools persist into the same
//! exposure document, so the document-fetch plumbing is shared.

use cucumber::gherkin::Step;
use cucumber::{then, when};
use serde_json::Value;

use crate::steps::tool_steps::ensure_mcp_client;
use crate::world::RpWorld;

/// Area bounds for the simulator's frames, the same sizing as
/// `detect_stars` / `measure_basic`.
const SIM_AREAS: (Option<u64>, Option<u64>) = (Some(5), Some(65_536));

// --- When steps ---

#[when("the MCP client calls \"measure_stars\" with the captured image path")]
async fn mcp_call_with_last_path(world: &mut RpWorld) {
    let image_path = world
        .last_image_path
        .clone()
        .expect("no captured image path available");

    call_tool(
        world,
        Some(&image_path),
        None,
        None,
        None,
        SIM_AREAS.0,
        SIM_AREAS.1,
    )
    .await;
}

#[when("the MCP client calls \"measure_stars\" with the captured document_id")]
async fn mcp_call_with_last_document_id(world: &mut RpWorld) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");

    call_tool(
        world,
        None,
        Some(&document_id),
        None,
        None,
        SIM_AREAS.0,
        SIM_AREAS.1,
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"measure_stars\" with the captured image path and threshold_sigma {float}"
)]
async fn mcp_call_with_threshold(world: &mut RpWorld, threshold_sigma: f64) {
    let image_path = world
        .last_image_path
        .clone()
        .expect("no captured image path available");

    call_tool(
        world,
        Some(&image_path),
        None,
        Some(threshold_sigma),
        None,
        SIM_AREAS.0,
        SIM_AREAS.1,
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"measure_stars\" with the captured image path and stamp_half_size {int}"
)]
async fn mcp_call_with_stamp(world: &mut RpWorld, stamp_half_size: i64) {
    let image_path = world
        .last_image_path
        .clone()
        .expect("no captured image path available");

    call_tool(
        world,
        Some(&image_path),
        None,
        None,
        Some(stamp_half_size),
        SIM_AREAS.0,
        SIM_AREAS.1,
    )
    .await;
}

#[when(expr = "the MCP client calls \"measure_stars\" with image path {string}")]
async fn mcp_call_with_path(world: &mut RpWorld, image_path: String) {
    call_tool(
        world,
        Some(&image_path),
        None,
        None,
        None,
        SIM_AREAS.0,
        SIM_AREAS.1,
    )
    .await;
}

#[when(expr = "the MCP client calls \"measure_stars\" with document_id {string}")]
async fn mcp_call_with_document_id(world: &mut RpWorld, document_id: String) {
    call_tool(
        world,
        None,
        Some(&document_id),
        None,
        None,
        SIM_AREAS.0,
        SIM_AREAS.1,
    )
    .await;
}

#[when("the MCP client calls \"measure_stars\" with no arguments")]
async fn mcp_call_no_args(world: &mut RpWorld) {
    ensure_mcp_client(world).await;
    let result = world
        .mcp()
        .call_tool("measure_stars", serde_json::json!({}))
        .await;

    record_result(world, result);
}

#[when(
    expr = "the MCP client calls \"measure_stars\" on the fixture {string} with threshold_sigma {float}, min_area {int} and max_area {int}"
)]
async fn mcp_call_on_fixture(
    world: &mut RpWorld,
    fixture: String,
    threshold_sigma: f64,
    min_area: u64,
    max_area: u64,
) {
    let path = fixture_path(&fixture);

    call_tool(
        world,
        Some(&path),
        None,
        Some(threshold_sigma),
        None,
        Some(min_area),
        Some(max_area),
    )
    .await;
}

#[when("the MCP client calls \"measure_stars\" with the captured image path but no min_area")]
async fn mcp_call_missing_min_area(world: &mut RpWorld) {
    let image_path = world
        .last_image_path
        .clone()
        .expect("no captured image path available");

    call_tool(
        world,
        Some(&image_path),
        None,
        None,
        None,
        None,
        SIM_AREAS.1,
    )
    .await;
}

#[when(
    expr = "the MCP client calls \"measure_stars\" on the fixture {string} with max_area {int} and the stars that call returned"
)]
async fn mcp_call_with_returned_stars(world: &mut RpWorld, fixture: String, max_area: u64) {
    let stars = result_or_panic(world)["stars"].clone();
    call_with_stars(world, &fixture_path(&fixture), Some(max_area), stars).await;
}

#[when(
    expr = "the MCP client calls \"measure_stars\" on the fixture {string} with max_area {int} and the stars:"
)]
async fn mcp_call_with_listed_stars(
    world: &mut RpWorld,
    fixture: String,
    max_area: u64,
    step: &Step,
) {
    let stars = stars_from_table(step);
    call_with_stars(world, &fixture_path(&fixture), Some(max_area), stars).await;
}

#[when(
    expr = "the MCP client calls \"measure_stars\" on the fixture {string} with no max_area and the stars:"
)]
async fn mcp_call_with_listed_stars_no_max_area(world: &mut RpWorld, fixture: String, step: &Step) {
    let stars = stars_from_table(step);
    call_with_stars(world, &fixture_path(&fixture), None, stars).await;
}

#[when(
    expr = "the MCP client calls \"measure_stars\" on the fixture {string} with max_area {int} and no stars"
)]
async fn mcp_call_with_no_stars(world: &mut RpWorld, fixture: String, max_area: u64) {
    call_with_stars(
        world,
        &fixture_path(&fixture),
        Some(max_area),
        serde_json::json!([]),
    )
    .await;
}

// --- Then steps ---

#[then(expr = "the measure_stars result should contain {string}")]
fn result_contains_field(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    assert!(
        result.get(&field).is_some(),
        "expected '{field}' in measure_stars result, got: {result:?}"
    );
}

#[then(expr = "the measure_stars result should contain {string} as a non-negative integer")]
fn result_contains_non_negative_integer(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in measure_stars result, got: {result:?}"));

    assert!(
        value.as_u64().is_some() || value.as_i64().is_some_and(|v| v >= 0),
        "expected '{field}' to be a non-negative integer, got: {value:?}"
    );
}

#[then(expr = "the measure_stars result should contain {string} as a non-negative number")]
fn result_contains_non_negative_number(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in measure_stars result, got: {result:?}"));

    let num = value
        .as_f64()
        .unwrap_or_else(|| panic!("expected '{field}' to be a number, got: {value:?}"));

    assert!(
        num >= 0.0,
        "expected '{field}' to be non-negative, got: {num}"
    );
}

#[then(expr = "the measure_stars result should contain {string} as an array")]
fn result_contains_array(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in measure_stars result, got: {result:?}"));

    assert!(
        value.is_array(),
        "expected '{field}' to be an array, got: {value:?}"
    );
}

#[then(expr = "the measure_stars result should contain {string} as an empty array")]
fn result_contains_empty_array(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in measure_stars result, got: {result:?}"));

    let arr = value
        .as_array()
        .unwrap_or_else(|| panic!("expected '{field}' to be an array, got: {value:?}"));

    assert!(
        arr.is_empty(),
        "expected '{}' to be empty, got {} entries",
        field,
        arr.len()
    );
}

#[then(expr = "the measure_stars result should contain {string} with value null")]
fn result_field_is_null(world: &mut RpWorld, field: String) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in measure_stars result, got: {result:?}"));

    assert!(
        value.is_null(),
        "expected '{field}' to be null, got: {value:?}"
    );
}

#[then(expr = "the measure_stars result should contain {string} with value {int}")]
fn result_field_equals_int(world: &mut RpWorld, field: String, expected: i64) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in measure_stars result, got: {result:?}"));

    let actual = value
        .as_i64()
        .or_else(|| value.as_u64().map(u64::cast_signed))
        .unwrap_or_else(|| panic!("expected '{field}' to be an integer, got: {value:?}"));

    assert_eq!(
        actual, expected,
        "expected '{field}' to equal {expected}, got: {actual}"
    );
}

#[then(expr = "the measure_stars result should contain {string} between {float} and {float}")]
fn result_field_between(world: &mut RpWorld, field: String, low: f64, high: f64) {
    let result = result_or_panic(world);
    let value = result
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in measure_stars result, got: {result:?}"));

    let num = value
        .as_f64()
        .unwrap_or_else(|| panic!("expected '{field}' to be a number, got: {value:?}"));

    assert!(
        (low..=high).contains(&num),
        "expected '{field}' between {low} and {high}, got: {num}"
    );
}

#[then("the measure_stars stars should be listed brightest first")]
fn stars_listed_brightest_first(world: &mut RpWorld) {
    let result = result_or_panic(world);
    let fluxes: Vec<f64> = result["stars"]
        .as_array()
        .unwrap_or_else(|| panic!("expected 'stars' to be an array, got: {result:?}"))
        .iter()
        .map(|star| {
            star["flux"]
                .as_f64()
                .unwrap_or_else(|| panic!("expected a numeric 'flux', got: {star:?}"))
        })
        .collect();

    assert!(
        fluxes.len() > 1,
        "expected more than one star to order, got: {fluxes:?}"
    );
    assert!(
        fluxes.windows(2).all(|pair| pair[0] >= pair[1]),
        "expected stars in descending flux order, got: {fluxes:?}"
    );
}

#[then(
    expr = "every measure_stars star should lie within {float} px of the star listed in its place"
)]
fn stars_lie_near_their_listed_places(world: &mut RpWorld, tolerance: f64) {
    let listed = listed_or_panic(world);
    let stars = stars_or_panic(world);
    assert_eq!(
        stars.len(),
        listed.len(),
        "expected one star per listed star, got: {stars:?}"
    );
    for (index, (star, given)) in stars.iter().zip(listed).enumerate() {
        let distance =
            (number(star, "x") - number(given, "x")).hypot(number(star, "y") - number(given, "y"));
        assert!(
            distance <= tolerance,
            "star {index} came back {distance:.3} px from where it was listed: {star:?} vs {given:?}"
        );
    }
}

#[then(
    expr = "every measured measure_stars star should have a radius of at least {float} and an snr above {float}"
)]
fn measured_stars_carry_radius_and_snr(world: &mut RpWorld, min_radius: f64, min_snr: f64) {
    let stars = stars_or_panic(world);
    let measured: Vec<&Value> = stars.iter().filter(|s| !s["hfr"].is_null()).collect();
    assert!(
        !measured.is_empty(),
        "expected measured stars, got: {stars:?}"
    );
    for star in measured {
        assert!(
            number(star, "radius") >= min_radius,
            "expected a radius of at least {min_radius}: {star:?}"
        );
        assert!(
            number(star, "snr") > min_snr,
            "expected an snr above {min_snr}: {star:?}"
        );
    }
}

#[then("the measure_stars stars should be, in order, unmeasured at:")]
fn stars_unmeasured_at(world: &mut RpWorld, step: &Step) {
    let expected = stars_from_table(step);
    let expected = expected.as_array().expect("the table is a list of stars");
    let stars = stars_or_panic(world);
    assert_eq!(
        stars.len(),
        expected.len(),
        "expected {} stars, got: {stars:?}",
        expected.len()
    );
    for (index, (star, place)) in stars.iter().zip(expected).enumerate() {
        for field in ["x", "y", "radius"] {
            assert!(
                (number(star, field) - number(place, field)).abs() < 1e-9,
                "star {index} should keep its {field} as listed: {star:?} vs {place:?}"
            );
        }
        for field in ["hfr", "fwhm", "eccentricity", "flux", "snr"] {
            assert!(
                star[field].is_null(),
                "star {index} should have a null {field}: {star:?}"
            );
        }
    }
}

#[then(expr = "exactly {int} of the {int} measure_stars stars should be measured")]
fn exactly_n_stars_measured(world: &mut RpWorld, measured: usize, total: usize) {
    let stars = stars_or_panic(world);
    assert_eq!(stars.len(), total, "expected {total} stars, got: {stars:?}");
    let count = stars.iter().filter(|s| !s["hfr"].is_null()).count();
    assert_eq!(
        count, measured,
        "expected {measured} measured, got: {stars:?}"
    );
}

// --- Helpers ---

/// The absolute path of a fixture under `tests/fixtures`. The suite runs
/// from the package directory (cargo's default, and `BDD_PACKAGE_DIR`
/// under Bazel); rp gets an absolute path because it runs as its own
/// process.
fn fixture_path(fixture: &str) -> String {
    let path = std::env::current_dir()
        .expect("current directory")
        .join("tests/fixtures")
        .join(fixture);
    assert!(path.is_file(), "fixture {} not found", path.display());
    path.to_str().expect("fixture path is UTF-8").to_string()
}

/// A `stars` array from a table with the header `x | y | radius`.
fn stars_from_table(step: &Step) -> Value {
    let table = step.table.as_ref().expect("the step needs a data table");
    let header = &table.rows[0];
    let column = |name: &str| {
        header
            .iter()
            .position(|h| h == name)
            .unwrap_or_else(|| panic!("the table has no '{name}' column"))
    };
    let (x, y, radius) = (column("x"), column("y"), column("radius"));
    let stars: Vec<Value> = table.rows[1..]
        .iter()
        .map(|row| {
            let cell = |i: usize| -> f64 { row[i].parse().expect("a number") };
            serde_json::json!({ "x": cell(x), "y": cell(y), "radius": cell(radius) })
        })
        .collect();
    Value::Array(stars)
}

/// `measure_stars` on `image_path` with `stars`, at the default
/// threshold, remembering the stars passed.
async fn call_with_stars(
    world: &mut RpWorld,
    image_path: &str,
    max_area: Option<u64>,
    stars: Value,
) {
    ensure_mcp_client(world).await;
    let mut args = serde_json::Map::new();
    args.insert(
        "image_path".to_string(),
        Value::String(image_path.to_string()),
    );
    if let Some(max_area) = max_area {
        args.insert("max_area".to_string(), serde_json::json!(max_area));
    }
    args.insert("stars".to_string(), stars.clone());
    world.last_listed_stars = Some(stars);
    let result = world
        .mcp()
        .call_tool("measure_stars", Value::Object(args))
        .await;
    record_result(world, result);
}

fn stars_or_panic(world: &RpWorld) -> &Vec<Value> {
    let result = result_or_panic(world);
    result["stars"]
        .as_array()
        .unwrap_or_else(|| panic!("expected 'stars' to be an array, got: {result:?}"))
}

fn listed_or_panic(world: &RpWorld) -> &Vec<Value> {
    world
        .last_listed_stars
        .as_ref()
        .and_then(Value::as_array)
        .expect("no listed stars passed")
}

fn number(value: &Value, field: &str) -> f64 {
    value[field]
        .as_f64()
        .unwrap_or_else(|| panic!("expected a numeric '{field}', got: {value:?}"))
}

#[allow(clippy::too_many_arguments)]
async fn call_tool(
    world: &mut RpWorld,
    image_path: Option<&str>,
    document_id: Option<&str>,
    threshold_sigma: Option<f64>,
    stamp_half_size: Option<i64>,
    min_area: Option<u64>,
    max_area: Option<u64>,
) {
    ensure_mcp_client(world).await;

    let mut args = serde_json::Map::new();
    if let Some(path) = image_path {
        args.insert("image_path".to_string(), Value::String(path.to_string()));
    }
    if let Some(doc_id) = document_id {
        args.insert("document_id".to_string(), Value::String(doc_id.to_string()));
    }
    if let Some(threshold) = threshold_sigma {
        args.insert("threshold_sigma".to_string(), serde_json::json!(threshold));
    }
    if let Some(stamp) = stamp_half_size {
        args.insert("stamp_half_size".to_string(), serde_json::json!(stamp));
    }
    if let Some(min_area) = min_area {
        args.insert("min_area".to_string(), serde_json::json!(min_area));
    }
    if let Some(max_area) = max_area {
        args.insert("max_area".to_string(), serde_json::json!(max_area));
    }

    let result = world
        .mcp()
        .call_tool("measure_stars", Value::Object(args))
        .await;

    record_result(world, result);
}

fn record_result(world: &mut RpWorld, result: Result<Value, String>) {
    match &result {
        Ok(v) => world.last_measure_stars_result = Some(v.clone()),
        Err(_) => world.last_measure_stars_result = None,
    }
    world.last_tool_result = Some(result);
}

const fn result_or_panic(world: &RpWorld) -> &Value {
    world
        .last_measure_stars_result
        .as_ref()
        .expect("no measure_stars result")
}
