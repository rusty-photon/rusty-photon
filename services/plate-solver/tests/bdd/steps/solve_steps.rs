//! Step definitions for `solve_request.feature` and shared HTTP /
//! response-assertion phrases reused by other feature files.
//!
//! Wrapper-startup is lazy: the "Given the wrapper is running…" step
//! marks intent only; the real spawn happens on first HTTP request,
//! by which time per-scenario `mock_astap_mode` / `argv_out_path` /
//! `astap_extra_env` state has been accumulated.

use crate::world::{HttpResponse, PlateSolverWorld};
use cucumber::gherkin::Step;
use cucumber::{given, then, when};
use std::time::Instant;

// ----- shared "Given the wrapper is running" used across features -----

#[given("the wrapper is running with mock_astap as its solver")]
async fn given_wrapper_running_with_mock(_world: &mut PlateSolverWorld) {
    // Lazy: real spawn happens on first HTTP request.
}

// ----- mock_astap mode setup -----

#[given(expr = "mock_astap is configured for {string} mode")]
async fn given_mock_astap_mode(world: &mut PlateSolverWorld, mode: String) {
    world.mock_astap_mode = Some(mode);
}

#[given("mock_astap is configured to write argv to a side-channel file")]
async fn given_mock_astap_argv_out(world: &mut PlateSolverWorld) {
    let dir = world.temp_dir_path();
    let p = dir.join("argv_out.txt");
    world.argv_out_path = Some(p);
}

#[given("a writable FITS path")]
async fn given_writable_fits_path(world: &mut PlateSolverWorld) {
    let dir = world.temp_dir_path();
    let p = dir.join("test.fits");
    std::fs::write(&p, b"placeholder").expect("touch FITS");
    world.fits_path = Some(p);
}

#[given("a non-existent FITS path")]
async fn given_non_existent_fits_path(world: &mut PlateSolverWorld) {
    let dir = world.temp_dir_path();
    let p = dir.join("does_not_exist.fits");
    // Intentionally do NOT create the file.
    world.fits_path = Some(p);
}

#[given("a fits_path pointing at a directory")]
async fn given_fits_path_is_directory(world: &mut PlateSolverWorld) {
    let dir = world.temp_dir_path();
    let p = dir.join("not_a_file.fits");
    std::fs::create_dir_all(&p).expect("mkdir fits-as-dir");
    world.fits_path = Some(p);
}

// The "unreadable FITS" scenario is gated `@unix` in the feature file.
// On Windows the simple `chmod 000` trick doesn't deny read (file ACLs
// override mode bits, and the test runner typically has override
// privileges); covering that branch reliably would need DENY ACLs or
// file locks, which isn't worth the complexity until a real Windows
// failure mode surfaces.
#[given("an unreadable FITS path")]
#[cfg(unix)]
async fn given_unreadable_fits_path(world: &mut PlateSolverWorld) {
    use std::os::unix::fs::PermissionsExt;
    let dir = world.temp_dir_path();
    let p = dir.join("unreadable.fits");
    std::fs::write(&p, b"placeholder").expect("touch FITS");
    // Mode 000: no read for owner/group/other. `File::open` returns
    // EACCES on the wrapper's open-check.
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
    world.fits_path = Some(p);
}

// ----- when: HTTP requests -----
//
// Note on the `\\/` escapes in `expr = "..."` patterns below: cucumber
// expressions interpret bare `/` as alternation (`red/blue` matches
// `red` or `blue`). A literal `/api/v1/solve` triggers a compile
// error: "An alternation can not be empty." `\\/` in the Rust
// string yields `\/` in the cucumber expression — the documented
// escape for a literal `/`. The literal-string form (no `expr =`)
// is not a cucumber expression and accepts bare `/`, which is why
// the next step works without escaping.

#[when("I POST to /api/v1/solve with that fits_path")]
async fn when_post_solve_with_fits_path(world: &mut PlateSolverWorld) {
    ensure_wrapper(world).await;
    let body = serde_json::json!({
        "fits_path": world.fits_path.as_ref().expect("fits_path"),
    });
    do_post(world, body).await;
}

#[when(expr = "I POST to \\/api\\/v1\\/solve with fits_path {string}")]
async fn when_post_solve_with_explicit_path(world: &mut PlateSolverWorld, path: String) {
    ensure_wrapper(world).await;
    let body = serde_json::json!({
        "fits_path": path,
    });
    do_post(world, body).await;
}

