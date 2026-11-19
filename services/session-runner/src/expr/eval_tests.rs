//! Table-driven evaluator tests: every operator, every function, every
//! namespace, and every error class from the evaluation pins in
//! `docs/services/session-runner.md` § Expressions.

use chrono::{DateTime, TimeZone, Utc};
use serde_json::{json, Value};

use super::{ErrorKind, EvalContext, ExprError, Expression, Span};

/// Fixed engine clock: 2026-07-03T04:00:00Z.
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 3, 4, 0, 0).unwrap()
}

fn eval_with(src: &str, ctx: &EvalContext<'_>) -> Result<Value, ExprError> {
    Expression::parse(src).unwrap().eval(ctx)
}

/// Evaluate with every namespace absent.
fn eval_empty(src: &str) -> Result<Value, ExprError> {
    eval_with(src, &EvalContext::new(now()))
}

fn session_ctx(session: &Value) -> EvalContext<'_> {
    EvalContext {
        session: Some(session),
        ..EvalContext::new(now())
    }
}

fn eval_session(src: &str, session: &Value) -> Result<Value, ExprError> {
    eval_with(src, &session_ctx(session))
}

// ---- literals --------------------------------------------------------------

#[test]
fn test_literals_evaluate_to_themselves() {
    assert_eq!(eval_empty("null").unwrap(), Value::Null);
    assert_eq!(eval_empty("true").unwrap(), json!(true));
    assert_eq!(eval_empty("false").unwrap(), json!(false));
    assert_eq!(eval_empty("3.5").unwrap(), json!(3.5));
    assert_eq!(eval_empty("-3").unwrap(), json!(-3.0));
    assert_eq!(eval_empty("'dark'").unwrap(), json!("dark"));
}

// ---- namespaces ------------------------------------------------------------

#[test]
fn test_every_namespace_resolves() {
    let params = json!({"v": "params"});
    let session = json!({"v": "session"});
    let result = json!({"v": "result"});
    let event = json!({"v": "event"});
    let error = json!({"v": "error"});
    let ctx = EvalContext {
        params: Some(&params),
        session: Some(&session),
        result: Some(&result),
        event: Some(&event),
        error: Some(&error),
        now: now(),
    };
    for ns in ["params", "session", "result", "event", "error"] {
        assert_eq!(
            eval_with(&format!("{ns}.v"), &ctx).unwrap(),
            json!(ns),
            "namespace {ns}"
        );
    }
}

#[test]
fn test_absent_namespace_root_is_null() {
    assert_eq!(eval_empty("session == null").unwrap(), json!(true));
    assert_eq!(eval_empty("error.message == null").unwrap(), json!(true));
}

#[test]
fn test_bare_namespace_root_evaluates_to_the_object() {
    let session = json!({"x": 1});
    assert_eq!(eval_session("session", &session).unwrap(), session);
}

// ---- path traversal (total: any miss is null) --------------------------------

#[test]
fn test_missing_member_is_null() {
    let session = json!({});
    assert_eq!(
        eval_session("session.missing == null", &session).unwrap(),
        json!(true)
    );
}

#[test]
fn test_member_on_non_object_is_null() {
    let session = json!({"num": 5, "s": "str", "arr": [1]});
    for path in ["session.num.field", "session.s.field", "session.arr.length"] {
        assert_eq!(
            eval_session(&format!("{path} == null"), &session).unwrap(),
            json!(true),
            "{path}"
        );
    }
}

#[test]
fn test_traversal_through_missing_chain_is_null() {
    let session = json!({});
    assert_eq!(
        eval_session("session.a.b.c == null", &session).unwrap(),
        json!(true)
    );
}

#[test]
fn test_array_index_in_range() {
    let session = json!({"arr": [10, 20, 30]});
    assert_eq!(eval_session("session.arr[0]", &session).unwrap(), json!(10));
    assert_eq!(eval_session("session.arr[2]", &session).unwrap(), json!(30));
}

