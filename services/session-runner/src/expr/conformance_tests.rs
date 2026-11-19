//! Conformance corpus derived from `docs/services/session-runner.md`
//! § Expressions and its grammar pins. Accept
//! cases optionally pin the canonical parse shape (precedence /
//! associativity probes); reject cases must fail at the parse or static-
//! check layer. The corpus is the regression suite for the grammar.

use super::{ErrorKind, Expression};

enum Expect {
    /// Must parse + pass static checks. `Some(canon)` additionally pins
    /// the parse shape.
    Accept(Option<&'static str>),
    /// Must fail (parse or static-check layer).
    Reject,
}

struct Case {
    src: &'static str,
    expect: Expect,
    tag: &'static str,
}

macro_rules! acc {
    ($src:expr, $tag:expr) => {
        Case {
            src: $src,
            expect: Expect::Accept(None),
            tag: $tag,
        }
    };
    ($src:expr, $tag:expr, canon = $canon:expr) => {
        Case {
            src: $src,
            expect: Expect::Accept(Some($canon)),
            tag: $tag,
        }
    };
    ($src:expr, $tag:expr, note = $note:expr) => {{
        let _ = $note;
        Case {
            src: $src,
            expect: Expect::Accept(None),
            tag: $tag,
        }
    }};
}

macro_rules! rej {
    ($src:expr, $tag:expr) => {
        Case {
            src: $src,
            expect: Expect::Reject,
            tag: $tag,
        }
    };
    ($src:expr, $tag:expr, note = $note:expr) => {{
        let _ = $note;
        Case {
            src: $src,
            expect: Expect::Reject,
            tag: $tag,
        }
    }};
}

const TAGS: &[&str] = &[
    "literals",
    "paths",
    "precedence",
    "operators",
    "ternary",
    "calls",
    "golden",
    "whitespace",
    "malformed",
    "chaining",
    "numbers",
    "assignment",
    "js-isms",
    "cel-isms",
    "identifiers",
    "semantic",
];

fn cases() -> Vec<Case> {
    vec![
        // ---- literals -----------------------------------------------------
        acc!("null", "literals", canon = "null"),
        acc!("true", "literals", canon = "true"),
        acc!("false", "literals", canon = "false"),
        acc!("0", "literals", canon = "0.0"),
        acc!("3", "literals", canon = "3.0"),
        acc!("3.5", "literals", canon = "3.5"),
        acc!("0.05", "literals", canon = "0.05"),
        acc!("300", "literals", canon = "300.0"),
        acc!("1e3", "literals", canon = "1000.0"),
        acc!("1.5e-3", "literals", canon = "0.0015"),
        acc!("2E+2", "literals", canon = "200.0"),
        acc!(
            "9223372036854775808",
            "literals",
            note = "i64::MAX+1 — probes i64-first number pipelines"
        ),
        acc!("'end_of_session'", "literals", canon = "\"end_of_session\""),
        acc!("\"double quoted\"", "literals", canon = "\"double quoted\""),
        acc!(r"'it\'s dark'", "literals", canon = "\"it's dark\""),
        acc!(r"'a\tb'", "literals", canon = "\"a\\tb\""),
        acc!(r"'line\nbreak'", "literals", canon = "\"line\\nbreak\""),
        acc!(r"'café'", "literals", canon = "\"café\""),
        acc!("'filter: Hα'", "literals", note = "raw unicode inside strings is fine"),

        // ---- paths --------------------------------------------------------
        acc!("params.camera_id", "paths", canon = "(. params camera_id)"),
        acc!("session.target_adu", "paths"),
        acc!("result.target.ra_hours", "paths", canon = "(. (. result target) ra_hours)"),
        acc!("event.time_to_flip_seconds", "paths"),
        acc!("error.message", "paths"),
        acc!("params._recovery.reason", "paths"),
        acc!("session._once", "paths", note = "write-reserved, read is grammatical"),
        acc!("result.items[0]", "paths", canon = "([] (. result items) 0.0)"),
        acc!("result.map['key']", "paths", canon = "([] (. result map) \"key\")"),
        acc!("session.filters[params.idx]", "paths"),
        acc!("result.items[0].name", "paths", canon = "(. ([] (. result items) 0.0) name)"),
        acc!("result.items.length", "paths", note = "`length` is just a field name"),
        acc!(
            "session.in",
            "paths",
            note = "`in` is not reserved in this grammar — probes CEL's reserved words"
        ),

        // ---- operators & precedence ---------------------------------------
        acc!("1 + 2 * 3", "precedence", canon = "(+ 1.0 (* 2.0 3.0))"),
        acc!("(1 + 2) * 3", "precedence", canon = "(* (+ 1.0 2.0) 3.0)"),
        acc!("10 - 4 - 3", "precedence", canon = "(- (- 10.0 4.0) 3.0)"),
        acc!("10 / 5 / 2", "precedence", canon = "(/ (/ 10.0 5.0) 2.0)"),
        acc!("1 + 2 - 3", "precedence", canon = "(- (+ 1.0 2.0) 3.0)"),
        acc!("2 * 3 % 4", "precedence", canon = "(% (* 2.0 3.0) 4.0)"),
        acc!(
            "session.frame % params.dither_every == 0",
            "precedence",
            canon = "(== (% (. session frame) (. params dither_every)) 0.0)"
        ),
        acc!("-session.offset", "operators", canon = "(neg (. session offset))"),
        acc!("- -3", "operators", note = "spaced double negation folds to 3.0"),
        acc!("-3", "operators", canon = "-3.0"),
        acc!("!session.imaging", "operators", canon = "(! (. session imaging))"),
        acc!("!!true", "operators", canon = "(! (! true))"),
        acc!("!(session.a && session.b)", "operators"),
        acc!(
            "session.a && session.b || session.c",
            "precedence",
            canon = "(|| (&& (. session a) (. session b)) (. session c))"
        ),
        acc!(
            "session.a || session.b && session.c",
            "precedence",
            canon = "(|| (. session a) (&& (. session b) (. session c)))"
        ),
        acc!(
            "session.a + session.b <= session.c + session.d",
            "precedence",
            canon = "(<= (+ (. session a) (. session b)) (+ (. session c) (. session d)))"
        ),
        acc!(
            "!has(session.x) && session.y > 0",
            "precedence",
            canon = "(&& (! (call has (. session x))) (> (. session y) 0.0))"
        ),
        acc!("result.hfr != null", "operators", canon = "(!= (. result hfr) null)"),
        acc!(
            "(session.a == session.b) == session.c",
            "precedence",
            canon = "(== (== (. session a) (. session b)) (. session c))"
        ),

        // ---- ternary ------------------------------------------------------
        acc!(
            "session.a ? session.b : session.c",
            "ternary",
            canon = "(?: (. session a) (. session b) (. session c))"
        ),
        acc!(
            "session.a ? 1 : session.b ? 2 : 3",
            "ternary",
            canon = "(?: (. session a) 1.0 (?: (. session b) 2.0 3.0))"
        ),
        acc!("clamp(session.x > 5 ? 1 : 2, 0, 10)", "ternary"),

        // ---- calls --------------------------------------------------------
        acc!("abs(session.hfr)", "calls", canon = "(call abs (. session hfr))"),
        acc!("min(session.a, session.b)", "calls"),
        acc!("max(1, 2)", "calls"),
        acc!("min(1, 2, 3)", "calls", note = "min/max pinned variadic >= 2"),
        acc!("clamp(session.x, session.lo, session.hi)", "calls"),
        acc!("floor(1.5)", "calls"),
        acc!("ceil(1.5)", "calls"),
        acc!("round(1.5)", "calls"),
        acc!("seconds('1m30s')", "calls", canon = "(call seconds \"1m30s\")"),
        acc!("humantime(session.duration)", "calls"),
        acc!("has(session.last_focus_hfr)", "calls"),
        acc!("has(result.target.name)", "calls"),
        acc!("has(result.items[0].name)", "calls"),
        acc!("seconds_until('2026-07-03T04:12:00Z')", "calls"),
        acc!("seconds_until(session.flip_at) <= 0", "calls"),

        // ---- golden expressions from the design doc -----------------------
        acc!(
            "abs(session.median_adu - session.target_adu) / session.target_adu <= params.tolerance",
            "golden"
        ),
        acc!(
            "clamp(result.median_adu == 0 ? session.duration * 2 : session.duration * (session.target_adu / result.median_adu), session.exp_min, session.exp_max)",
            "golden"
        ),
        acc!("event.hfr > session.last_focus_hfr * 1.2", "golden"),
        acc!("session.imaging == true", "golden"),
        acc!("event.time_to_flip_seconds < 300", "golden"),
        acc!("result.reason == 'end_of_session'", "golden"),
        acc!("result.target == null", "golden"),
        acc!("session.session_over != true", "golden"),
        acc!("result.converged == false", "golden"),
        acc!("'exposure never converged'", "golden"),
        acc!("seconds(params.initial_duration)", "golden"),
        acc!(
            "result.hfr != null && result.hfr > session.last_focus_hfr * 1.2",
            "golden",
            canon = "(&& (!= (. result hfr) null) (> (. result hfr) (* (. session last_focus_hfr) 1.2)))"
        ),

        // ---- whitespace ---------------------------------------------------
        acc!("  1 + 2  ", "whitespace"),
        acc!("1\n  +\n  2", "whitespace", note = "newlines OK — JSON strings can embed \\n"),
        acc!("((((1))))", "whitespace", canon = "1.0"),

        // ---- malformed ----------------------------------------------------
        rej!("", "malformed"),
        rej!("   ", "malformed"),
        rej!("1 +", "malformed"),
        rej!("* 3", "malformed"),
        rej!("(1 + 2", "malformed"),
        rej!("1 + 2)", "malformed"),
        rej!("abs(session.hfr", "malformed"),
        rej!("min(1,)", "malformed", note = "JS allows trailing commas in calls"),
        rej!("min(,1)", "malformed"),
        rej!("min(1 2)", "malformed"),
        rej!("session.", "malformed"),
        rej!("result..hfr", "malformed"),
        rej!(".foo", "malformed"),
        rej!("session.a session.b", "malformed"),
        rej!("1 2", "malformed"),
        rej!("session.x ? 1", "malformed"),
        rej!("session.x ?: 1", "malformed", note = "no elvis operator"),
        rej!("'unterminated", "malformed"),
        rej!(r"'bad \q escape'", "malformed", note = "unknown escape must not pass silently"),
        rej!("\"mixed'", "malformed"),

        // ---- comparison chaining (pinned: parse error) ---------------------
        rej!("session.a < session.b < session.c", "chaining"),
        rej!("session.a == session.b == session.c", "chaining"),
        rej!("session.a == session.b < session.c", "chaining",
             note = "CEL groups (a==b)<c, JS groups a==(b<c) — pinned to reject instead"),
        rej!("1 <= session.x <= 10", "chaining"),

        // ---- number lexical rules ------------------------------------------
        rej!(".5", "numbers", note = "pinned: JSON number syntax — leading digit required"),
        rej!("5.", "numbers", note = "pinned: JSON number syntax — digits after point required"),
        rej!("0x1F", "numbers"),
        rej!("0b101", "numbers"),
        rej!("0o17", "numbers"),
        rej!("1_000", "numbers"),
        rej!("1n", "numbers"),
        rej!("007", "numbers", note = "leading zeros rejected (JS parses legacy octal!)"),
        rej!("1e999", "numbers", note = "overflows f64 — literal must be finite"),
        rej!("+1", "numbers", note = "no unary plus"),

        // ---- assignment ----------------------------------------------------
        rej!("session.x = 1", "assignment"),
        rej!("session.x += 1", "assignment"),
        rej!("session.a == session.b = session.c", "assignment"),

        // ---- JS-isms that must not leak in ---------------------------------
        rej!("session.a ?? session.b", "js-isms"),
        rej!("session.a?.b", "js-isms"),
        rej!("x => x", "js-isms"),
        rej!("`tpl ${session.x}`", "js-isms"),
        rej!("[1, 2, 3]", "js-isms", note = "no array literals — arrays only from tool results"),
        rej!("({a: 1})", "js-isms", note = "no object literals"),
        rej!("typeof session.x", "js-isms"),
        rej!("session.x instanceof session.y", "js-isms"),
        rej!("new Foo()", "js-isms"),
        rej!("session.a, session.b", "js-isms", note = "no comma operator"),
        rej!("session.x ** 2", "js-isms"),
        rej!("session.x & 1", "js-isms"),
        rej!("session.x | 1", "js-isms"),
        rej!("session.x ^ 1", "js-isms"),
        rej!("~session.x", "js-isms"),
        rej!("session.x << 2", "js-isms"),
        rej!("void session.x", "js-isms"),
        rej!("delete session.x", "js-isms"),
        rej!("this.x", "js-isms"),
        rej!("session.a === session.b", "js-isms", note = "want a 'use ==' hint"),
        rej!("session.a !== session.b", "js-isms"),
        rej!("1 // comment", "js-isms", note = "no comments"),
        rej!("1 /* c */ + 2", "js-isms"),
        rej!("await session.x", "js-isms"),
        rej!("session.x++", "js-isms"),
        rej!("--session.x", "js-isms", note = "JS reads this as pre-decrement; rejected outright"),
        rej!("1; 2", "js-isms"),

        // ---- CEL-isms that must not leak in ---------------------------------
        rej!("session.list.map(x, x * 2)", "cel-isms", note = "no comprehensions"),
        rej!("session.list.filter(x, x > 0)", "cel-isms"),
        rej!("session.list.all(x, x > 0)", "cel-isms"),
        rej!("session.list.exists(x, x > 0)", "cel-isms"),
        rej!("1 in session.list", "cel-isms"),
        rej!("session.x.size()", "cel-isms", note = "no method calls at all"),
        rej!("result.items.length()", "cel-isms"),
        rej!("b'bytes'", "cel-isms"),
        rej!("r'raw'", "cel-isms"),
        rej!("1u", "cel-isms"),

        // ---- identifier rules ------------------------------------------------
        rej!("café + 1", "identifiers", note = "ASCII identifiers only"),
        rej!("$x + 1", "identifiers"),
        rej!("session.$ra", "identifiers"),
        rej!("session.née", "identifiers"),
        rej!("result.null", "identifiers", note = "reserved word as field — use ['null']"),

        // ---- static-check layer ---------------------------------------------
        rej!("x + 1", "semantic", note = "unknown namespace root"),
        rej!("abs", "semantic", note = "function name in value position"),
        rej!("foo(1)", "semantic", note = "unknown function"),
        rej!("abs(1, 2)", "semantic", note = "arity"),
        rej!("clamp(1)", "semantic", note = "arity"),
        rej!("min()", "semantic", note = "arity"),
        rej!("min(1)", "semantic", note = "arity"),
        rej!("has(1)", "semantic", note = "has() needs a path"),
        rej!("has('x')", "semantic", note = "has() needs a path"),
        rej!("has(session.x > 1)", "semantic", note = "has() needs a path"),
        rej!("params(1)", "semantic", note = "namespace used as function"),
        rej!("duration('1h')", "semantic", note = "CEL builtin, not in our set"),
        rej!("timestamp('2026-01-01T00:00:00Z')", "semantic", note = "CEL builtin"),
        rej!("type(session.x)", "semantic", note = "CEL builtin"),
        rej!("size(session.list)", "semantic", note = "CEL builtin"),
    ]
}

/// Run every corpus case with the given tag; panic with a failure list if
/// any case deviates from its expectation.
fn run_tag(tag: &str) {
    let mut failures = Vec::new();
    let mut seen = 0usize;
    for case in cases() {
        if case.tag != tag {
            continue;
        }
        seen += 1;
        let outcome = Expression::parse(case.src);
        match (&case.expect, &outcome) {
            (Expect::Accept(None), Ok(_)) => {}
            (Expect::Accept(Some(canon)), Ok(e)) => {
                let got = e.canon();
                if got != *canon {
                    failures.push(format!(
                        "  {:?}: canon mismatch\n    expected {canon}\n    got      {got}",
                        case.src
                    ));
                }
            }
            (Expect::Accept(_), Err(e)) => {
                failures.push(format!("  {:?}: expected accept, got error: {e}", case.src));
            }
            (Expect::Reject, Ok(e)) => {
                failures.push(format!(
                    "  {:?}: expected reject, parsed as {}",
                    case.src,
                    e.canon()
                ));
            }
            (Expect::Reject, Err(e)) => {
                if e.kind != ErrorKind::Parse {
                    failures.push(format!(
                        "  {:?}: rejected with kind {:?}, expected Parse",
                        case.src, e.kind
                    ));
                }
                // Spans must be valid slice boundaries in the source —
                // /validate consumers will slice with them.
                if case.src.get(e.span.start..e.span.end).is_none() {
                    failures.push(format!(
                        "  {:?}: error span {}..{} is not a valid slice of the source",
                        case.src, e.span.start, e.span.end
                    ));
                }
            }
        }
    }
    assert!(seen > 0, "no corpus cases with tag {tag}");
    assert!(
        failures.is_empty(),
        "corpus tag `{tag}` deviations:\n{}",
        failures.join("\n")
    );
}

#[test]
fn test_corpus_covers_every_tag_and_no_stray_tags() {
    for tag in TAGS {
        assert!(
            cases().iter().any(|c| c.tag == *tag),
            "tag {tag} has no cases"
        );
    }
    for case in cases() {
        assert!(
            TAGS.contains(&case.tag),
            "stray tag {} on {:?}",
            case.tag,
            case.src
        );
    }
}

#[test]
fn test_corpus_literals() {
    run_tag("literals");
}

#[test]
fn test_corpus_paths() {
    run_tag("paths");
}

#[test]
fn test_corpus_precedence() {
    run_tag("precedence");
}

#[test]
fn test_corpus_operators() {
    run_tag("operators");
}

#[test]
fn test_corpus_ternary() {
    run_tag("ternary");
}

#[test]
fn test_corpus_calls() {
    run_tag("calls");
}

#[test]
fn test_corpus_golden() {
    run_tag("golden");
}

#[test]
fn test_corpus_whitespace() {
    run_tag("whitespace");
}

#[test]
fn test_corpus_malformed() {
    run_tag("malformed");
}

#[test]
fn test_corpus_chaining() {
    run_tag("chaining");
}

#[test]
fn test_corpus_numbers() {
    run_tag("numbers");
}

#[test]
fn test_corpus_assignment() {
    run_tag("assignment");
}

#[test]
fn test_corpus_js_isms() {
    run_tag("js-isms");
}

#[test]
fn test_corpus_cel_isms() {
    run_tag("cel-isms");
}

#[test]
fn test_corpus_identifiers() {
    run_tag("identifiers");
}

#[test]
fn test_corpus_semantic() {
    run_tag("semantic");
}

// ---- the 2 a.m. error-message battery ----------------------------------
// Realistic authoring mistakes must all fail, and the marquee diagnostics
// are pinned so a refactor can't silently degrade them.

#[test]
fn test_battery_all_error() {
    let battery = [
        "session.frames >= params.count &&",
        "abs(session.hfr",
        "result.reason == 'end_of_session",
        "session.a === session.b",
        "result..hfr",
        "session.x ? 1",
        "1_000 + 2",
        "session.a < session.b < session.c",
        "[1, 2, 3]",
        "session.list.map(x, x * 2)",
        "session.x = 1",
        "foo(session.x)",
        "clamp(1)",
        "x + 1",
    ];
    for src in battery {
        let err = Expression::parse(src).unwrap_err();
        assert!(!err.message.is_empty(), "empty diagnostic for {src:?}");
        assert!(err.span.end >= err.span.start, "bad span for {src:?}");
    }
}

#[test]
fn test_diagnostic_strict_equality_hint() {
    let err = Expression::parse("session.a === session.b").unwrap_err();
    assert!(err.message.contains("use `==`"), "got: {}", err.message);
}

#[test]
fn test_diagnostic_assignment_hint() {
    let err = Expression::parse("session.x = 1").unwrap_err();
    assert!(
        err.message.contains("did you mean `==`"),
        "got: {}",
        err.message
    );
    assert!(
        err.message.contains("`set` instruction"),
        "got: {}",
        err.message
    );
}

#[test]
fn test_diagnostic_chaining_hint() {
    let err = Expression::parse("session.a < session.b < session.c").unwrap_err();
    assert!(
        err.message.contains("cannot be chained"),
        "got: {}",
        err.message
    );
}

#[test]
fn test_diagnostic_nullish_coalescing_hint() {
    let err = Expression::parse("session.a ?? session.b").unwrap_err();
    assert!(err.message.contains("has("), "got: {}", err.message);
}

#[test]
fn test_diagnostic_unknown_function_lists_alternatives() {
    let err = Expression::parse("foo(1)").unwrap_err();
    assert!(err.message.contains("available:"), "got: {}", err.message);
    assert!(
        err.message.contains("seconds_until"),
        "got: {}",
        err.message
    );
}

#[test]
fn test_diagnostic_method_call_names_builtins() {
    let err = Expression::parse("session.x.size()").unwrap_err();
    assert!(
        err.message.contains("method calls are not supported"),
        "got: {}",
        err.message
    );
}

// ---- recursion guard -------------------------------------------------------
// Unbounded nesting depth must be a parse error, not a stack overflow;
// the `expr_parse` fuzz target drives the same path.

#[test]
fn test_deep_paren_nesting_is_an_error_not_a_crash() {
    let src = format!("{}1{}", "(".repeat(10_000), ")".repeat(10_000));
    let err = Expression::parse(&src).unwrap_err();
    assert!(
        err.message.contains("nested too deeply"),
        "got: {}",
        err.message
    );
}

#[test]
fn test_deep_unary_run_is_an_error_not_a_crash() {
    let src = format!("{}true", "!".repeat(10_000));
    let err = Expression::parse(&src).unwrap_err();
    assert!(
        err.message.contains("nested too deeply"),
        "got: {}",
        err.message
    );
}

#[test]
fn test_deep_call_nesting_is_an_error_not_a_crash() {
    let src = format!("{}1{}", "abs(".repeat(10_000), ")".repeat(10_000));
    let err = Expression::parse(&src).unwrap_err();
    assert!(
        err.message.contains("nested too deeply"),
        "got: {}",
        err.message
    );
}

#[test]
fn test_reasonable_nesting_is_fine() {
    let src = format!("{}1{}", "(".repeat(30), ")".repeat(30));
    Expression::parse(&src).unwrap();
}

// ---- public error API ----------------------------------------------------

#[test]
fn test_parse_error_kind_and_display() {
    let err = Expression::parse("1 +").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Parse);
    let shown = err.to_string();
    assert!(shown.contains("(at "), "got: {shown}");
}