#[when(expr = "I POST to \\/api\\/v1\\/solve with raw body {string}")]
async fn when_post_solve_with_raw_body(world: &mut PlateSolverWorld, raw: String) {
    ensure_wrapper(world).await;
    let url = format!("{}/api/v1/solve", world.wrapper_url());
    let started = Instant::now();
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Content-Type", "application/json")
        .body(raw)
        .send()
        .await
        .expect("POST");
    record_response(world, resp, started).await;
}

#[when(expr = "I POST to \\/api\\/v1\\/solve with that fits_path and timeout {string}")]
async fn when_post_solve_with_timeout(world: &mut PlateSolverWorld, timeout: String) {
    ensure_wrapper(world).await;
    let body = serde_json::json!({
        "fits_path": world.fits_path.as_ref().expect("fits_path"),
        "timeout": timeout,
    });
    do_post(world, body).await;
}

#[when(expr = "I POST to \\/api\\/v1\\/solve with that fits_path and hint {word} set to {word}")]
async fn when_post_solve_with_hint(world: &mut PlateSolverWorld, field: String, value: String) {
    ensure_wrapper(world).await;
    let value_f: f64 = value
        .parse()
        .unwrap_or_else(|_| panic!("hint value not f64: {value}"));
    world.pending_hint = Some((field.clone(), value_f));
    let mut body = serde_json::json!({
        "fits_path": world.fits_path.as_ref().expect("fits_path"),
    });
    body[field] = serde_json::Value::from(value_f);
    do_post(world, body).await;
}

#[when("I POST to /api/v1/solve with that fits_path and these hints:")]
async fn when_post_solve_with_hint_table(world: &mut PlateSolverWorld, step: &Step) {
    ensure_wrapper(world).await;
    let table = step.table.as_ref().expect("expected a data table");
    let mut body = serde_json::json!({
        "fits_path": world.fits_path.as_ref().expect("fits_path"),
    });
    // Skip the header row (field | value).
    for row in table.rows.iter().skip(1) {
        assert_eq!(
            row.len(),
            2,
            "hint row must be `field | value`, got {row:?}"
        );
        let field = row[0].trim();
        let value = row[1].trim();
        let value_f: f64 = value
            .parse()
            .unwrap_or_else(|_| panic!("hint value not f64 in row {row:?}: {value}"));
        body[field] = serde_json::Value::from(value_f);
    }
    do_post(world, body).await;
}

// ----- then: response assertions (shared across feature files) -----

#[then(expr = "the response status is {int}")]
async fn then_response_status_is(world: &mut PlateSolverWorld, status: u16) {
    let actual = world
        .last_response
        .as_ref()
        .expect("no response captured")
        .status;
    assert_eq!(
        actual,
        status,
        "expected {status}, got {actual}; body={:?}",
        world.last_response.as_ref().unwrap().body
    );
}

#[then(expr = "the response field {string} is {string}")]
async fn then_response_field_is_string(
    world: &mut PlateSolverWorld,
    path: String,
    expected: String,
) {
    let actual = json_at(world, &path).expect("path missing");
    let actual_s = actual.as_str().expect("not a string");
    assert_eq!(
        actual_s, expected,
        "field {path}: expected {expected:?}, got {actual_s:?}"
    );
}

#[then(expr = "the response field {string} is {int}")]
async fn then_response_field_is_int(world: &mut PlateSolverWorld, path: String, expected: i64) {
    let actual = json_at(world, &path).expect("path missing");
    let actual_i = actual.as_i64().expect("not an int");
    assert_eq!(actual_i, expected);
}

#[then(expr = "the response field {string} is approximately {float}")]
async fn then_response_field_approx(world: &mut PlateSolverWorld, path: String, expected: f64) {
    let actual = json_at(world, &path).expect("path missing");
    let actual_f = actual.as_f64().expect("not a float");
    let tol = 1e-2;
    assert!(
        (actual_f - expected).abs() < tol,
        "field {path}: expected ~{expected}, got {actual_f}"
    );
}

#[then(expr = "the response field {string} is approximately {float} within {float} degrees")]
async fn then_response_field_approx_within(
    world: &mut PlateSolverWorld,
    path: String,
    expected: f64,
    tolerance: f64,
) {
    let actual = json_at(world, &path).expect("path missing");
    let actual_f = actual.as_f64().expect("not a float");
    assert!(
        (actual_f - expected).abs() < tolerance,
        "field {path}: expected {expected} ± {tolerance}, got {actual_f}"
    );
}

