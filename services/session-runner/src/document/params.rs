//! Invocation-parameter validation (layer 3 of 3 — see
//! `docs/services/session-runner.md` § Validation): the `/invoke`
//! payload's `config.parameters` object checked against the document's
//! declarations, producing the immutable `params.*` namespace value.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::duration;
use super::model::{ParameterDecl, ParameterType};
use super::ValidationIssue;

/// Whether `v` inhabits the declared parameter type. Shared by
/// declaration-default checking (layer 1) and invocation binding
/// (layer 3). The error is a message fragment; callers attach the
/// JSON Pointer.
pub(super) fn type_check(ty: ParameterType, v: &Value) -> Result<(), String> {
    let ok = match ty {
        ParameterType::String => v.is_string(),
        // Stricter than JSON Schema's `integer` (which admits 3.0): a
        // parameter integer is a JSON integer literal.
        ParameterType::Integer => v.is_i64() || v.is_u64(),
        ParameterType::Number => v.is_number(),
        ParameterType::Boolean => v.is_boolean(),
        ParameterType::Duration => match v {
            Value::String(s) => return duration::parse_duration(s).map(|_| ()),
            _ => false,
        },
        ParameterType::Array => v.is_array(),
    };
    if ok {
        Ok(())
    } else {
        let article = if matches!(ty, ParameterType::Integer | ParameterType::Array) {
            "an"
        } else {
            "a"
        };
        Err(format!("expected {article} `{}` value", ty.name()))
    }
}

/// Binds invocation parameters to the document's declarations.
///
/// Unknown names, missing required parameters, and type mismatches are
/// all reported (not just the first); on success the returned object —
/// supplied values plus defaults for the rest — is the `params.*`
/// namespace for the whole session. Pointers are relative to the
/// invocation's `config` object (e.g. `/parameters/camera_id`).
///
/// # Errors
///
/// Returns every binding [`ValidationIssue`]: a non-object
/// `parameters` value, unknown names, missing required parameters, and
/// type mismatches.
pub fn bind_parameters(
    decls: &BTreeMap<String, ParameterDecl>,
    supplied: Option<&Value>,
) -> Result<Value, Vec<ValidationIssue>> {
    let mut issues = Vec::new();
    let empty = Map::new();
    let supplied = match supplied {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => {
            return Err(vec![issue(
                "/parameters",
                "`parameters` must be a JSON object",
            )]);
        }
    };

    for name in supplied.keys() {
        if !decls.contains_key(name) {
            let declared = if decls.is_empty() {
                "the document declares no parameters".to_owned()
            } else {
                let names: Vec<&str> = decls.keys().map(String::as_str).collect();
                format!("the document declares: {}", names.join(", "))
            };
            issues.push(issue(
                &pointer(name),
                format!("unknown parameter `{name}` — {declared}"),
            ));
        }
    }

    let mut bound = Map::new();
    for (name, decl) in decls {
        match supplied.get(name) {
            Some(v) => match type_check(decl.ty, v) {
                Ok(()) => {
                    bound.insert(name.clone(), v.clone());
                }
                Err(msg) => {
                    issues.push(issue(&pointer(name), format!("parameter `{name}`: {msg}")));
                }
            },
            None => match &decl.default {
                Some(d) => {
                    bound.insert(name.clone(), d.clone());
                }
                None => issues.push(issue(
                    "/parameters",
                    format!(
                        "missing required parameter `{name}` (type `{}`)",
                        decl.ty.name()
                    ),
                )),
            },
        }
    }

    if issues.is_empty() {
        Ok(Value::Object(bound))
    } else {
        Err(issues)
    }
}

fn pointer(name: &str) -> String {
    let escaped = name.replace('~', "~0").replace('/', "~1");
    format!("/parameters/{escaped}")
}

fn issue(ptr: &str, message: impl Into<String>) -> ValidationIssue {
    ValidationIssue {
        pointer: ptr.to_owned(),
        message: message.into(),
        expr_span: None,
    }
}