#[test]
fn test_error_serializes_with_span_and_kind() {
    let err = Expression::parse("007").unwrap_err();
    let v = serde_json::to_value(&err).unwrap();
    assert_eq!(v["kind"], "parse");
    assert!(v["span"]["start"].is_u64());
    assert!(v["span"]["end"].is_u64());
    assert!(v["message"].is_string());
}

#[test]
fn test_error_span_covers_the_whole_utf8_char() {
    // `é` occupies bytes 3..5; the span must cover the full character so
    // it stays a valid slice boundary (a bare start+1 would split it).
    let src = "café + 1";
    let err = Expression::parse(src).unwrap_err();
    assert_eq!((err.span.start, err.span.end), (3, 5));
    // `get` rather than a slice: it *returns* the boundary check the
    // assertion is about, instead of panicking when it fails.
    assert_eq!(src.get(err.span.start..err.span.end), Some("é"));
}

#[test]
fn test_expression_display_and_source_are_verbatim() {
    let e = Expression::parse("  1 + 2  ").unwrap();
    assert_eq!(e.source(), "  1 + 2  ");
    assert_eq!(e.to_string(), "  1 + 2  ");
}

#[test]
fn test_namespaces_reports_exactly_the_roots_read() {
    let cases: &[(&str, &[&str])] = &[
        ("1 + 2", &[]),
        ("params.camera_id", &["params"]),
        (
            "has(session.x) && session.x > event.hfr ? result.a : error.message",
            &["error", "event", "result", "session"],
        ),
        // Roots inside index expressions and call arguments count too.
        ("session[params.key]", &["params", "session"]),
        (
            "abs(result.median_adu - session.target_adu)",
            &["result", "session"],
        ),
        // A namespace-named field is not a root: `session.params` reads
        // only `session`.
        ("session.params", &["session"]),
    ];
    for (src, want) in cases {
        let expr = Expression::parse(src).unwrap();
        let got: Vec<&str> = expr.namespaces().into_keys().collect();
        assert_eq!(got, *want, "namespaces of {src:?}");
    }
}