#[test]
fn test_array_index_misses_are_null() {
    let session = json!({"arr": [10, 20, 30]});
    for src in [
        "session.arr[3]",     // out of range
        "session.arr[-1]",    // negative
        "session.arr[0.5]",   // fractional
        "session.arr['x']",   // wrong index type
        "session.arr[1e300]", // absurdly large
    ] {
        assert_eq!(
            eval_session(&format!("{src} == null"), &session).unwrap(),
            json!(true),
            "{src}"
        );
    }
}

#[test]
fn test_object_index_by_string() {
    let session = json!({"map": {"key": 7, "null": 8}});
    assert_eq!(
        eval_session("session.map['key']", &session).unwrap(),
        json!(7)
    );
    // Reserved words as field names are reachable via indexing.
    assert_eq!(
        eval_session("session.map['null']", &session).unwrap(),
        json!(8)
    );
    assert_eq!(
        eval_session("session.map['absent'] == null", &session).unwrap(),
        json!(true)
    );
    assert_eq!(
        eval_session("session.map[0] == null", &session).unwrap(),
        json!(true)
    );
}

#[test]
fn test_index_on_scalar_or_null_is_null() {
    let session = json!({"num": 5});
    assert_eq!(
        eval_session("session.num[0] == null", &session).unwrap(),
        json!(true)
    );
    assert_eq!(
        eval_session("session.missing[0] == null", &session).unwrap(),
        json!(true)
    );
}

#[test]
fn test_computed_index_expression_still_raises() {
    let session = json!({"arr": [1]});
    let err = eval_session("session.arr[1 / 0]", &session).unwrap_err();
    assert!(
        err.message.contains("division by zero"),
        "got: {}",
        err.message
    );
}

// ---- arithmetic --------------------------------------------------------------

#[test]
fn test_arithmetic_operators() {
    assert_eq!(eval_empty("1 + 2").unwrap(), json!(3.0));
    assert_eq!(eval_empty("5 - 2").unwrap(), json!(3.0));
    assert_eq!(eval_empty("3 * 4").unwrap(), json!(12.0));
    assert_eq!(eval_empty("10 / 4").unwrap(), json!(2.5));
    assert_eq!(eval_empty("7 % 3").unwrap(), json!(1.0));
    assert_eq!(eval_empty("-(1 + 2)").unwrap(), json!(-3.0));
}

#[test]
fn test_remainder_sign_follows_the_dividend() {
    assert_eq!(eval_empty("-7 % 3").unwrap(), json!(-1.0));
    assert_eq!(eval_empty("7 % -3").unwrap(), json!(1.0));
}

#[test]
fn test_division_by_zero_raises() {
    let err = eval_empty("1 / 0").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Eval);
    assert_eq!(err.message, "division by zero");
    assert_eq!(err.span, Span::new(0, 5));
}

#[test]
fn test_remainder_by_zero_raises() {
    let err = eval_empty("1 % 0").unwrap_err();
    assert!(err.message.contains("remainder"), "got: {}", err.message);
}

#[test]
fn test_overflow_raises_at_the_producing_operation() {
    for src in [
        "1e308 + 1e308",
        "-1e308 - 1e308",
        "1e308 * 10",
        "1e308 / 1e-308",
    ] {
        let err = eval_empty(src).unwrap_err();
        assert!(
            err.message.contains("overflow"),
            "{src}: got {}",
            err.message
        );
        assert_eq!(err.kind, ErrorKind::Eval, "{src}");
    }
}

#[test]
fn test_null_in_arithmetic_raises_with_guard_hint() {
    let err = eval_empty("1 + session.missing").unwrap_err();
    assert!(
        err.message.contains("guard with has"),
        "got: {}",
        err.message
    );
    // The span points at the null-producing operand, not the whole sum.
    assert_eq!(err.span, Span::new(4, 19));
}

