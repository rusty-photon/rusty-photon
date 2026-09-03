//! Layer-2 catalog-validation tests: names, required parameters, closed
//! schemas, literal type checks, `$expr` handling, poll-trigger sources,
//! and the pointer derivation for every container shape.

use serde_json::{json, Value};

use super::catalog::{validate_against_catalog, ToolSpec};
use super::Document;

fn spec(name: &str, input_schema: Value) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        input_schema,
    }
}

/// A `capture`-like closed schema: two required, one optional.
fn capture_spec() -> ToolSpec {
    spec(
        "capture",
        json!({
            "type": "object",
            "properties": {
                "camera_id": { "type": "string" },
                "duration": { "type": "string" },
                "gain": { "type": "integer", "minimum": 0 }
            },
            "required": ["camera_id", "duration"],
            "additionalProperties": false
        }),
    )
}

fn doc(document: Value) -> Document {
    Document::from_value(&document)
        .unwrap_or_else(|issues| panic!("test document invalid: {issues:#?}"))
}

fn doc_with_root(root: Value) -> Document {
    doc(json!({ "version": 1, "name": "t", "root": root }))
}

fn findings(document: &Document, catalog: &[ToolSpec]) -> Vec<(String, String)> {
    validate_against_catalog(document, catalog)
        .into_iter()
        .map(|issue| (issue.pointer, issue.message))
        .collect()
}

#[test]
fn test_a_valid_call_produces_no_issues() {
    let document = doc_with_root(json!({
        "tool": "capture",
        "args": { "camera_id": "cam", "duration": "2s", "gain": 100 }
    }));
    assert_eq!(findings(&document, &[capture_spec()]), vec![]);
}

#[test]
fn test_unknown_tool_is_reported_at_the_tool_key() {
    let document = doc_with_root(json!({ "tool": "captre", "args": { "x": 1 } }));
    assert_eq!(
        findings(&document, &[capture_spec()]),
        vec![(
            "/root/tool".to_owned(),
            "tool `captre` is not in rp's tool catalog".to_owned()
        )]
    );
}

#[test]
fn test_missing_required_parameter_is_reported() {
    let document = doc_with_root(json!({
        "tool": "capture",
        "args": { "camera_id": "cam" }
    }));
    assert_eq!(
        findings(&document, &[capture_spec()]),
        vec![(
            "/root".to_owned(),
            "tool `capture` requires argument `duration`".to_owned()
        )]
    );
}

#[test]
fn test_expr_arguments_satisfy_required_parameters() {
    let document = doc_with_root(json!({
        "tool": "capture",
        "args": {
            "camera_id": { "$expr": "session.cam" },
            "duration": { "$expr": "humantime(session.duration)" }
        }
    }));
    assert_eq!(findings(&document, &[capture_spec()]), vec![]);
}

#[test]
fn test_unknown_argument_name_is_reported_for_closed_schemas() {
    // Both literal and $expr arguments get the name check — a misspelled
    // argument must not silently travel to the tool.
    let document = doc_with_root(json!({
        "tool": "capture",
        "args": {
            "camera_id": "cam",
            "duration": "2s",
            "gian": 100,
            "durration": { "$expr": "session.d" }
        }
    }));
    assert_eq!(
        findings(&document, &[capture_spec()]),
        vec![
            (
                "/root/args/durration".to_owned(),
                "tool `capture` has no parameter `durration`".to_owned()
            ),
            (
                "/root/args/gian".to_owned(),
                "tool `capture` has no parameter `gian`".to_owned()
            ),
        ]
    );
}

#[test]
fn test_unknown_argument_names_are_allowed_for_open_schemas() {
    let open = spec(
        "annotate",
        json!({ "type": "object", "properties": { "note": { "type": "string" } } }),
    );
    let document = doc_with_root(json!({
        "tool": "annotate",
        "args": { "note": "n", "extra": 1 }
    }));
    assert_eq!(findings(&document, &[open]), vec![]);
}

#[test]
fn test_literal_argument_type_mismatch_is_reported_with_its_pointer() {
    let document = doc_with_root(json!({
        "tool": "capture",
        "args": { "camera_id": "cam", "duration": "2s", "gain": -5 }
    }));
    let issues = findings(&document, &[capture_spec()]);
    assert_eq!(issues.len(), 1, "{issues:#?}");
    assert_eq!(issues[0].0, "/root/args/gain");
    assert!(
        issues[0]
            .1
            .starts_with("argument does not match tool `capture`'s parameter schema:"),
        "{}",
        issues[0].1
    );
}

#[test]
fn test_nested_literal_values_are_validated_with_nested_pointers() {
    let nested = spec(
        "slew",
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "object",
                    "properties": {
                        "ra_hours": { "type": "number" },
                        "dec_degrees": { "type": "number" }
                    },
                    "required": ["ra_hours", "dec_degrees"]
                }
            },
            "required": ["target"]
        }),
    );
    let document = doc_with_root(json!({
        "tool": "slew",
        "args": { "target": { "ra_hours": "twelve", "dec_degrees": 45.0 } }
    }));
    let issues = findings(&document, &[nested]);
    assert_eq!(issues.len(), 1, "{issues:#?}");
    // The instance path inside the literal object continues the pointer.
    assert_eq!(issues[0].0, "/root/args/target/ra_hours");
}

