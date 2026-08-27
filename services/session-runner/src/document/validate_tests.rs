//! Document-validation tests: the shared corpus battery plus targeted
//! model-shape and error-payload checks.

use serde_json::{json, Value};

use super::corpus::{self, Case, Expect};
use super::*;

/// Runs every corpus case through `Document::parse` and aggregates
/// failures so one run reports every deviation, mirroring the expression
/// module's conformance suite.
#[test]
fn test_corpus() {
    let mut failures = Vec::new();
    for Case { name, src, expect } in corpus::cases() {
        match (expect, Document::parse(&src)) {
            (Expect::Valid, Err(issues)) => {
                failures.push(format!(
                    "  {name}: expected valid, got: {}",
                    render(&issues)
                ));
            }
            (Expect::Invalid(_), Ok(_)) => {
                failures.push(format!(
                    "  {name}: expected rejection, document was accepted"
                ));
            }
            (Expect::Invalid(wants), Err(issues)) => {
                for (pointer, fragment) in wants {
                    let hit = issues
                        .iter()
                        .any(|i| i.pointer == *pointer && i.message.contains(fragment));
                    if !hit {
                        failures.push(format!(
                            "  {name}: no issue at {pointer:?} containing {fragment:?}; got: {}",
                            render(&issues)
                        ));
                    }
                }
            }
            (Expect::Valid, Ok(_)) => {}
        }
    }
    assert!(
        failures.is_empty(),
        "{} corpus failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn render(issues: &[ValidationIssue]) -> String {
    let lines: Vec<String> = issues.iter().map(ToString::to_string).collect();
    lines.join("; ")
}

#[test]
fn test_json_syntax_error_is_a_single_whole_document_issue() {
    let issues = Document::parse("{ not json").unwrap_err();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].pointer, "");
    assert!(
        issues[0].message.contains("not valid JSON"),
        "{}",
        issues[0].message
    );
}

#[test]
fn test_arbitrarily_deep_values_are_gated_not_recursed() {
    // `Document::from_value` is public: a programmatically built `Value`
    // never went through serde_json's 128-level parser limit, so the
    // walk gates nesting explicitly instead of recursing into it.
    let mut v = json!(1);
    for _ in 0..300 {
        v = json!({ "sequence": [v] });
    }
    let issues = Document::from_value(&v).unwrap_err();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].pointer, "");
    assert!(
        issues[0].message.contains("nesting exceeds 128 levels"),
        "{}",
        issues[0].message
    );

    // Values arriving as text hit serde_json's own limit first.
    let deep_text = format!("{}1{}", "[".repeat(300), "]".repeat(300));
    let issues = Document::parse(&deep_text).unwrap_err();
    assert!(
        issues[0].message.contains("not valid JSON"),
        "{}",
        issues[0].message
    );
}

#[test]
fn test_nesting_gate_counts_containers_only_and_sits_at_128() {
    let deep = |containers: usize| {
        let mut v = json!(1);
        for _ in 0..containers {
            v = json!([v]);
        }
        v
    };
    // Exactly 128 containers clear the gate — the innermost primitive
    // does not add a level — and the walk then reports the actual shape
    // error. (serde_json itself stops at 127 containers, so nothing
    // parseable comes near the gate.)
    let issues = Document::from_value(&deep(128)).unwrap_err();
    assert!(
        issues[0].message.contains("must be a JSON object"),
        "{}",
        issues[0].message
    );
    let issues = Document::from_value(&deep(129)).unwrap_err();
    assert!(
        issues[0].message.contains("nesting exceeds 128 levels"),
        "{}",
        issues[0].message
    );
}

#[test]
fn test_unsupported_version_short_circuits_all_other_findings() {
    // The rest of this document is riddled with v1 errors, but a document
    // for another format version gets exactly one error and no v1 noise.
    let issues = Document::parse(&json!({ "version": 3, "root": "nope", "extra": 1 }).to_string())
        .unwrap_err();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].pointer, "/version");
}

#[test]
fn test_all_findings_are_reported_in_one_pass() {
    let issues = Document::parse(
        &json!({
            "version": 1, "name": "",
            "estimated_duration": "soon",
            "root": { "toool": "park" }
        })
        .to_string(),
    )
    .unwrap_err();
    let pointers: Vec<&str> = issues.iter().map(|i| i.pointer.as_str()).collect();
    assert!(pointers.contains(&"/name"), "{pointers:?}");
    assert!(pointers.contains(&"/estimated_duration"), "{pointers:?}");
    assert!(pointers.contains(&"/root"), "{pointers:?}");
}

#[test]
fn test_expression_errors_carry_the_span_within_the_expression() {
    let issues = Document::parse(
        &json!({ "version": 1, "name": "t",
                 "root": { "if": "1 ++ 2", "then": [ { "tool": "x" } ] } })
        .to_string(),
    )
    .unwrap_err();
    assert_eq!(issues[0].pointer, "/root/if");
    let span = issues[0].expr_span.unwrap();
    assert!(span.start <= span.end && span.end <= "1 ++ 2".len());
}