#[test]
fn test_wrong_types_in_arithmetic_raise() {
    let err = eval_empty("'a' + 'b'").unwrap_err();
    assert!(err.message.contains("got string"), "got: {}", err.message);
    let err = eval_empty("true * 2").unwrap_err();
    assert!(err.message.contains("got boolean"), "got: {}", err.message);
    let session = json!({"arr": []});
    let err = eval_session("session.arr - 1", &session).unwrap_err();
    assert!(err.message.contains("got array"), "got: {}", err.message);
}

#[test]
fn test_unary_minus_needs_a_number() {
    let err = eval_empty("-'a'").unwrap_err();
    assert!(err.message.contains("unary `-`"), "got: {}", err.message);
}

// ---- ordered comparisons -------------------------------------------------------

#[test]
fn test_ordered_comparisons_on_numbers() {
    assert_eq!(eval_empty("1 < 2").unwrap(), json!(true));
    assert_eq!(eval_empty("2 <= 2").unwrap(), json!(true));
    assert_eq!(eval_empty("3 > 4").unwrap(), json!(false));
    assert_eq!(eval_empty("5 >= 5").unwrap(), json!(true));
}

#[test]
fn test_string_ordering_is_a_type_error() {
    let err = eval_empty("'a' < 'b'").unwrap_err();
    assert!(
        err.message.contains("string ordering is not defined"),
        "got: {}",
        err.message
    );
}

#[test]
fn test_null_and_bool_in_ordered_comparison_raise() {
    let err = eval_empty("session.missing < 1").unwrap_err();
    assert!(
        err.message.contains("guard with has"),
        "got: {}",
        err.message
    );
    let err = eval_empty("true < false").unwrap_err();
    assert!(err.message.contains("got boolean"), "got: {}", err.message);
}

// ---- equality --------------------------------------------------------------

#[test]
fn test_equality_is_strict_within_a_type() {
    assert_eq!(eval_empty("1 == 1").unwrap(), json!(true));
    assert_eq!(eval_empty("'a' == 'a'").unwrap(), json!(true));
    assert_eq!(eval_empty("'a' != 'b'").unwrap(), json!(true));
    assert_eq!(eval_empty("true == true").unwrap(), json!(true));
    assert_eq!(eval_empty("null == null").unwrap(), json!(true));
}

#[test]
fn test_cross_type_equality_is_false_not_an_error() {
    assert_eq!(eval_empty("1 == '1'").unwrap(), json!(false));
    assert_eq!(eval_empty("true == 1").unwrap(), json!(false));
    assert_eq!(eval_empty("null == 0").unwrap(), json!(false));
    assert_eq!(eval_empty("null != 0").unwrap(), json!(true));
}

#[test]
fn test_json_integer_equals_expression_number() {
    // Tool results carry JSON integers; expression numbers are f64. They
    // must compare by numeric value, not by JSON representation.
    let session = json!({"count": 5});
    assert_eq!(
        eval_session("session.count == 5", &session).unwrap(),
        json!(true)
    );
    assert_eq!(
        eval_session("session.count == 5.0", &session).unwrap(),
        json!(true)
    );
}

#[test]
fn test_deep_equality_ignores_numeric_representation() {
    let session = json!({"a": [1, {"k": 2}], "b": [1.0, {"k": 2.0}]});
    assert_eq!(
        eval_session("session.a == session.b", &session).unwrap(),
        json!(true)
    );
}

#[test]
fn test_deep_equality_on_arrays_and_objects() {
    let session = json!({
        "a": [1, 2], "b": [1, 2], "c": [2, 1],
        "o1": {"x": 1, "y": 2}, "o2": {"y": 2, "x": 1}, "o3": {"x": 1}
    });
    assert_eq!(
        eval_session("session.a == session.b", &session).unwrap(),
        json!(true)
    );
    assert_eq!(
        eval_session("session.a == session.c", &session).unwrap(),
        json!(false)
    );
    assert_eq!(
        eval_session("session.o1 == session.o2", &session).unwrap(),
        json!(true)
    );
    assert_eq!(
        eval_session("session.o1 == session.o3", &session).unwrap(),
        json!(false)
    );
}