#[test]
fn test_nested_required_inside_a_literal_object_still_applies() {
    // Stripping `required` only applies to the top level (where $expr
    // arguments are invisible); a literal object argument must still be
    // complete.
    let nested = spec(
        "slew",
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "object",
                    "properties": { "ra_hours": { "type": "number" },
                                     "dec_degrees": { "type": "number" } },
                    "required": ["ra_hours", "dec_degrees"]
                }
            },
            "required": ["target"]
        }),
    );
    let document = doc_with_root(json!({
        "tool": "slew",
        "args": { "target": { "ra_hours": 12.0 } }
    }));
    let issues = findings(&document, &[nested]);
    assert_eq!(issues.len(), 1, "{issues:#?}");
    assert_eq!(issues[0].0, "/root/args/target");
    assert!(issues[0].1.contains("dec_degrees"), "{}", issues[0].1);
}

#[test]
fn test_expr_argument_values_are_not_type_checked_statically() {
    // The $expr value is checked at runtime when the call is made; a
    // literal in the same call is still checked.
    let document = doc_with_root(json!({
        "tool": "capture",
        "args": {
            "camera_id": { "$expr": "1 + 1" },
            "duration": 5
        }
    }));
    let issues = findings(&document, &[capture_spec()]);
    assert_eq!(issues.len(), 1, "{issues:#?}");
    assert_eq!(issues[0].0, "/root/args/duration");
}

#[test]
fn test_tools_are_checked_in_every_container_shape() {
    let document = doc_with_root(json!({ "sequence": [
        { "tool": "missing_in_sequence" },
        { "repeat": { "count": 1 }, "body": [ { "tool": "missing_in_body" } ] },
        { "if": "true",
          "then": [ { "tool": "missing_in_then" } ],
          "else": [ { "tool": "missing_in_else" } ] },
        { "try": [ { "tool": "missing_in_try" } ],
          "catch": [ { "tool": "missing_in_catch" } ],
          "finally": [ { "tool": "missing_in_finally" } ] }
    ] }));
    let pointers: Vec<String> = findings(&document, &[])
        .into_iter()
        .map(|(pointer, _)| pointer)
        .collect();
    assert_eq!(
        pointers,
        vec![
            "/root/sequence/0/tool",
            "/root/sequence/1/body/0/tool",
            "/root/sequence/2/then/0/tool",
            "/root/sequence/2/else/0/tool",
            "/root/sequence/3/try/0/tool",
            "/root/sequence/3/catch/0/tool",
            "/root/sequence/3/finally/0/tool",
        ]
    );
}

#[test]
fn test_trigger_actions_and_poll_sources_are_checked() {
    let document = doc(json!({
        "version": 1, "name": "t",
        "triggers": [
            { "id": "t1",
              "on": { "poll": { "tool": "get_meridian_status",
                                 "args": { "mount_id": 5 },
                                 "interval": "60s" } },
              "do": [ { "tool": "missing_in_do" } ] }
        ],
        "root": { "log": { "message": "m" } }
    }));
    let meridian = spec(
        "get_meridian_status",
        json!({
            "type": "object",
            "properties": { "mount_id": { "type": "string" } },
            "required": ["mount_id"]
        }),
    );
    let issues = findings(&document, &[meridian]);
    assert_eq!(issues.len(), 2, "{issues:#?}");
    assert_eq!(issues[0].0, "/triggers/0/on/poll/args/mount_id");
    assert_eq!(issues[1].0, "/triggers/0/do/0/tool");
    assert_eq!(
        issues[1].1,
        "tool `missing_in_do` is not in rp's tool catalog"
    );
}

#[test]
fn test_a_non_object_input_schema_only_gets_the_name_check() {
    let odd = spec("odd", json!(true));
    let document = doc_with_root(json!({ "tool": "odd", "args": { "anything": 1 } }));
    assert_eq!(findings(&document, &[odd]), vec![]);
}

#[test]
fn test_golden_calibrator_flats_passes_against_a_matching_catalog() {
    let document = doc(super::corpus::golden_calibrator_flats());
    let object_schema = |properties: Value, required: &[&str]| {
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        })
    };
    let catalog = vec![
        spec(
            "get_camera_info",
            object_schema(json!({ "camera_id": { "type": "string" } }), &["camera_id"]),
        ),
        spec(
            "get_cover_state",
            object_schema(
                json!({ "calibrator_id": { "type": "string" } }),
                &["calibrator_id"],
            ),
        ),
        spec(
            "close_cover",
            object_schema(
                json!({ "calibrator_id": { "type": "string" } }),
                &["calibrator_id"],
            ),
        ),
        spec(
            "open_cover",
            object_schema(
                json!({ "calibrator_id": { "type": "string" } }),
                &["calibrator_id"],
            ),
        ),
        spec(
            "calibrator_on",
            object_schema(
                json!({ "calibrator_id": { "type": "string" },
                         "brightness": { "type": "integer" } }),
                &["calibrator_id"],
            ),
        ),
        spec(
            "calibrator_off",
            object_schema(
                json!({ "calibrator_id": { "type": "string" } }),
                &["calibrator_id"],
            ),
        ),
        spec(
            "set_filter",
            object_schema(
                json!({ "filter_wheel_id": { "type": "string" },
                         "filter_name": { "type": "string" } }),
                &["filter_wheel_id", "filter_name"],
            ),
        ),
        spec(
            "capture",
            object_schema(
                json!({ "camera_id": { "type": "string" },
                         "duration": { "type": "string" } }),
                &["camera_id", "duration"],
            ),
        ),
        spec(
            "compute_image_stats",
            object_schema(
                json!({ "document_id": { "type": "string" } }),
                &["document_id"],
            ),
        ),
        spec("start_cooldown", object_schema(json!({}), &[])),
        spec("start_warmup", object_schema(json!({}), &[])),
    ];
    assert_eq!(findings(&document, &catalog), vec![]);
}

