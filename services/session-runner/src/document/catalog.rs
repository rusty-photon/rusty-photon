//! Layer-2 **catalog validation**: every `tool` node in a validated
//! document checked against `rp`'s live tool catalog (`tools/list`), per
//! `docs/services/session-runner.md` § Validation.
//!
//! Checks, in order per tool node:
//!
//! 1. The tool name exists in the catalog.
//! 2. Every parameter the tool's input schema marks `required` is present
//!    among the call's arguments (as a literal **or** a `$expr` — the
//!    expression's value is type-checked at runtime when the call is
//!    made).
//! 3. When the schema pins `additionalProperties: false`, every argument
//!    name must be a declared property (this covers `$expr` arguments
//!    too — a misspelled argument name must not silently travel).
//! 4. Literal argument values are validated against the tool's input
//!    schema (with `required` and top-level `additionalProperties`
//!    stripped, since checks 2–3 already cover them for both argument
//!    kinds and `$expr` values are absent from the validated object).
//!
//! Issues carry JSON-Pointer locations into the **document**. The typed
//! model does not store source pointers, so this walk re-derives them —
//! the derivation is fixed by the document syntax itself (`sequence`/
//! `body`/`then`/`else`/`try`/`catch`/`finally` array keys, `do` for
//! trigger actions, `on/poll` for poll sources) and pinned by tests.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::model::{ArgValue, Document, Instruction, InstructionKind, TriggerSource};
use super::validate::child;
use super::ValidationIssue;

/// One catalog entry: a tool name plus its parameter JSON Schema, as
/// listed by `rp`'s `tools/list`.
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub input_schema: Value,
}

/// Validate every tool call in `doc` (the procedure tree and all trigger
/// `do` blocks and poll sources) against the catalog. Returns all
/// findings; empty means the document passes layer 2.
#[must_use]
pub fn validate_against_catalog(doc: &Document, catalog: &[ToolSpec]) -> Vec<ValidationIssue> {
    let by_name: BTreeMap<&str, &ToolSpec> = catalog
        .iter()
        .map(|spec| (spec.name.as_str(), spec))
        .collect();
    let mut issues = Vec::new();

    walk_instruction(&doc.root, "/root", &by_name, &mut issues);
    for (index, trigger) in doc.triggers.iter().enumerate() {
        let trigger_ptr = format!("/triggers/{index}");
        if let TriggerSource::Poll { tool, args, .. } = &trigger.on {
            check_tool_call(
                tool,
                args,
                &format!("{trigger_ptr}/on/poll"),
                &by_name,
                &mut issues,
            );
        }
        for (action_index, action) in trigger.actions.iter().enumerate() {
            walk_instruction(
                action,
                &format!("{trigger_ptr}/do/{action_index}"),
                &by_name,
                &mut issues,
            );
        }
    }
    issues
}

fn walk_block(
    block: &[Instruction],
    base_ptr: &str,
    by_name: &BTreeMap<&str, &ToolSpec>,
    issues: &mut Vec<ValidationIssue>,
) {
    for (index, ins) in block.iter().enumerate() {
        walk_instruction(ins, &format!("{base_ptr}/{index}"), by_name, issues);
    }
}

fn walk_instruction(
    ins: &Instruction,
    ptr: &str,
    by_name: &BTreeMap<&str, &ToolSpec>,
    issues: &mut Vec<ValidationIssue>,
) {
    match &ins.kind {
        InstructionKind::Tool(call) => {
            check_tool_call(&call.tool, &call.args, ptr, by_name, issues);
        }
        InstructionKind::Sequence(body) => {
            walk_block(body, &child(ptr, "sequence"), by_name, issues);
        }
        InstructionKind::Repeat(repeat) => {
            walk_block(&repeat.body, &child(ptr, "body"), by_name, issues);
        }
        InstructionKind::If {
            then, otherwise, ..
        } => {
            walk_block(then, &child(ptr, "then"), by_name, issues);
            if let Some(otherwise) = otherwise {
                walk_block(otherwise, &child(ptr, "else"), by_name, issues);
            }
        }
        InstructionKind::Try {
            body,
            catch,
            finally,
        } => {
            walk_block(body, &child(ptr, "try"), by_name, issues);
            if let Some(catch) = catch {
                walk_block(catch, &child(ptr, "catch"), by_name, issues);
            }
            if let Some(finally) = finally {
                walk_block(finally, &child(ptr, "finally"), by_name, issues);
            }
        }
        InstructionKind::Set(_)
        | InstructionKind::Fail { .. }
        | InstructionKind::Wait(_)
        | InstructionKind::Log(_) => {}
    }
}