#[then(expr = "the response field {string} contains {string}")]
async fn then_response_field_contains(world: &mut PlateSolverWorld, path: String, needle: String) {
    let actual = json_at(world, &path).expect("path missing");
    let actual_s = actual.as_str().expect("not a string");
    assert!(
        actual_s.contains(&needle),
        "field {path}: expected to contain {needle:?}, got {actual_s:?}"
    );
}

#[then(expr = "the response field {string} contains {string} case-insensitively")]
async fn then_response_field_contains_ci(
    world: &mut PlateSolverWorld,
    path: String,
    needle: String,
) {
    let actual = json_at(world, &path).expect("path missing");
    let actual_s = actual.as_str().expect("not a string");
    assert!(
        actual_s.to_lowercase().contains(&needle.to_lowercase()),
        "field {path}: expected to contain {needle:?} (case-insensitive), got {actual_s:?}"
    );
}

#[then(expr = "the spawned argv contains the flag {string}")]
async fn then_argv_contains_flag(world: &mut PlateSolverWorld, flag: String) {
    let argv = read_argv(world);
    assert!(
        argv.iter().any(|a| a == &flag),
        "argv missing flag {flag:?}: {argv:?}"
    );
}

#[then(expr = "the spawned argv value after {string} is approximately {float}")]
async fn then_argv_value_after_flag(world: &mut PlateSolverWorld, flag: String, expected: f64) {
    let argv = read_argv(world);
    let idx = argv
        .iter()
        .position(|a| a == &flag)
        .unwrap_or_else(|| panic!("argv missing flag {flag:?}: {argv:?}"));
    let value: f64 = argv
        .get(idx + 1)
        .expect("no value after flag")
        .parse()
        .expect("non-numeric value");
    assert!(
        (value - expected).abs() < 1e-3,
        "argv value after {flag:?}: expected ~{expected}, got {value}"
    );
}

// Append "<label>,<elapsed_ms>" to the CSV file named by the
// `PLATE_SOLVER_PERF_CSV` env var. When the env var is unset (the
// local-dev default), the step is a no-op so BDD runs aren't burdened
// with perf-trending side effects. The nightly workflow sets the var
// per matrix leg and uploads the resulting CSV as an artifact (issue
// #236). Writes a header row on first write so the CSV is
// self-describing.
#[then(expr = "I record the solve duration as {string}")]
async fn then_record_solve_duration(world: &mut PlateSolverWorld, label: String) {
    use std::io::Write;

    let Ok(path) = std::env::var("PLATE_SOLVER_PERF_CSV") else {
        return;
    };
    let elapsed = world
        .last_response_elapsed
        .expect("no last_response_elapsed recorded — was there a When step?");
    let elapsed_ms = elapsed.as_millis();
    let path = std::path::PathBuf::from(path);
    let needs_header = !path.exists();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).expect("create perf csv parent dir");
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    if needs_header {
        writeln!(file, "label,elapsed_ms").expect("write csv header");
    }
    writeln!(file, "{label},{elapsed_ms}").expect("write csv row");
}

// ----- helpers -----

async fn ensure_wrapper(world: &mut PlateSolverWorld) {
    if world.service_handle.is_none() {
        world.start_wrapper_with_mock().await;
    }
}

async fn do_post(world: &mut PlateSolverWorld, body: serde_json::Value) {
    let url = format!("{}/api/v1/solve", world.wrapper_url());
    let started = Instant::now();
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .expect("POST");
    record_response(world, resp, started).await;
}

async fn record_response(world: &mut PlateSolverWorld, resp: reqwest::Response, started: Instant) {
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.expect("read body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    world.last_response_elapsed = Some(started.elapsed());
    world.last_response = Some(HttpResponse { status, body });
}

/// Walk a dotted JSON path into the response body.
fn json_at<'a>(world: &'a PlateSolverWorld, path: &str) -> Option<&'a serde_json::Value> {
    let body = &world.last_response.as_ref()?.body;
    let mut cur = body;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

fn read_argv(world: &PlateSolverWorld) -> Vec<String> {
    let p = world
        .argv_out_path
        .as_ref()
        .expect("argv_out_path not set — Given step missing?");
    let s = std::fs::read_to_string(p).expect("read argv_out");
    s.lines()
        .filter(|l| !l.is_empty())
        .map(std::string::ToString::to_string)
        .collect()
}