// ---- logic -------------------------------------------------------------------

#[test]
fn test_boolean_operators() {
    assert_eq!(eval_empty("true && false").unwrap(), json!(false));
    assert_eq!(eval_empty("true || false").unwrap(), json!(true));
    assert_eq!(eval_empty("!true").unwrap(), json!(false));
}

#[test]
fn test_and_or_short_circuit() {
    // The right side would raise (division by zero) if evaluated.
    assert_eq!(eval_empty("false && 1 / 0 == 1").unwrap(), json!(false));
    assert_eq!(eval_empty("true || 1 / 0 == 1").unwrap(), json!(true));
}

#[test]
fn test_short_circuit_makes_has_guards_sound() {
    let session = json!({});
    assert_eq!(
        eval_session("has(session.x) && session.x > 0", &session).unwrap(),
        json!(false)
    );
}

#[test]
fn test_no_truthiness() {
    let err = eval_empty("1 && true").unwrap_err();
    assert!(
        err.message.contains("no truthiness"),
        "got: {}",
        err.message
    );
    let err = eval_empty("true && 'yes'").unwrap_err();
    assert!(
        err.message.contains("no truthiness"),
        "got: {}",
        err.message
    );
    let err = eval_empty("!session.missing").unwrap_err();
    assert!(
        err.message.contains("guard with has"),
        "got: {}",
        err.message
    );
}

// ---- conditional -----------------------------------------------------------

#[test]
fn test_conditional_selects_branch() {
    assert_eq!(eval_empty("true ? 1 : 2").unwrap(), json!(1.0));
    assert_eq!(eval_empty("false ? 1 : 2").unwrap(), json!(2.0));
}

#[test]
fn test_conditional_evaluates_only_the_taken_branch() {
    assert_eq!(eval_empty("true ? 1 : 1 / 0").unwrap(), json!(1.0));
    assert_eq!(eval_empty("false ? 1 / 0 : 2").unwrap(), json!(2.0));
}

#[test]
fn test_conditional_needs_a_boolean_condition() {
    let err = eval_empty("1 ? 2 : 3").unwrap_err();
    assert!(
        err.message.contains("`?:` condition"),
        "got: {}",
        err.message
    );
}

// ---- functions ---------------------------------------------------------------

#[test]
fn test_abs() {
    assert_eq!(eval_empty("abs(-2.5)").unwrap(), json!(2.5));
    assert_eq!(eval_empty("abs(2.5)").unwrap(), json!(2.5));
}

#[test]
fn test_floor_ceil() {
    assert_eq!(eval_empty("floor(1.7)").unwrap(), json!(1.0));
    assert_eq!(eval_empty("floor(-1.5)").unwrap(), json!(-2.0));
    assert_eq!(eval_empty("ceil(1.2)").unwrap(), json!(2.0));
    assert_eq!(eval_empty("ceil(-1.2)").unwrap(), json!(-1.0));
}

#[test]
fn test_round_half_away_from_zero() {
    assert_eq!(eval_empty("round(2.5)").unwrap(), json!(3.0));
    assert_eq!(eval_empty("round(-2.5)").unwrap(), json!(-3.0));
    assert_eq!(eval_empty("round(2.4)").unwrap(), json!(2.0));
}

#[test]
fn test_min_max_variadic() {
    assert_eq!(eval_empty("min(3, 1, 2)").unwrap(), json!(1.0));
    assert_eq!(eval_empty("max(3, 1, 2)").unwrap(), json!(3.0));
    assert_eq!(eval_empty("min(2, 1)").unwrap(), json!(1.0));
}