/// Check one tool call. `node_ptr` is the node carrying the `tool` key —
/// an instruction node, or a trigger's `on/poll` object.
fn check_tool_call(
    tool: &str,
    args: &BTreeMap<String, ArgValue>,
    node_ptr: &str,
    by_name: &BTreeMap<&str, &ToolSpec>,
    issues: &mut Vec<ValidationIssue>,
) {
    let Some(spec) = by_name.get(tool) else {
        issues.push(ValidationIssue {
            pointer: child(node_ptr, "tool"),
            message: format!("tool `{tool}` is not in rp's tool catalog"),
            expr_span: None,
        });
        return;
    };
    let Value::Object(schema) = &spec.input_schema else {
        // A non-object input schema offers nothing to check against;
        // the name check above is all layer 2 can do.
        return;
    };
    let args_ptr = child(node_ptr, "args");

    // 2. Required parameters must be present (literal or `$expr`).
    if let Some(Value::Array(required)) = schema.get("required") {
        for name in required.iter().filter_map(Value::as_str) {
            if !args.contains_key(name) {
                issues.push(ValidationIssue {
                    pointer: node_ptr.to_owned(),
                    message: format!("tool `{tool}` requires argument `{name}`"),
                    expr_span: None,
                });
            }
        }
    }

    check_addressing_alternatives(tool, schema, args, node_ptr, issues);

    // 3. Unknown argument names, when the schema is closed.
    if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
        let properties = schema.get("properties").and_then(Value::as_object);
        for name in args.keys() {
            let declared = properties.is_some_and(|p| p.contains_key(name));
            if !declared {
                issues.push(ValidationIssue {
                    pointer: child(&args_ptr, name),
                    message: format!("tool `{tool}` has no parameter `{name}`"),
                    expr_span: None,
                });
            }
        }
    }

    check_literal_values(tool, schema, args, node_ptr, &args_ptr, issues);
}

/// Check 2b: addressing alternatives. A top-level `oneOf` whose
/// branches are presence-only (each object carrying nothing but a
/// `required` name list) declares mutually exclusive argument
/// sets — rp's train-addressable tools publish
/// `camera_id`-or-`train_id` this way. Exactly one branch must
/// be fully present among the call's argument names (literal or
/// `$expr` — like check 2, this is a name-presence rule, so it
/// covers both kinds). Value combinators (branches constraining
/// anything beyond presence) are left to check 4.
fn check_addressing_alternatives(
    tool: &str,
    schema: &Map<String, Value>,
    args: &BTreeMap<String, ArgValue>,
    node_ptr: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    let Some(branches) = presence_one_of(schema) else {
        return;
    };
    let satisfied = branches
        .iter()
        .filter(|branch| branch.iter().all(|name| args.contains_key(*name)))
        .count();
    if satisfied != 1 {
        let names = branches
            .iter()
            .map(|branch| branch.join(" + "))
            .collect::<Vec<_>>();
        let alternatives = match names.as_slice() {
            [a, b] => format!("{a} or {b}"),
            [rest @ .., last] if names.len() > 2 => {
                format!("{}, or {last}", rest.join(", "))
            }
            _ => names.join(""),
        };
        let problem = if satisfied == 0 {
            "requires exactly one of"
        } else {
            "accepts only one of"
        };
        issues.push(ValidationIssue {
            pointer: node_ptr.to_owned(),
            message: format!("tool `{tool}` {problem}: {alternatives}"),
            expr_span: None,
        });
    }
}

/// Check 4: literal values against the schema, with `required` and
/// top-level `additionalProperties` stripped (covered by checks 2/2b/3
/// for both argument kinds; `$expr` values are absent from the object
/// validated here).
fn check_literal_values(
    tool: &str,
    schema: &Map<String, Value>,
    args: &BTreeMap<String, ArgValue>,
    node_ptr: &str,
    args_ptr: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    let literals: Map<String, Value> = args
        .iter()
        .filter_map(|(name, arg)| match arg {
            ArgValue::Literal(value) => Some((name.clone(), value.clone())),
            ArgValue::Expr(_) => None,
        })
        .collect();
    if literals.is_empty() {
        return;
    }
    let mut stripped = schema.clone();
    stripped.remove("required");
    stripped.remove("additionalProperties");
    if presence_one_of(&stripped).is_some() {
        // Check 2b enforced the presence combinator against both
        // argument kinds; validating it here against the literal-only
        // object would falsely fail `$expr`-addressed calls.
        stripped.remove("oneOf");
    }
    match jsonschema::validator_for(&Value::Object(stripped)) {
        Ok(validator) => {
            for error in validator.iter_errors(&Value::Object(literals)) {
                let instance_path = error.instance_path().to_string();
                let pointer = format!("{args_ptr}{instance_path}");
                issues.push(ValidationIssue {
                    pointer,
                    message: format!(
                        "argument does not match tool `{tool}`'s parameter schema: {error}"
                    ),
                    expr_span: None,
                });
            }
        }
        Err(error) => {
            // Not the document's fault, but layer 2 cannot vouch for the
            // call — fail loud rather than run unvalidated (tenet 3).
            issues.push(ValidationIssue {
                pointer: node_ptr.to_owned(),
                message: format!("tool `{tool}`'s parameter schema does not compile: {error}"),
                expr_span: None,
            });
        }
    }
}

/// The schema's top-level `oneOf` as a presence combinator: every branch
/// an object carrying **only** a `required` array of strings. Returns the
/// branches' name lists, or `None` when the `oneOf` is absent or any
/// branch constrains more than presence (those stay check 4's business).
fn presence_one_of(schema: &Map<String, Value>) -> Option<Vec<Vec<&str>>> {
    let branches = schema.get("oneOf")?.as_array()?;
    branches
        .iter()
        .map(|branch| {
            let object = branch.as_object()?;
            if object.len() != 1 {
                return None;
            }
            let names = object.get("required")?.as_array()?;
            names.iter().map(Value::as_str).collect()
        })
        .collect()
}