#[test]
fn test_validation_issue_serializes_for_the_validate_response() {
    let issues = Document::parse(
        &json!({ "version": 1, "name": "t",
                 "root": { "if": "1 +", "then": [ { "tool": "x" } ] } })
        .to_string(),
    )
    .unwrap_err();
    let v = serde_json::to_value(&issues[0]).unwrap();
    assert_eq!(v["pointer"], "/root/if");
    assert!(v["message"].is_string());
    assert!(v["expr_span"]["start"].is_u64());
    assert!(v["expr_span"]["end"].is_u64());

    // Non-expression issues omit the span field entirely.
    let issues = Document::parse(&json!({ "version": 1 }).to_string()).unwrap_err();
    let v = serde_json::to_value(&issues[0]).unwrap();
    assert!(v.get("expr_span").is_none());
}

#[test]
fn test_validation_issue_display_is_pointer_then_message() {
    let issue = ValidationIssue {
        pointer: "/root/if".to_owned(),
        message: "invalid expression: boom".to_owned(),
        expr_span: None,
    };
    assert_eq!(issue.to_string(), "/root/if: invalid expression: boom");
}

#[test]
fn test_pointer_segments_are_rfc6901_escaped() {
    // A tool-arg key containing `/` and `~` must be escaped in the
    // pointer (`~1`, `~0`) so consumers can resolve it mechanically.
    let issues = Document::parse(
        &json!({ "version": 1, "name": "t",
                 "root": { "tool": "x", "args": { "a/b~c": { "$expr": 3 } } } })
        .to_string(),
    )
    .unwrap_err();
    assert_eq!(issues[0].pointer, "/root/args/a~1b~0c/$expr");
}

// ---- typed-model shape ---------------------------------------------------

fn parse(v: Value) -> Document {
    Document::from_value(&v).unwrap()
}

#[test]
fn test_golden_document_builds_the_expected_tree() {
    let doc = parse(corpus::golden_calibrator_flats());
    assert_eq!(doc.version, 1);
    assert_eq!(doc.name, "calibrator-flats");
    assert_eq!(doc.parameters.len(), 8);
    assert_eq!(doc.triggers, Vec::<crate::document::model::Trigger>::new());

    let InstructionKind::Sequence(steps) = &doc.root.kind else {
        panic!("root is {:?}", doc.root.kind);
    };
    assert_eq!(steps.len(), 6);

    // camera-info tool, set, the fail-fast target guard, the initial
    // cover-state read + set, then the try.
    assert_eq!(steps[2].id.as_deref(), Some("target-adu-guard"));
    assert_eq!(steps[3].id.as_deref(), Some("initial-cover"));
    let InstructionKind::Try {
        body,
        catch,
        finally,
    } = &steps[5].kind
    else {
        panic!("sixth step is {:?}", steps[5].kind);
    };
    assert!(catch.is_none());
    assert_eq!(finally.as_ref().unwrap().len(), 2);
    assert_eq!(body.len(), 5);

    // The filter-plan loop: a `while` mode over the array parameter with
    // a literal budget.
    let filter_plan = &body[3];
    assert_eq!(filter_plan.id.as_deref(), Some("filter-plan"));
    let InstructionKind::Repeat(outer) = &filter_plan.kind else {
        panic!("filter-plan is {:?}", filter_plan.kind);
    };
    let RepeatMode::While {
        condition,
        max_iterations,
    } = &outer.mode
    else {
        panic!("mode is {:?}", outer.mode);
    };
    assert_eq!(
        condition.source(),
        "has(params.filters[session.filter_index])"
    );
    assert_eq!(max_iterations, &Bound::Literal(64));
    assert_eq!(outer.body.len(), 6);

    // The brightness ladder wraps the find-exposure loop; the inner
    // find-exposure is an `until` mode with an `$expr` bound.
    let ladder = &outer.body[2];
    assert_eq!(ladder.id.as_deref(), Some("brightness-ladder"));
    let InstructionKind::Repeat(ladder_repeat) = &ladder.kind else {
        panic!("brightness-ladder is {:?}", ladder.kind);
    };
    let find_exposure = &ladder_repeat.body[0];
    assert_eq!(find_exposure.id.as_deref(), Some("find-exposure"));
    let InstructionKind::Repeat(repeat) = &find_exposure.kind else {
        panic!("find-exposure is {:?}", find_exposure.kind);
    };
    let RepeatMode::Until {
        condition,
        max_iterations,
    } = &repeat.mode
    else {
        panic!("mode is {:?}", repeat.mode);
    };
    assert!(condition.source().contains("params.tolerance"));
    let Bound::Expr(e) = max_iterations else {
        panic!("bound is {max_iterations:?}");
    };
    assert_eq!(e.source(), "params.max_iterations");

    // The capture loop: a `count` mode with an `$expr` count.
    let InstructionKind::Repeat(capture_loop) = &outer.body[4].kind else {
        panic!("capture loop is {:?}", outer.body[4].kind);
    };
    assert!(matches!(
        &capture_loop.mode,
        RepeatMode::Count {
            count: Bound::Expr(_),
            max_iterations: None
        }
    ));

    // Tool args: `$expr` wrappers and literals are told apart.
    let InstructionKind::Tool(get_info) = &steps[0].kind else {
        panic!("first step is {:?}", steps[0].kind);
    };
    assert_eq!(get_info.tool, "get_camera_info");
    assert!(matches!(get_info.args["camera_id"], ArgValue::Expr(_)));
    assert!(get_info.retry.is_none());
}