/// A train-addressable schema in rp's published shape: no top-level
/// `required` for addressing; a presence-only `oneOf` declares the
/// mutually exclusive alternatives.
fn train_addressable_capture_spec() -> ToolSpec {
    spec(
        "capture",
        json!({
            "type": "object",
            "properties": {
                "camera_id": { "type": "string" },
                "train_id": { "type": "string" },
                "duration": { "type": "string" }
            },
            "required": ["duration"],
            "oneOf": [
                { "required": ["camera_id"] },
                { "required": ["train_id"] }
            ],
            "additionalProperties": false
        }),
    )
}

#[test]
fn test_presence_one_of_accepts_either_alternative_as_literal_or_expr() {
    for args in [
        json!({ "camera_id": "cam", "duration": "2s" }),
        json!({ "train_id": "main", "duration": "2s" }),
        json!({ "train_id": { "$expr": "params.train_id" }, "duration": "2s" }),
    ] {
        let document = doc_with_root(json!({ "tool": "capture", "args": args }));
        assert_eq!(
            findings(&document, &[train_addressable_capture_spec()]),
            vec![],
            "args {args}"
        );
    }
}

#[test]
fn test_presence_one_of_fails_a_call_with_no_addressing() {
    let document = doc_with_root(json!({
        "tool": "capture",
        "args": { "duration": "2s" }
    }));
    assert_eq!(
        findings(&document, &[train_addressable_capture_spec()]),
        vec![(
            "/root".to_owned(),
            "tool `capture` requires exactly one of: camera_id or train_id".to_owned()
        )]
    );
}

#[test]
fn test_presence_one_of_fails_a_call_with_both_alternatives() {
    let document = doc_with_root(json!({
        "tool": "capture",
        "args": { "camera_id": "cam", "train_id": { "$expr": "params.train_id" }, "duration": "2s" }
    }));
    assert_eq!(
        findings(&document, &[train_addressable_capture_spec()]),
        vec![(
            "/root".to_owned(),
            "tool `capture` accepts only one of: camera_id or train_id".to_owned()
        )]
    );
}

#[test]
fn test_presence_one_of_with_a_multi_name_branch_requires_the_full_pair() {
    // auto_focus publishes `camera_id + focuser_id` or `train_id`; a
    // call carrying only half the explicit pair satisfies neither.
    let auto_focus = spec(
        "auto_focus",
        json!({
            "type": "object",
            "properties": {
                "camera_id": { "type": "string" },
                "focuser_id": { "type": "string" },
                "train_id": { "type": "string" }
            },
            "oneOf": [
                { "required": ["camera_id", "focuser_id"] },
                { "required": ["train_id"] }
            ],
            "additionalProperties": false
        }),
    );
    let document = doc_with_root(json!({
        "tool": "auto_focus",
        "args": { "camera_id": "cam" }
    }));
    assert_eq!(
        findings(&document, &[auto_focus]),
        vec![(
            "/root".to_owned(),
            "tool `auto_focus` requires exactly one of: camera_id + focuser_id or train_id"
                .to_owned()
        )]
    );
}

#[test]
fn test_a_value_constraining_one_of_is_left_to_the_literal_check() {
    // A `oneOf` whose branches constrain more than presence is NOT the
    // addressing contract: check 2b ignores it and check 4 validates
    // literal values against it unchanged.
    let picky = spec(
        "picky",
        json!({
            "type": "object",
            "properties": { "mode": { "type": "string" } },
            "oneOf": [
                { "properties": { "mode": { "const": "fast" } }, "required": ["mode"] },
                { "properties": { "mode": { "const": "slow" } }, "required": ["mode"] }
            ]
        }),
    );
    let ok = doc_with_root(json!({ "tool": "picky", "args": { "mode": "fast" } }));
    assert_eq!(findings(&ok, std::slice::from_ref(&picky)), vec![]);
    let bad = doc_with_root(json!({ "tool": "picky", "args": { "mode": "wrong" } }));
    let issues = findings(&bad, &[picky]);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(
        issues[0].1.contains("parameter schema"),
        "the value combinator must reach the literal check: {issues:?}"
    );
}
