//! Assertions over the exit code, the JSON report, and the text output.

use cucumber::then;
use serde_json::Value;

use crate::world::DoctorWorld;

fn check_matches(check: &Value, status: &str, name: &str, service: Option<&str>) -> bool {
    check["status"] == status
        && check["name"] == name
        && service.is_none_or(|s| check["service"] == s)
}

pub fn find_check(world: &mut DoctorWorld, status: &str, name: &str, service: Option<&str>) {
    let found = world
        .checks()
        .iter()
        .find(|c| check_matches(c, status, name, service))
        .cloned();
    match found {
        Some(check) => world.last_check = Some(check),
        None => panic!(
            "no {status} check named {name}{} in report:\n{}",
            service
                .map(|s| format!(" for service {s}"))
                .unwrap_or_default(),
            serde_json::to_string_pretty(world.report()).expect("report serializes")
        ),
    }
}

#[then(expr = "the report contains no check named {string}")]
fn no_check_named(world: &mut DoctorWorld, name: String) {
    let matching: Vec<Value> = world
        .checks()
        .iter()
        .filter(|c| c["name"] == name.as_str())
        .cloned()
        .collect();
    assert!(
        matching.is_empty(),
        "unexpected check(s) named {name}: {matching:?}"
    );
}

#[then(expr = "doctor exits with code {int}")]
fn exit_code(world: &mut DoctorWorld, expected: i32) {
    let output = world.output.as_ref().expect("run doctor first");
    let code = output.status.code().expect("doctor was signal-killed");
    assert_eq!(
        code,
        expected,
        "expected exit {expected}, got {code}; stderr:\n{}\nstdout:\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}

#[then(expr = "the report field {string} is {int}")]
fn report_field_int(world: &mut DoctorWorld, field: String, expected: i64) {
    assert_eq!(
        world.report()[&field],
        Value::from(expected),
        "report field {field}"
    );
}

#[then(expr = "the report field {string} is {string}")]
fn report_field_str(world: &mut DoctorWorld, field: String, expected: String) {
    assert_eq!(
        world.report()[&field],
        Value::from(expected.as_str()),
        "report field {field}"
    );
}

#[then(expr = "the report contains a/an {string} check named {string} for service {string}")]
fn contains_check_for_service(
    world: &mut DoctorWorld,
    status: String,
    name: String,
    service: String,
) {
    find_check(world, &status, &name, Some(&service));
}

#[then(expr = "the report contains a/an {string} check named {string}")]
fn contains_check(world: &mut DoctorWorld, status: String, name: String) {
    find_check(world, &status, &name, None);
}

#[then(expr = "that check's detail mentions {string}")]
fn check_detail_mentions(world: &mut DoctorWorld, needle: String) {
    let needle = world.expand(&needle);
    let check = world.last_check.as_ref().expect("no check matched yet");
    let detail = check["detail"].as_str().expect("check has a detail");
    assert!(
        detail.contains(&needle),
        "detail {detail:?} lacks {needle:?}"
    );
}

#[then(expr = "that check's suggestion mentions {string}")]
fn check_suggestion_mentions(world: &mut DoctorWorld, needle: String) {
    let check = world.last_check.as_ref().expect("no check matched yet");
    let suggestion = check["suggestion"]
        .as_str()
        .expect("check has a suggestion");
    assert!(
        suggestion.contains(&needle),
        "suggestion {suggestion:?} lacks {needle:?}"
    );
}

#[then(expr = "the report has no {string} checks")]
fn no_checks_with_status(world: &mut DoctorWorld, status: String) {
    let offending: Vec<&Value> = world
        .checks()
        .iter()
        .filter(|c| c["status"] == status.as_str())
        .collect();
    assert!(
        offending.is_empty(),
        "unexpected {status} checks: {offending:?}"
    );
}

#[then(expr = "the report has no checks named {string}")]
fn no_checks_named(world: &mut DoctorWorld, name: String) {
    let offending: Vec<&Value> = world
        .checks()
        .iter()
        .filter(|c| c["name"] == name.as_str())
        .collect();
    assert!(
        offending.is_empty(),
        "unexpected checks named {name}: {offending:?}"
    );
}

#[then(expr = "the report contains exactly {int} check(s) named {string}")]
fn count_checks_named(world: &mut DoctorWorld, expected: usize, name: String) {
    let matching: Vec<&Value> = world
        .checks()
        .iter()
        .filter(|c| c["name"] == name.as_str())
        .collect();
    assert_eq!(
        matching.len(),
        expected,
        "checks named {name}: {matching:?}"
    );
}

#[then(expr = "the report has no checks named {string} for service {string}")]
fn no_checks_named_for_service(world: &mut DoctorWorld, name: String, service: String) {
    let offending: Vec<&Value> = world
        .checks()
        .iter()
        .filter(|c| c["name"] == name.as_str() && c["service"] == service.as_str())
        .collect();
    assert!(
        offending.is_empty(),
        "unexpected checks named {name} for {service}: {offending:?}"
    );
}

#[then(expr = "the report has no checks named {string} with status {string}")]
fn no_checks_named_with_status(world: &mut DoctorWorld, name: String, status: String) {
    let offending: Vec<&Value> = world
        .checks()
        .iter()
        .filter(|c| c["name"] == name.as_str() && c["status"] == status.as_str())
        .collect();
    assert!(
        offending.is_empty(),
        "unexpected {status} checks named {name}: {offending:?}"
    );
}

#[then(expr = "the text output contains {string}")]
fn text_output_contains(world: &mut DoctorWorld, needle: String) {
    let stdout = world.stdout();
    assert!(
        stdout.contains(&needle),
        "stdout lacks {needle:?}:\n{stdout}"
    );
}

#[then("the text output contains a summary line with the ok, warn, and fail counts")]
fn text_output_summary(world: &mut DoctorWorld) {
    let stdout = world.stdout();
    let summary = stdout
        .lines()
        .find(|l| l.starts_with("summary: "))
        .unwrap_or_else(|| panic!("no summary line in:\n{stdout}"));
    for part in ["ok", "warn", "fail"] {
        assert!(summary.contains(part), "summary {summary:?} lacks {part}");
    }
}