#[test]
fn test_parameter_declarations_capture_type_and_default() {
    let doc = parse(corpus::golden_calibrator_flats());
    let cam = &doc.parameters["camera_id"];
    assert_eq!(cam.ty, ParameterType::String);
    assert!(cam.default.is_none());

    let tolerance = &doc.parameters["tolerance"];
    assert_eq!(tolerance.ty, ParameterType::Number);
    assert_eq!(tolerance.default, Some(json!(0.05)));

    let initial = &doc.parameters["initial_duration"];
    assert_eq!(initial.ty, ParameterType::Duration);
    // Duration defaults stay strings: expressions read them via seconds().
    assert_eq!(initial.default, Some(json!("1s")));

    let filters = &doc.parameters["filters"];
    assert_eq!(filters.ty, ParameterType::Array);
    assert!(filters.default.is_none());
}

#[test]
fn test_set_entries_expose_path_segments_and_document_key() {
    let doc = parse(json!({
        "version": 1, "name": "t",
        "root": { "set": { "session.report.frames": "0" } }
    }));
    let InstructionKind::Set(entries) = &doc.root.kind else {
        panic!("root is {:?}", doc.root.kind);
    };
    assert_eq!(entries[0].path, vec!["report", "frames"]);
    assert_eq!(entries[0].key(), "session.report.frames");
}

#[test]
fn test_wait_until_defaults_poll_interval_to_ten_seconds() {
    let doc = parse(json!({
        "version": 1, "name": "t",
        "root": { "wait": { "until": "session.ready == true", "timeout": "1m" } }
    }));
    let InstructionKind::Wait(Wait::Until {
        poll_interval,
        timeout,
        ..
    }) = &doc.root.kind
    else {
        panic!("root is {:?}", doc.root.kind);
    };
    assert_eq!(*poll_interval, std::time::Duration::from_secs(10));
    assert_eq!(*timeout, std::time::Duration::from_mins(1));
}

#[test]
fn test_log_level_defaults_to_debug() {
    let doc = parse(json!({
        "version": 1, "name": "t", "root": { "log": { "message": "x" } }
    }));
    let InstructionKind::Log(log) = &doc.root.kind else {
        panic!("root is {:?}", doc.root.kind);
    };
    assert_eq!(log.level, LogLevel::Debug);
    assert!(log.values.is_empty());
}

#[test]
fn test_trigger_model_captures_gates_and_bookkeeping_fields() {
    let doc = parse(json!({
        "version": 1, "name": "t", "root": { "sequence": [] },
        "triggers": [ {
            "id": "refocus",
            "on": { "event": "exposure_complete" },
            "when": "event.document_id != null",
            "while": "session.imaging == true",
            "cooldown": "15m",
            "once": true,
            "do": [ { "log": { "message": "x" } } ]
        } ]
    }));
    let t = &doc.triggers[0];
    assert_eq!(t.id, "refocus");
    assert_eq!(t.on, TriggerSource::Event("exposure_complete".to_owned()));
    assert!(t.when.is_some() && t.while_gate.is_some());
    assert_eq!(t.cooldown, Some(std::time::Duration::from_mins(15)));
    assert!(t.once);
    assert_eq!(t.actions.len(), 1);
}

#[test]
fn test_trigger_once_defaults_to_false() {
    let doc = parse(json!({
        "version": 1, "name": "t", "root": { "sequence": [] },
        "triggers": [ { "id": "a", "on": { "event": "x" },
                        "do": [ { "log": { "message": "x" } } ] } ]
    }));
    assert!(!doc.triggers[0].once);
}

#[test]
fn test_estimated_and_max_duration_parse_to_durations() {
    let doc = parse(json!({
        "version": 1, "name": "t", "estimated_duration": "1h30m",
        "max_duration": "12h", "root": { "sequence": [] }
    }));
    assert_eq!(
        doc.estimated_duration,
        Some(std::time::Duration::from_mins(90))
    );
    assert_eq!(doc.max_duration, Some(std::time::Duration::from_hours(12)));
    let bare = parse(json!({ "version": 1, "name": "t", "root": { "sequence": [] } }));
    assert!(bare.estimated_duration.is_none() && bare.max_duration.is_none());
}
