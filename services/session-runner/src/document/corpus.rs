//! The document-validation corpus, shared by two suites:
//!
//! - `validate_tests` runs every case through `Document::parse` and
//!   checks the verdict (and, for rejections, that each expected
//!   pointer/message pair is reported);
//! - `schema_agreement_tests` runs every case through the published
//!   `workflow-v1.schema.json` and enforces the one-directional
//!   agreement: anything the validator accepts must pass the schema
//!   (the validator is deliberately *stronger* than the schema — it
//!   also enforces uniqueness, expression grammar, duration semantics,
//!   `$expr` placement, and namespace scoping, which the schema cannot
//!   express).

use serde_json::json;

pub(super) struct Case {
    pub name: &'static str,
    pub src: String,
    pub expect: Expect,
}

pub(super) enum Expect {
    Valid,
    /// The document must be rejected, and for each listed pair an issue
    /// with exactly that pointer and a message containing the fragment
    /// must be reported.
    Invalid(&'static [(&'static str, &'static str)]),
}

fn valid(name: &'static str, src: serde_json::Value) -> Case {
    Case {
        name,
        src: src.to_string(),
        expect: Expect::Valid,
    }
}

fn invalid(
    name: &'static str,
    src: serde_json::Value,
    wants: &'static [(&'static str, &'static str)],
) -> Case {
    Case {
        name,
        src: src.to_string(),
        expect: Expect::Invalid(wants),
    }
}

/// Wraps a root instruction in a minimal valid document.
fn doc(root: serde_json::Value) -> serde_json::Value {
    json!({ "version": 1, "name": "t", "root": root })
}

#[allow(clippy::too_many_lines)]
pub(super) fn cases() -> Vec<Case> {
    vec![
        // ---- valid documents ------------------------------------------
        valid("minimal", doc(json!({ "log": { "message": "hello" } }))),
        valid("root_is_a_bare_tool", doc(json!({ "tool": "park" }))),
        valid("empty_root_sequence", doc(json!({ "sequence": [] }))),
        valid(
            "description_and_durations",
            json!({
                "version": 1, "name": "t", "description": "a test document",
                "estimated_duration": "1h30m", "max_duration": "12h",
                "root": { "sequence": [] }
            }),
        ),
        valid(
            "parameters_of_every_type",
            json!({
                "version": 1, "name": "t",
                "parameters": {
                    "cam": { "type": "string", "required": true },
                    "count": { "type": "integer", "default": 3 },
                    "fraction": { "type": "number", "default": 0.5 },
                    "guide": { "type": "boolean", "default": true },
                    "initial": { "type": "duration", "default": "1.5s" },
                    "filters": { "type": "array", "default": [{ "name": "L", "count": 5 }] }
                },
                "root": { "sequence": [] }
            }),
        ),
        valid(
            "tool_with_literal_and_expr_args_and_retry",
            doc(json!({
                "tool": "capture",
                "args": {
                    "camera_id": { "$expr": "params.camera_id" },
                    "duration": "300s",
                    "roi": { "x": 0, "y": 0, "w": 100, "h": 100 },
                    "filters": ["Ha", "OIII"]
                },
                "retry": { "max_attempts": 3, "backoff": "10s" },
                "id": "main-capture", "once": "first-capture"
            })),
        ),
        valid(
            "repeat_until_with_expr_bound",
            doc(json!({
                "repeat": {
                    "until": "abs(session.median_adu - session.target_adu) / session.target_adu <= 0.05",
                    "max_iterations": { "$expr": "params.max_iterations" }
                },
                "body": [ { "tool": "capture" } ]
            })),
        ),
        valid(
            "repeat_while",
            doc(json!({
                "repeat": { "while": "session.session_over != true", "max_iterations": 1000 },
                "body": [ { "tool": "get_next_target" } ]
            })),
        ),
        valid(
            "repeat_count_with_optional_max_iterations",
            doc(json!({
                "repeat": { "count": { "$expr": "params.count" }, "max_iterations": 500 },
                "body": [ { "tool": "capture" } ]
            })),
        ),
        valid(
            "repeat_count_zero",
            doc(json!({ "repeat": { "count": 0 }, "body": [ { "tool": "capture" } ] })),
        ),
        valid(
            "if_then_else",
            doc(json!({
                "if": "result.converged == false",
                "then": [ { "log": { "level": "info", "message": "did not converge" } } ],
                "else": [ { "log": { "message": "converged" } } ]
            })),
        ),
        valid(
            "set_multiple_disjoint_keys",
            doc(json!({
                "set": {
                    "session.target_adu": "result.max_adu * params.target_fraction",
                    "session.exp_min": "result.exposure_min",
                    "session.report.frames": "0"
                }
            })),
        ),
        valid(
            "try_catch_finally_reads_error",
            doc(json!({
                "try": [ { "tool": "capture" } ],
                "catch": [
                    { "log": { "message": "capture failed", "values": { "why": "error.message", "tool": "error.tool" } } }
                ],
                "finally": [ { "tool": "calibrator_off", "args": { "calibrator_id": "flat-panel" } } ]
            })),
        ),
        valid("bare_try", doc(json!({ "try": [ { "tool": "park" } ] }))),
        valid(
            "fail_with_expression_message",
            doc(json!({ "fail": { "message": "'exposure never converged'" } })),
        ),
        valid(
            "wait_variants",
            doc(json!({ "sequence": [
                { "wait": { "duration": "30s" } },
                { "wait": { "until_event": "guide_settled", "timeout": "5m" } },
                { "wait": { "until": "seconds_until(session.flip_at) <= 0", "poll_interval": "10s", "timeout": "2h" } },
                { "wait": { "until": "session.ready == true", "timeout": "1m" } }
            ] })),
        ),
        valid(
            "log_levels_and_values",
            doc(json!({ "log": {
                "level": "info", "message": "exposure converged",
                "values": { "duration": "session.duration" }
            } })),
        ),
        valid(
            "event_trigger_with_gates",
            json!({
                "version": 1, "name": "t", "root": { "sequence": [] },
                "triggers": [ {
                    "id": "refocus-on-hfr",
                    "on": { "event": "exposure_complete" },
                    "when": "has(session.last_focus_hfr) && event.document_id != null",
                    "while": "session.imaging == true",
                    "cooldown": "15m",
                    "once": false,
                    "do": [
                        { "tool": "measure_basic", "args": { "document_id": { "$expr": "event.document_id" } } },
                        { "if": "result.hfr != null && result.hfr > session.last_focus_hfr * 1.2",
                          "then": [ { "set": { "session.last_focus_hfr": "result.hfr" } } ] }
                    ]
                } ]
            }),
        ),
        valid(
            "poll_trigger",
            json!({
                "version": 1, "name": "t", "root": { "sequence": [] },
                "parameters": { "mount_id": { "type": "string", "required": true } },
                "triggers": [ {
                    "id": "flip-when-due",
                    "on": { "poll": { "tool": "get_meridian_status",
                                      "args": { "mount_id": { "$expr": "params.mount_id" } },
                                      "interval": "60s" } },
                    "when": "event.time_to_flip_seconds < 300",
                    "do": [ { "log": { "message": "flip due" } } ]
                } ]
            }),
        ),
        valid(
            "correction_trigger_and_once",
            json!({
                "version": 1, "name": "t", "root": { "sequence": [] },
                "triggers": [ {
                    "id": "handle-correction",
                    "on": { "event": "correction_requested" },
                    "once": true,
                    "do": [ { "log": { "message": "correction", "values": { "action": "event.action" } } } ]
                } ]
            }),
        ),
        valid("shipped_deep_sky", golden_deep_sky()),
        valid("shipped_sky_flat", golden_sky_flat()),
        // ---- invalid: document level -----------------------------------
        invalid(
            "document_not_an_object",
            json!([1, 2]),
            &[("", "must be a JSON object")],
        ),
        invalid(
            "missing_version",
            json!({ "name": "t", "root": { "sequence": [] } }),
            &[("", "missing required key `version`")],
        ),
        invalid(
            "unsupported_version",
            json!({ "version": 2, "name": "t", "root": { "sequence": [] } }),
            &[("/version", "unsupported document version 2")],
        ),
        invalid(
            "fractional_version",
            json!({ "version": 1.5, "name": "t", "root": { "sequence": [] } }),
            &[("/version", "unsupported document version 1.5")],
        ),
        invalid(
            "missing_name_and_root",
            json!({ "version": 1 }),
            &[
                ("", "missing required key `name`"),
                ("", "missing required key `root`"),
            ],
        ),
        invalid(
            "empty_name",
            json!({ "version": 1, "name": "", "root": { "sequence": [] } }),
            &[("/name", "non-empty string")],
        ),
        invalid(
            "unknown_top_level_key",
            json!({ "version": 1, "name": "t", "root": { "sequence": [] }, "trigger": [] }),
            &[("/trigger", "unknown key `trigger` in a workflow document")],
        ),
        invalid(
            "description_not_a_string",
            json!({ "version": 1, "name": "t", "description": 3, "root": { "sequence": [] } }),
            &[("/description", "must be a string")],
        ),
        invalid(
            "bad_top_level_duration",
            json!({ "version": 1, "name": "t", "estimated_duration": "soon",
                    "root": { "sequence": [] } }),
            &[("/estimated_duration", "not a valid duration")],
        ),
        invalid(
            "duration_outside_published_pattern",
            json!({ "version": 1, "name": "t", "max_duration": "1day",
                    "root": { "sequence": [] } }),
            &[("/max_duration", "document duration format")],
        ),
        // ---- invalid: parameters ---------------------------------------
        invalid(
            "parameters_not_an_object",
            json!({ "version": 1, "name": "t", "parameters": [], "root": { "sequence": [] } }),
            &[("/parameters", "must be a JSON object")],
        ),
        invalid(
            "reserved_parameter_name",
            json!({ "version": 1, "name": "t",
                    "parameters": { "_recovery": { "type": "string", "required": true } },
                    "root": { "sequence": [] } }),
            &[("/parameters/_recovery", "reserved for the engine")],
        ),
        invalid(
            "parameter_name_starts_with_digit",
            json!({ "version": 1, "name": "t",
                    "parameters": { "1st": { "type": "string", "required": true } },
                    "root": { "sequence": [] } }),
            &[("/parameters/1st", "not a valid parameter name")],
        ),
        invalid(
            "parameter_decl_unknown_key",
            json!({ "version": 1, "name": "t",
                    "parameters": { "cam": { "type": "string", "required": true, "doc": "x" } },
                    "root": { "sequence": [] } }),
            &[("/parameters/cam/doc", "unknown key `doc`")],
        ),
        invalid(
            "parameter_unknown_type",
            json!({ "version": 1, "name": "t",
                    "parameters": { "cam": { "type": "text", "required": true } },
                    "root": { "sequence": [] } }),
            &[("/parameters/cam/type", "unknown parameter type `text`")],
        ),
        invalid(
            "parameter_required_false",
            json!({ "version": 1, "name": "t",
                    "parameters": { "cam": { "type": "string", "required": false } },
                    "root": { "sequence": [] } }),
            &[("/parameters/cam/required", "`required` must be `true`")],
        ),
        invalid(
            "parameter_required_and_default",
            json!({ "version": 1, "name": "t",
                    "parameters": { "n": { "type": "integer", "required": true, "default": 3 } },
                    "root": { "sequence": [] } }),
            &[("/parameters/n", "not both")],
        ),
        invalid(
            "parameter_neither_required_nor_default",
            json!({ "version": 1, "name": "t",
                    "parameters": { "n": { "type": "integer" } },
                    "root": { "sequence": [] } }),
            &[("/parameters/n", "either `required: true` or a `default`")],
        ),
        invalid(
            "integer_default_is_a_float",
            json!({ "version": 1, "name": "t",
                    "parameters": { "n": { "type": "integer", "default": 3.5 } },
                    "root": { "sequence": [] } }),
            &[("/parameters/n/default", "expected an `integer` value")],
        ),
        invalid(
            "array_default_is_not_an_array",
            json!({ "version": 1, "name": "t",
                    "parameters": { "filters": { "type": "array", "default": { "name": "L" } } },
                    "root": { "sequence": [] } }),
            &[("/parameters/filters/default", "expected an `array` value")],
        ),
        invalid(
            "duration_default_is_not_a_duration",
            json!({ "version": 1, "name": "t",
                    "parameters": { "d": { "type": "duration", "default": "fast" } },
                    "root": { "sequence": [] } }),
            &[("/parameters/d/default", "not a valid duration")],
        ),
        // ---- invalid: instruction shape --------------------------------
        invalid(
            "root_not_an_object",
            doc(json!("park")),
            &[("/root", "an instruction must be a JSON object")],
        ),
        invalid(
            "empty_instruction",
            doc(json!({})),
            &[("/root", "not an instruction")],
        ),
        invalid(
            "misspelled_discriminant",
            doc(json!({ "toool": "park" })),
            &[("/root", "found keys `toool`")],
        ),
        invalid(
            "two_discriminants",
            doc(json!({ "tool": "park", "wait": { "duration": "1s" } })),
            &[("/root", "exactly one discriminant key")],
        ),
        invalid(
            "script_is_reserved",
            doc(json!({ "script": "return 1" })),
            &[("/root/script", "reserved for a future format version")],
        ),
        // The reservation message is for well-formed future-version
        // documents (where `script` is the sole discriminant). Alongside
        // another discriminant the object is malformed under *every*
        // format version, so the accurate diagnosis is the generic
        // exactly-one error naming both keys — not a version mismatch.
        invalid(
            "script_alongside_another_discriminant",
            doc(json!({ "tool": "x", "script": "return 1" })),
            &[(
                "/root",
                "exactly one discriminant key; found `tool`, `script`",
            )],
        ),
        invalid(
            "unknown_key_in_tool_instruction",
            doc(json!({ "tool": "capture", "retyr": { "max_attempts": 3, "backoff": "1s" } })),
            &[("/root/retyr", "unknown key `retyr` in a `tool` instruction")],
        ),
        invalid(
            "empty_id_and_once",
            doc(json!({ "tool": "park", "id": "", "once": "" })),
            &[
                ("/root/id", "non-empty string"),
                ("/root/once", "non-empty string"),
            ],
        ),
        invalid(
            "duplicate_once_keys",
            doc(json!({ "sequence": [
                { "tool": "calibrator_on", "once": "panel-on" },
                { "tool": "calibrator_on", "once": "panel-on" }
            ] })),
            &[("/root/sequence/1/once", "duplicate `once` key `panel-on`")],
        ),
        // ---- invalid: tool ----------------------------------------------
        invalid(
            "tool_name_empty",
            doc(json!({ "tool": "" })),
            &[("/root/tool", "non-empty string")],
        ),
        invalid(
            "args_not_an_object",
            doc(json!({ "tool": "capture", "args": [1] })),
            &[("/root/args", "`args` must be a JSON object")],
        ),
        invalid(
            "retry_missing_backoff",
            doc(json!({ "tool": "capture", "retry": { "max_attempts": 3 } })),
            &[("/root/retry", "`retry` requires a `backoff`")],
        ),
        invalid(
            "retry_zero_attempts",
            doc(json!({ "tool": "capture", "retry": { "max_attempts": 0, "backoff": "1s" } })),
            &[("/root/retry/max_attempts", "positive integer")],
        ),
        invalid(
            "retry_unknown_key",
            doc(json!({ "tool": "capture",
                        "retry": { "max_attempts": 2, "backoff": "1s", "jitter": true } })),
            &[("/root/retry/jitter", "unknown key `jitter` in `retry`")],
        ),
        invalid(
            "expr_wrapper_with_extra_key",
            doc(json!({ "tool": "capture",
                        "args": { "d": { "$expr": "session.duration", "unit": "s" } } })),
            &[("/root/args/d", "must contain only that key")],
        ),
        invalid(
            "expr_wrapper_not_a_string",
            doc(json!({ "tool": "capture", "args": { "d": { "$expr": 3 } } })),
            &[("/root/args/d/$expr", "must be an expression string")],
        ),
        invalid(
            "nested_expr_inside_literal_arg",
            doc(json!({ "tool": "slew",
                        "args": { "coords": { "ra": { "$expr": "session.ra" }, "dec": 1.5 } } })),
            &[("/root/args/coords/ra/$expr", "direct argument value")],
        ),
        invalid(
            "nested_expr_inside_literal_array",
            doc(json!({ "tool": "capture",
                        "args": { "plan": [ { "$expr": "params.filter" } ] } })),
            &[("/root/args/plan/0/$expr", "direct argument value")],
        ),
        // ---- invalid: repeat ---------------------------------------------
        invalid(
            "repeat_without_mode",
            doc(json!({ "repeat": { "max_iterations": 5 }, "body": [ { "tool": "x" } ] })),
            &[("/root/repeat", "exactly one of `until`, `while`, `count`")],
        ),
        invalid(
            "repeat_with_two_modes",
            doc(
                json!({ "repeat": { "until": "true", "count": 3, "max_iterations": 5 },
                        "body": [ { "tool": "x" } ] }),
            ),
            &[("/root/repeat", "found `until`, `count`")],
        ),
        invalid(
            "repeat_until_without_max_iterations",
            doc(json!({ "repeat": { "until": "session.done == true" },
                        "body": [ { "tool": "x" } ] })),
            &[("/root/repeat", "`max_iterations` is required with `until`")],
        ),
        invalid(
            "repeat_negative_count",
            doc(json!({ "repeat": { "count": -1 }, "body": [ { "tool": "x" } ] })),
            &[("/root/repeat/count", "non-negative integer")],
        ),
        invalid(
            "repeat_float_count",
            doc(json!({ "repeat": { "count": 3.0 }, "body": [ { "tool": "x" } ] })),
            &[("/root/repeat/count", "non-negative integer")],
        ),
        invalid(
            "repeat_zero_max_iterations",
            doc(json!({ "repeat": { "until": "true", "max_iterations": 0 },
                        "body": [ { "tool": "x" } ] })),
            &[("/root/repeat/max_iterations", "positive integer")],
        ),
        invalid(
            "repeat_missing_body",
            doc(json!({ "repeat": { "count": 3 } })),
            &[("/root", "a `repeat` instruction requires a `body`")],
        ),
        invalid(
            "repeat_empty_body",
            doc(json!({ "repeat": { "count": 3 }, "body": [] })),
            &[("/root/body", "at least one instruction")],
        ),
        invalid(
            "repeat_unknown_option",
            doc(json!({ "repeat": { "count": 3, "parallel": true }, "body": [ { "tool": "x" } ] })),
            &[(
                "/root/repeat/parallel",
                "unknown key `parallel` in `repeat`",
            )],
        ),
        // ---- invalid: if / set / try / fail ------------------------------
        invalid(
            "if_missing_then",
            doc(json!({ "if": "true" })),
            &[("/root", "an `if` instruction requires a `then`")],
        ),
        invalid(
            "if_empty_else",
            doc(json!({ "if": "true", "then": [ { "tool": "x" } ], "else": [] })),
            &[("/root/else", "at least one instruction")],
        ),
        invalid(
            "set_empty",
            doc(json!({ "set": {} })),
            &[("/root/set", "at least one entry")],
        ),
        invalid(
            "set_key_not_under_session",
            doc(json!({ "set": { "params.count": "1" } })),
            &[("/root/set/params.count", "not a valid set key")],
        ),
        invalid(
            "set_key_bare_session",
            doc(json!({ "set": { "session": "1" } })),
            &[("/root/set/session", "not a valid set key")],
        ),
        invalid(
            "set_key_reserved_engine_state",
            doc(json!({ "set": { "session._once.x": "true" } })),
            &[("/root/set/session._once.x", "reserved engine state")],
        ),
        invalid(
            "set_key_segment_starts_with_digit",
            doc(json!({ "set": { "session.2fast": "1" } })),
            &[("/root/set/session.2fast", "not a valid set key")],
        ),
        invalid(
            "set_overlapping_keys",
            doc(json!({ "set": { "session.a": "1", "session.a.b": "2" } })),
            &[("/root/set/session.a.b", "overlap")],
        ),
        // The overlap scan must see through interleaving keys
        // (`session.a2` sorts between `session.a` and `session.a.b` as a
        // raw string) and report each entry against its nearest prefix
        // ancestor in a chain.
        invalid(
            "set_overlap_chain_with_interleaving_key",
            doc(json!({ "set": { "session.a": "1", "session.a2": "2",
                                 "session.a.b": "3", "session.a.b.c": "4" } })),
            &[
                (
                    "/root/set/session.a.b",
                    "`session.a` and `session.a.b` overlap",
                ),
                (
                    "/root/set/session.a.b.c",
                    "`session.a.b` and `session.a.b.c` overlap",
                ),
            ],
        ),
        invalid(
            "set_value_not_an_expression_string",
            doc(json!({ "set": { "session.a": 1 } })),
            &[("/root/set/session.a", "must be an expression string")],
        ),
        invalid(
            "try_empty_body",
            doc(json!({ "try": [] })),
            &[("/root/try", "at least one instruction")],
        ),
        invalid(
            "catch_empty",
            doc(json!({ "try": [ { "tool": "x" } ], "catch": [] })),
            &[("/root/catch", "at least one instruction")],
        ),
        invalid(
            "fail_without_message",
            doc(json!({ "fail": {} })),
            &[("/root/fail", "requires a `message`")],
        ),
        invalid(
            "fail_unknown_key",
            doc(json!({ "fail": { "message": "'x'", "code": 3 } })),
            &[("/root/fail/code", "unknown key `code` in `fail`")],
        ),
        // ---- invalid: wait ------------------------------------------------
        invalid(
            "wait_empty",
            doc(json!({ "wait": {} })),
            &[(
                "/root/wait",
                "exactly one of `duration`, `until_event`, `until`",
            )],
        ),
        invalid(
            "wait_two_variants",
            doc(json!({ "wait": { "duration": "1s", "until": "true", "timeout": "1m" } })),
            &[("/root/wait", "found `duration`, `until`")],
        ),
        invalid(
            "wait_duration_with_timeout",
            doc(json!({ "wait": { "duration": "1s", "timeout": "1m" } })),
            &[(
                "/root/wait/timeout",
                "not used with a fixed-`duration` wait",
            )],
        ),
        invalid(
            "wait_until_event_without_timeout",
            doc(json!({ "wait": { "until_event": "guide_settled" } })),
            &[("/root/wait", "requires a `timeout`")],
        ),
        invalid(
            "wait_until_event_with_poll_interval",
            doc(json!({ "wait": { "until_event": "x", "timeout": "1m", "poll_interval": "5s" } })),
            &[(
                "/root/wait/poll_interval",
                "only applies to an `until` wait",
            )],
        ),
        invalid(
            "wait_until_without_timeout",
            doc(json!({ "wait": { "until": "session.ready == true" } })),
            &[("/root/wait", "requires a `timeout`")],
        ),
        invalid(
            "wait_unknown_key",
            doc(json!({ "wait": { "duration": "1s", "jitter": "1s" } })),
            &[("/root/wait/jitter", "unknown key `jitter` in `wait`")],
        ),
        // ---- invalid: log --------------------------------------------------
        invalid(
            "log_missing_message",
            doc(json!({ "log": { "level": "info" } })),
            &[("/root/log", "requires a `message`")],
        ),
        invalid(
            "log_bad_level",
            doc(json!({ "log": { "level": "warn", "message": "x" } })),
            &[("/root/log/level", "must be \"debug\" or \"info\"")],
        ),
        invalid(
            "log_values_not_an_object",
            doc(json!({ "log": { "message": "x", "values": [1] } })),
            &[("/root/log/values", "must be an object of expressions")],
        ),
        // ---- invalid: expressions -----------------------------------------
        invalid(
            "expression_syntax_error",
            doc(json!({ "if": "1 +", "then": [ { "tool": "x" } ] })),
            &[("/root/if", "invalid expression")],
        ),
        invalid(
            "expression_unknown_namespace",
            doc(json!({ "if": "config.count > 1", "then": [ { "tool": "x" } ] })),
            &[("/root/if", "invalid expression")],
        ),
        invalid(
            "event_out_of_scope_in_tree",
            doc(json!({ "if": "event.hfr > 2", "then": [ { "tool": "x" } ] })),
            &[("/root/if", "`event` is not in scope here")],
        ),
        invalid(
            "error_out_of_scope_outside_catch",
            doc(json!({ "log": { "message": "x", "values": { "m": "error.message" } } })),
            &[("/root/log/values/m", "`error` is not in scope here")],
        ),
        invalid(
            "event_out_of_scope_in_poll_args",
            json!({
                "version": 1, "name": "t", "root": { "sequence": [] },
                "triggers": [ {
                    "id": "p",
                    "on": { "poll": { "tool": "status",
                                      "args": { "x": { "$expr": "event.y" } },
                                      "interval": "30s" } },
                    "do": [ { "log": { "message": "x" } } ]
                } ]
            }),
            &[(
                "/triggers/0/on/poll/args/x/$expr",
                "`event` is not in scope here",
            )],
        ),
        invalid(
            "zero_poll_interval",
            json!({
                "version": 1, "name": "t", "root": { "sequence": [] },
                "triggers": [ {
                    "id": "p",
                    "on": { "poll": { "tool": "status", "interval": "0s" } },
                    "do": [ { "log": { "message": "x" } } ]
                } ]
            }),
            &[(
                "/triggers/0/on/poll/interval",
                "a poll `interval` must be positive",
            )],
        ),
        // ---- invalid: triggers ---------------------------------------------
        invalid(
            "triggers_not_an_array",
            json!({ "version": 1, "name": "t", "triggers": {}, "root": { "sequence": [] } }),
            &[("/triggers", "must be an array")],
        ),
        invalid(
            "trigger_missing_id_and_do",
            json!({ "version": 1, "name": "t", "root": { "sequence": [] },
                    "triggers": [ { "on": { "event": "x" } } ] }),
            &[
                ("/triggers/0", "requires an `id`"),
                ("/triggers/0", "requires a `do` block"),
            ],
        ),
        invalid(
            "trigger_missing_on",
            json!({ "version": 1, "name": "t", "root": { "sequence": [] },
                    "triggers": [ { "id": "a", "do": [ { "tool": "x" } ] } ] }),
            &[("/triggers/0", "requires an `on` source")],
        ),
        invalid(
            "duplicate_trigger_ids",
            json!({ "version": 1, "name": "t", "root": { "sequence": [] },
                    "triggers": [
                        { "id": "a", "on": { "event": "x" }, "do": [ { "tool": "x" } ] },
                        { "id": "a", "on": { "event": "y" }, "do": [ { "tool": "y" } ] }
                    ] }),
            &[("/triggers/1/id", "duplicate trigger id `a`")],
        ),
        invalid(
            "trigger_on_with_both_sources",
            json!({ "version": 1, "name": "t", "root": { "sequence": [] },
                    "triggers": [ { "id": "a",
                                    "on": { "event": "x", "poll": { "tool": "y", "interval": "1m" } },
                                    "do": [ { "tool": "x" } ] } ] }),
            &[("/triggers/0/on", "exactly one of `event` or `poll`")],
        ),
        invalid(
            "poll_missing_interval",
            json!({ "version": 1, "name": "t", "root": { "sequence": [] },
                    "triggers": [ { "id": "a", "on": { "poll": { "tool": "y" } },
                                    "do": [ { "tool": "x" } ] } ] }),
            &[("/triggers/0/on/poll", "requires an `interval`")],
        ),
        invalid(
            "trigger_once_not_boolean",
            json!({ "version": 1, "name": "t", "root": { "sequence": [] },
                    "triggers": [ { "id": "a", "on": { "event": "x" }, "once": "yes",
                                    "do": [ { "tool": "x" } ] } ] }),
            &[("/triggers/0/once", "is a boolean")],
        ),
        invalid(
            "trigger_empty_do",
            json!({ "version": 1, "name": "t", "root": { "sequence": [] },
                    "triggers": [ { "id": "a", "on": { "event": "x" }, "do": [] } ] }),
            &[("/triggers/0/do", "at least one instruction")],
        ),
        invalid(
            "trigger_unknown_key",
            json!({ "version": 1, "name": "t", "root": { "sequence": [] },
                    "triggers": [ { "id": "a", "on": { "event": "x" }, "priority": 1,
                                    "do": [ { "tool": "x" } ] } ] }),
            &[(
                "/triggers/0/priority",
                "unknown key `priority` in a trigger",
            )],
        ),
        // Instruction `once` keys inside a trigger `do` share the
        // document-wide uniqueness space with the procedure tree.
        invalid(
            "duplicate_once_across_tree_and_trigger",
            json!({
                "version": 1, "name": "t",
                "root": { "tool": "calibrator_on", "once": "panel-on" },
                "triggers": [ { "id": "a", "on": { "event": "x" },
                                "do": [ { "tool": "calibrator_on", "once": "panel-on" } ] } ]
            }),
            &[("/triggers/0/do/0/once", "duplicate `once` key `panel-on`")],
        ),
    ]
}

/// The shipped `deep_sky.json` first-party document — the dispatch-loop
/// golden case: planner-driven target acquisition, the trigger overlay
/// (event + poll sources), fail-fast parameter guards, and a `while`
/// loop re-entrant with zero `once` markers. Embedding the real file
/// keeps every suite that consumes this corpus pinned to the artifact
/// that ships, not a copy.
pub fn golden_deep_sky() -> serde_json::Value {
    serde_json::from_str(include_str!("../../workflows/deep_sky.json"))
        .expect("workflows/deep_sky.json is not valid JSON")
}

/// The shipped `sky_flat.json` first-party document — the
/// adaptive-convergence golden case: per-frame exposure rescaling
/// against a changing sky, the dusk/dawn twilight-window branches, a
/// `wait` inside a loop, and the index-marker re-entrancy idiom.
/// Embedded verbatim like its sibling: the suites pin the shipped
/// artifact, not a copy.
pub fn golden_sky_flat() -> serde_json::Value {
    serde_json::from_str(include_str!("../../workflows/sky_flat.json"))
        .expect("workflows/sky_flat.json is not valid JSON")
}