#[test]
fn test_min_rejects_non_numbers() {
    let err = eval_empty("min(1, 'a')").unwrap_err();
    assert!(err.message.contains("min()"), "got: {}", err.message);
}

#[test]
fn test_clamp() {
    assert_eq!(eval_empty("clamp(5, 0, 10)").unwrap(), json!(5.0));
    assert_eq!(eval_empty("clamp(-5, 0, 10)").unwrap(), json!(0.0));
    assert_eq!(eval_empty("clamp(15, 0, 10)").unwrap(), json!(10.0));
}

#[test]
fn test_clamp_rejects_inverted_bounds() {
    let err = eval_empty("clamp(5, 10, 0)").unwrap_err();
    assert!(err.message.contains("lo <= hi"), "got: {}", err.message);
}

#[test]
fn test_seconds_parses_humantime() {
    assert_eq!(eval_empty("seconds('1m30s')").unwrap(), json!(90.0));
    assert_eq!(eval_empty("seconds('2h')").unwrap(), json!(7200.0));
    assert_eq!(eval_empty("seconds('500ms')").unwrap(), json!(0.5));
}

#[test]
fn test_seconds_rejects_garbage_and_non_strings() {
    let err = eval_empty("seconds('not a duration')").unwrap_err();
    assert!(
        err.message.contains("could not parse"),
        "got: {}",
        err.message
    );
    let err = eval_empty("seconds(90)").unwrap_err();
    assert!(
        err.message.contains("humantime string"),
        "got: {}",
        err.message
    );
}

#[test]
fn test_humantime_formats_seconds() {
    assert_eq!(eval_empty("humantime(90)").unwrap(), json!("1m 30s"));
    assert_eq!(eval_empty("humantime(0)").unwrap(), json!("0s"));
    assert_eq!(
        eval_empty("humantime(90.5)").unwrap(),
        json!("1m 30s 500ms")
    );
}

#[test]
fn test_humantime_rejects_negative_and_absurd_inputs() {
    let err = eval_empty("humantime(-1)").unwrap_err();
    assert!(err.message.contains("non-negative"), "got: {}", err.message);
    let err = eval_empty("humantime(1e300)").unwrap_err();
    assert!(err.message.contains("in range"), "got: {}", err.message);
    let err = eval_empty("humantime('90s')").unwrap_err();
    assert!(
        err.message.contains("needs a number"),
        "got: {}",
        err.message
    );
}

#[test]
fn test_seconds_humantime_round_trip() {
    assert_eq!(
        eval_empty("seconds(humantime(90)) == 90").unwrap(),
        json!(true)
    );
}

#[test]
fn test_has_semantics() {
    let session = json!({"present": 1, "explicit_null": null, "nested": {"deep": [42]}});
    assert_eq!(
        eval_session("has(session.present)", &session).unwrap(),
        json!(true)
    );
    assert_eq!(
        eval_session("has(session.explicit_null)", &session).unwrap(),
        json!(false)
    );
    assert_eq!(
        eval_session("has(session.missing)", &session).unwrap(),
        json!(false)
    );
    assert_eq!(
        eval_session("has(session.nested.deep[0])", &session).unwrap(),
        json!(true)
    );
    assert_eq!(
        eval_session("has(session.nested.deep[9])", &session).unwrap(),
        json!(false)
    );
    assert_eq!(eval_empty("has(session)").unwrap(), json!(false));
}

#[test]
fn test_seconds_until_future_past_and_offset() {
    // now() is 2026-07-03T04:00:00Z.
    assert_eq!(
        eval_empty("seconds_until('2026-07-03T04:05:00Z')").unwrap(),
        json!(300.0)
    );
    assert_eq!(
        eval_empty("seconds_until('2026-07-03T03:00:00Z')").unwrap(),
        json!(-3600.0)
    );
    // A +02:00 offset time equal to 04:00Z.
    assert_eq!(
        eval_empty("seconds_until('2026-07-03T06:00:00+02:00')").unwrap(),
        json!(0.0)
    );
}

