use crate::world::{FilemonitorWorld, ParsingRuleConfig};
use ascom_alpaca::ASCOMErrorCode;
use cucumber::{given, then};

#[given("case-insensitive matching")]
const fn case_insensitive(world: &mut FilemonitorWorld) {
    world.case_sensitive = false;
}

#[given("case-sensitive matching")]
const fn case_sensitive(world: &mut FilemonitorWorld) {
    world.case_sensitive = true;
}

#[given(expr = "a contains rule with pattern {string} that evaluates to safe")]
fn contains_rule_safe(world: &mut FilemonitorWorld, pattern: String) {
    world.rules.push(ParsingRuleConfig {
        rule_type: "contains".to_string(),
        pattern,
        safe: true,
    });
}

#[given(expr = "a contains rule with pattern {string} that evaluates to unsafe")]
fn contains_rule_unsafe(world: &mut FilemonitorWorld, pattern: String) {
    world.rules.push(ParsingRuleConfig {
        rule_type: "contains".to_string(),
        pattern,
        safe: false,
    });
}

fn resolve_regex_pattern(name: &str) -> String {
    match name {
        "safe_or_ok" => r"Status:\s*(SAFE|OK)".to_string(),
        "danger_or_error" => r"Status:\s*(DANGER|ERROR)".to_string(),
        other => other.to_string(),
    }
}

#[given(expr = "a regex rule with pattern {string} that evaluates to safe")]
fn regex_rule_safe(world: &mut FilemonitorWorld, pattern: String) {
    world.rules.push(ParsingRuleConfig {
        rule_type: "regex".to_string(),
        pattern: resolve_regex_pattern(&pattern),
        safe: true,
    });
}

#[given(expr = "a regex rule with pattern {string} that evaluates to unsafe")]
fn regex_rule_unsafe(world: &mut FilemonitorWorld, pattern: String) {
    world.rules.push(ParsingRuleConfig {
        rule_type: "regex".to_string(),
        pattern: resolve_regex_pattern(&pattern),
        safe: false,
    });
}

#[given("filemonitor is running with these rules")]
async fn filemonitor_running_with_rules(world: &mut FilemonitorWorld) {
    world.start_filemonitor().await;
}

#[given(expr = "filemonitor is running with these rules and {int} second polling")]
async fn filemonitor_running_with_rules_and_polling(world: &mut FilemonitorWorld, interval: u64) {
    world.polling_interval = interval;
    world.start_filemonitor().await;
}

#[then(expr = "is_safe should return {word}")]
async fn is_safe_should_return(world: &mut FilemonitorWorld, expected: String) {
    let result = world.monitor().is_safe().await.unwrap();
    let expected_val = expected == "true";
    assert_eq!(
        result, expected_val,
        "expected is_safe={expected_val} but got {result}"
    );
}

#[then("is_safe should fail with a not connected error")]
async fn is_safe_should_fail_not_connected(world: &mut FilemonitorWorld) {
    let err = world
        .monitor()
        .is_safe()
        .await
        .expect_err("expected NotConnected error but got Ok");
    assert_eq!(
        err.code,
        ASCOMErrorCode::NOT_CONNECTED,
        "expected NOT_CONNECTED error code but got {:?}: {}",
        err.code,
        err
    );
}

#[then(expr = "the name should be {string}")]
async fn name_should_be(world: &mut FilemonitorWorld, expected: String) {
    let name = world.monitor().name().await.unwrap();
    assert_eq!(
        name, expected,
        "expected name '{expected}' but got '{name}'"
    );
}

#[then(expr = "the description should be {string}")]
async fn description_should_be(world: &mut FilemonitorWorld, expected: String) {
    let description = world.monitor().description().await.unwrap();
    assert_eq!(description, expected);
}

#[then(expr = "the driver info should be {string}")]
async fn driver_info_should_be(world: &mut FilemonitorWorld, expected: String) {
    let driver_info = world.monitor().driver_info().await.unwrap();
    assert_eq!(driver_info, expected);
}

#[then(expr = "the driver version should be {string}")]
async fn driver_version_should_be(world: &mut FilemonitorWorld, expected: String) {
    let driver_version = world.monitor().driver_version().await.unwrap();
    assert_eq!(driver_version, expected);
}