#[test]
fn test_seconds_until_fractional_seconds_keep_their_sign() {
    // chrono's `TimeDelta::subsec_nanos()` is signed (unlike
    // `std::time::Duration`'s), so a negative fractional delta keeps its
    // sign and magnitude: num_seconds() + subsec_nanos()*1e-9 is exact.
    assert_eq!(
        eval_empty("seconds_until('2026-07-03T03:59:58.5Z')").unwrap(),
        json!(-1.5)
    );
    assert_eq!(
        eval_empty("seconds_until('2026-07-03T03:59:59.75Z')").unwrap(),
        json!(-0.25)
    );
    assert_eq!(
        eval_empty("seconds_until('2026-07-03T04:00:00.25Z')").unwrap(),
        json!(0.25)
    );
}

#[test]
fn test_seconds_until_reads_the_blackboard() {
    let session = json!({"flip_at": "2026-07-03T04:10:00Z"});
    assert_eq!(
        eval_session("seconds_until(session.flip_at) <= 0", &session).unwrap(),
        json!(false)
    );
}

#[test]
fn test_seconds_until_rejects_garbage_and_non_strings() {
    let err = eval_empty("seconds_until('yesterday')").unwrap_err();
    assert!(err.message.contains("RFC 3339"), "got: {}", err.message);
    let err = eval_empty("seconds_until(5)").unwrap_err();
    assert!(err.message.contains("RFC 3339"), "got: {}", err.message);
}

// ---- golden end-to-end -----------------------------------------------------

#[test]
fn test_golden_flats_tolerance_check() {
    let params = json!({"tolerance": 0.1});
    let session = json!({"median_adu": 31000.0, "target_adu": 30000.0});
    let ctx = EvalContext {
        params: Some(&params),
        session: Some(&session),
        ..EvalContext::new(now())
    };
    let v = eval_with(
        "abs(session.median_adu - session.target_adu) / session.target_adu <= params.tolerance",
        &ctx,
    )
    .unwrap();
    assert_eq!(v, json!(true));
}

#[test]
fn test_golden_flats_exposure_adaptation() {
    let session = json!({"duration": 2.0, "target_adu": 30000.0, "exp_min": 0.1, "exp_max": 10.0});
    let src = "clamp(result.median_adu == 0 ? session.duration * 2 : session.duration * (session.target_adu / result.median_adu), session.exp_min, session.exp_max)";

    // A dark result doubles the exposure…
    let result = json!({"median_adu": 0});
    let ctx = EvalContext {
        session: Some(&session),
        result: Some(&result),
        ..EvalContext::new(now())
    };
    assert_eq!(eval_with(src, &ctx).unwrap(), json!(4.0));

    // …a bright one scales it toward the target, clamped to bounds.
    let result = json!({"median_adu": 60000.0});
    let ctx = EvalContext {
        session: Some(&session),
        result: Some(&result),
        ..EvalContext::new(now())
    };
    assert_eq!(eval_with(src, &ctx).unwrap(), json!(1.0));
}

// ---- error metadata -----------------------------------------------------------

#[test]
fn test_eval_errors_have_eval_kind_and_serialize() {
    let err = eval_empty("1 / 0").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Eval);
    let v = serde_json::to_value(&err).unwrap();
    assert_eq!(v["kind"], "eval");
}

#[test]
fn test_number_precision_follows_f64() {
    // Numbers are f64 by contract: a u64 beyond 2^53 compares by its f64
    // approximation. This is the documented trade-off of the f64-only pin
    // (design § Expressions).
    let session = json!({"big": 9_007_199_254_740_993_u64});
    assert_eq!(
        eval_session("session.big == 9007199254740992", &session).unwrap(),
        json!(true)
    );
}
