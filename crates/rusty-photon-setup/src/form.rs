//! A submitted form coerced back into config values, field by field, by the
//! rules [`FieldKind`] states.

use rusty_photon_config::actions::FieldError;
use serde_json::Value;

use crate::schema::{FieldKind, FieldSpec};

/// Submitted form pairs with duplicate keys preserved.
///
/// A checkbox group posts one `name=value` pair per checked box, and a map
/// decoding would collapse duplicates — so the pairs are kept as sent.
/// Single-value lookups take the first occurrence.
#[derive(Debug, Default, Clone)]
pub struct FormValues(Vec<(String, String)>);

impl From<Vec<(String, String)>> for FormValues {
    fn from(pairs: Vec<(String, String)>) -> Self {
        Self(pairs)
    }
}

impl FormValues {
    /// The first value submitted under `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Every value submitted under `name`, in order.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.0
            .iter()
            .filter(move |(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Whether anything was submitted under `name`.
    #[must_use]
    pub fn contains_key(&self, name: &str) -> bool {
        self.0.iter().any(|(k, _)| k == name)
    }

    /// Replace every pair under `name` with a single `value` (appending
    /// when absent).
    pub fn set(&mut self, name: &str, value: String) {
        self.0.retain(|(k, _)| k != name);
        self.0.push((name.to_string(), value));
    }

    /// The pairs as sent.
    #[must_use]
    pub fn pairs(&self) -> &[(String, String)] {
        &self.0
    }
}

/// Overlay one field from the submitted form onto `config`.
///
/// The field is written at its pointer by its kind's rule; a rejected value
/// becomes a [`FieldError`] and the config is left as it was at that
/// pointer. A [`FieldKind::FileOnly`] leaf is never touched.
pub fn overlay_field(
    config: &mut Value,
    spec: &FieldSpec,
    form: &FormValues,
    errors: &mut Vec<FieldError>,
) {
    overlay_field_except(config, spec, form, &[], errors);
}

/// [`overlay_field`], leaving every control named in `skipped` as the config
/// has it.
///
/// A tagged enum's field that a command-line override pins is what
/// `skipped` names: the enum's overlay would otherwise write it from the
/// form. A pinned field outside an enum is simply not overlaid; this
/// reaches the ones inside.
pub fn overlay_field_except(
    config: &mut Value,
    spec: &FieldSpec,
    form: &FormValues,
    skipped: &[String],
    errors: &mut Vec<FieldError>,
) {
    overlay_at(config, spec.pointer(), spec, form, skipped, errors);
}

/// [`overlay_field`] at an explicit pointer — a variant's field writes under
/// its enum's node rather than at its own (relative) pointer.
fn overlay_at(
    config: &mut Value,
    pointer: &str,
    spec: &FieldSpec,
    form: &FormValues,
    skipped: &[String],
    errors: &mut Vec<FieldError>,
) {
    let name = spec.name();
    match spec.kind() {
        // A checkbox submits its name only when checked: present ⇒ true.
        FieldKind::Bool => {
            set_pointer(config, pointer, Value::Bool(form.contains_key(name)));
        }
        // A checkbox group submits one pair per checked box; absent pairs
        // mean nothing is checked ⇒ an empty array. Values are written in
        // `options` (schema `enum`) order, which also dedupes.
        FieldKind::IntSet { options } => {
            let selected: Result<Vec<i64>, ()> = form
                .get_all(name)
                .map(|raw| match raw.trim().parse::<i64>() {
                    Ok(n) if options.contains(&n) => Ok(n),
                    _ => Err(()),
                })
                .collect();
            match selected {
                Ok(selected) => {
                    let chosen: Vec<Value> = options
                        .iter()
                        .filter(|o| selected.contains(o))
                        .map(|o| Value::from(*o))
                        .collect();
                    set_pointer(config, pointer, Value::Array(chosen));
                }
                Err(()) => errors.push(field_error(name, "must be values from the allowed set")),
            }
        }
        FieldKind::Str { nullable } => {
            if let Some(raw) = form.get(name) {
                // Optional: clear to null, the same gesture the number kinds
                // honour below. `""` is not a spelling of "unset" — a service
                // that distinguishes the two must receive the one the operator
                // meant. Required: keep writing the empty string, so a service
                // that validates its own required fields still gets to say so.
                if *nullable && raw.trim().is_empty() {
                    set_pointer(config, pointer, Value::Null);
                } else {
                    set_pointer(config, pointer, Value::String(raw.to_string()));
                }
            }
        }
        // A blank secret means "unchanged"; anything else replaces it.
        FieldKind::Secret => {
            if let Some(raw) = form.get(name).filter(|raw| !raw.is_empty()) {
                set_pointer(config, pointer, Value::String(raw.to_string()));
            }
        }
        FieldKind::Int { nullable, min, max } => {
            let Some(raw) = form.get(name) else {
                return;
            };
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                // Optional: clear to null. Required: say so, rather than
                // keep the prior value and call the save unchanged.
                if *nullable {
                    set_pointer(config, pointer, Value::Null);
                } else {
                    errors.push(field_error(name, "is required"));
                }
            } else {
                match trimmed.parse::<i64>() {
                    Ok(n) if min.is_none_or(|lo| n >= lo) && max.is_none_or(|hi| n <= hi) => {
                        set_pointer(config, pointer, Value::from(n));
                    }
                    _ => errors.push(field_error(name, &int_error(*min, *max))),
                }
            }
        }
        FieldKind::Num { nullable } => {
            let Some(raw) = form.get(name) else {
                return;
            };
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                if *nullable {
                    set_pointer(config, pointer, Value::Null);
                } else {
                    errors.push(field_error(name, "is required"));
                }
            } else {
                match trimmed.parse::<f64>() {
                    Ok(n) => set_pointer(config, pointer, Value::from(n)),
                    Err(_) => errors.push(field_error(name, "must be a number")),
                }
            }
        }
        FieldKind::Variants { tag, variants } => {
            overlay_variants(config, pointer, name, tag, variants, form, skipped, errors);
        }
        FieldKind::FileOnly => {}
    }
}

/// The chosen variant's fields, under its tag; a variant change replaces
/// the whole object so the other variant's fields do not linger beside the
/// new tag. A field named in `skipped` keeps what the object has.
#[allow(clippy::too_many_arguments)]
fn overlay_variants(
    config: &mut Value,
    pointer: &str,
    name: &str,
    tag: &str,
    variants: &[crate::schema::Variant],
    form: &FormValues,
    skipped: &[String],
    errors: &mut Vec<FieldError>,
) {
    let Some(chosen) = form.get(name) else {
        return;
    };
    let Some(variant) = variants.iter().find(|v| v.name == chosen) else {
        errors.push(field_error(name, "must be one of the offered variants"));
        return;
    };
    let current_tag = config
        .pointer(pointer)
        .and_then(|v| v.get(tag))
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut object = if current_tag.as_deref() == Some(chosen) {
        config.pointer(pointer).cloned().unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    if !object.is_object() {
        object = Value::Object(serde_json::Map::new());
    }
    set_pointer(
        &mut object,
        &format!("/{tag}"),
        Value::String(chosen.to_string()),
    );
    for field in &variant.fields {
        if skipped.iter().any(|s| s == field.name()) {
            continue;
        }
        overlay_at(&mut object, field.pointer(), field, form, skipped, errors);
    }
    set_pointer(config, pointer, object);
}

fn int_error(min: Option<i64>, max: Option<i64>) -> String {
    match (min, max) {
        (Some(lo), Some(hi)) => format!("must be a whole number between {lo} and {hi}"),
        _ => "must be a whole number".to_string(),
    }
}

fn field_error(path: &str, msg: &str) -> FieldError {
    FieldError {
        path: path.to_string(),
        msg: msg.to_string(),
    }
}

/// Write `value` at `pointer`, creating the leaf key when its parent object
/// exists without it.
///
/// An optional field absent from the file (absence is how "unset" is
/// spelled) must still be settable from the page.
pub fn set_pointer(config: &mut Value, pointer: &str, value: Value) {
    if let Some(slot) = config.pointer_mut(pointer) {
        *slot = value;
        return;
    }
    let Some((parent, key)) = pointer.rsplit_once('/') else {
        return;
    };
    if let Some(Value::Object(map)) = config.pointer_mut(parent) {
        _ = map.insert(key.to_string(), value);
    }
}

/// The text an input renders for the value at `pointer`: a string as is, a
/// number or bool spelled out, anything else (absent, null, composite)
/// empty.
#[must_use]
pub fn text_at(config: &Value, pointer: &str) -> String {
    config.pointer(pointer).map_or_else(String::new, text_of)
}

/// The text an input renders for `value`: a string as is, a number or bool
/// spelled out, anything else (null, composite) empty.
#[must_use]
pub fn text_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// Whether the value at `pointer` is `true`.
#[must_use]
pub fn bool_at(config: &Value, pointer: &str) -> bool {
    config
        .pointer(pointer)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether the array at `pointer` contains the integer `value` — the
/// `checked` state of a checkbox-group member.
#[must_use]
pub fn array_contains(config: &Value, pointer: &str, value: i64) -> bool {
    config
        .pointer(pointer)
        .is_some_and(|items| array_has(items, value))
}

/// Whether `items` is an array containing the integer `value`.
#[must_use]
pub fn array_has(items: &Value, value: i64) -> bool {
    items
        .as_array()
        .is_some_and(|items| items.iter().any(|v| v.as_i64() == Some(value)))
}

/// Serialize `value` to JSON with object keys sorted recursively, so the bytes
/// are the same whichever map backend `serde_json` was built with.
#[must_use]
pub fn canonical_json(value: &Value) -> String {
    fn sort_keys(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut sorted = serde_json::Map::new();
                for key in keys {
                    _ = sorted.insert(key.clone(), sort_keys(&map[key]));
                }
                Value::Object(sorted)
            }
            Value::Array(items) => Value::Array(items.iter().map(sort_keys).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&sort_keys(value)).unwrap_or_default()
}

/// Dotted JSON paths whose leaf values differ between `before` and `after`.
/// Objects recurse; everything else compares by equality.
#[must_use]
pub fn changed_paths(before: &Value, after: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    diff_into(before, after, "", &mut paths);
    paths
}

fn diff_into(before: &Value, after: &Value, prefix: &str, out: &mut Vec<String>) {
    match (before, after) {
        (Value::Object(b), Value::Object(a)) => {
            let keys: std::collections::BTreeSet<&String> = b.keys().chain(a.keys()).collect();
            for key in keys {
                let child = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                match (b.get(key), a.get(key)) {
                    (Some(bv), Some(av)) => diff_into(bv, av, &child, out),
                    _ => out.push(child),
                }
            }
        }
        _ => {
            if before != after {
                out.push(prefix.to_string());
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::schema::FieldModel;
    use serde_json::json;

    fn form_from(pairs: &[(&str, &str)]) -> FormValues {
        FormValues::from(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<Vec<_>>(),
        )
    }

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "serial": {
                    "type": "object",
                    "properties": {
                        "port": { "type": "string" },
                        "baud_rate": { "type": "integer", "minimum": 0 },
                        "label": { "type": ["string", "null"] },
                        "gain": { "type": "number" },
                        "enabled": { "type": "boolean" },
                        "key": { "type": "string" },
                    },
                },
                "server": {
                    "type": "object",
                    "properties": {
                        "port": { "type": "integer", "minimum": 0, "maximum": 65535 },
                        "discovery_port": { "type": ["integer", "null"], "minimum": 0, "maximum": 65535 },
                    },
                },
                "cooling": {
                    "type": "object",
                    "properties": {
                        "targets": { "type": "array", "items": { "type": "integer", "enum": [-10, -5, 0] } },
                    },
                },
            },
        })
    }

    fn model() -> FieldModel {
        FieldModel::from_schema(&schema()).with_secrets(&["/serial/key"])
    }

    fn overlay(config: &mut Value, name: &str, form: &FormValues) -> Vec<FieldError> {
        let model = model();
        let spec = model.field(name).unwrap();
        let mut errors = Vec::new();
        overlay_field(config, spec, form, &mut errors);
        errors
    }

    #[test]
    fn form_values_preserves_duplicates_and_set_replaces_them() {
        let mut form = form_from(&[("targets", "-10"), ("other", "x"), ("targets", "0")]);
        assert_eq!(form.get("targets"), Some("-10"), "get takes the first pair");
        assert_eq!(
            form.get_all("targets").collect::<Vec<_>>(),
            vec!["-10", "0"]
        );
        assert!(form.contains_key("other"));
        assert!(!form.contains_key("absent"));
        form.set("targets", "5".to_string());
        assert_eq!(form.get_all("targets").collect::<Vec<_>>(), vec!["5"]);
        form.set("fresh", "1".to_string());
        assert_eq!(form.get("fresh"), Some("1"), "set appends when absent");
        assert_eq!(form.pairs().len(), 3);
    }

    #[test]
    fn a_string_is_set_verbatim_and_a_cleared_optional_string_becomes_null() {
        let mut config = json!({ "serial": { "port": "/dev/a", "label": "x" } });
        let errors = overlay(
            &mut config,
            "serial.port",
            &form_from(&[("serial.port", "/dev/b")]),
        );
        assert_eq!(errors, Vec::new());
        assert_eq!(config["serial"]["port"], "/dev/b");
        overlay(
            &mut config,
            "serial.label",
            &form_from(&[("serial.label", "  ")]),
        );
        assert!(config["serial"]["label"].is_null());
        // A required string keeps sending "", for the service to reject.
        overlay(
            &mut config,
            "serial.port",
            &form_from(&[("serial.port", "")]),
        );
        assert_eq!(config["serial"]["port"], "");
    }

    #[test]
    fn a_checkbox_is_true_when_present_and_false_when_absent() {
        let mut config = json!({ "serial": { "enabled": true } });
        overlay(&mut config, "serial.enabled", &form_from(&[]));
        assert_eq!(config["serial"]["enabled"], false);
        overlay(
            &mut config,
            "serial.enabled",
            &form_from(&[("serial.enabled", "on")]),
        );
        assert_eq!(config["serial"]["enabled"], true);
    }

    #[test]
    fn an_integer_is_range_checked_and_a_bad_one_is_a_field_error() {
        let mut config = json!({ "server": { "port": 1, "discovery_port": 2 } });
        let errors = overlay(
            &mut config,
            "server.port",
            &form_from(&[("server.port", "99999")]),
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, "server.port");
        assert_eq!(errors[0].msg, "must be a whole number between 0 and 65535");
        assert_eq!(
            config["server"]["port"], 1,
            "a rejected value leaves the prior one"
        );
        let errors = overlay(
            &mut config,
            "server.port",
            &form_from(&[("server.port", "fast")]),
        );
        assert_eq!(errors.len(), 1);
        let errors = overlay(
            &mut config,
            "server.port",
            &form_from(&[("server.port", "4247")]),
        );
        assert_eq!(errors, Vec::new());
        assert_eq!(config["server"]["port"], 4247);
    }

    #[test]
    fn a_cleared_integer_is_null_when_optional_and_an_error_when_required() {
        let mut config = json!({ "server": { "port": 11119, "discovery_port": 32227 } });
        let errors = overlay(
            &mut config,
            "server.discovery_port",
            &form_from(&[("server.discovery_port", "")]),
        );
        assert_eq!(errors, Vec::new());
        assert!(config["server"]["discovery_port"].is_null());
        let errors = overlay(
            &mut config,
            "server.port",
            &form_from(&[("server.port", "")]),
        );
        assert_eq!(errors[0].path, "server.port");
        assert_eq!(errors[0].msg, "is required");
        assert_eq!(
            config["server"]["port"], 11119,
            "the prior value stays for the re-render"
        );
        let mut config = json!({ "serial": { "gain": 1.5 } });
        let errors = overlay(
            &mut config,
            "serial.gain",
            &form_from(&[("serial.gain", " ")]),
        );
        assert_eq!(errors[0].msg, "is required");
        assert_eq!(config["serial"]["gain"], 1.5);
    }

    #[test]
    fn an_absent_optional_key_is_created_by_a_submission() {
        // Absence is the spelling of "unset", so the file may lack the key;
        // the page must still be able to set it.
        let mut config = json!({ "server": { "port": 11119 } });
        overlay(
            &mut config,
            "server.discovery_port",
            &form_from(&[("server.discovery_port", "32227")]),
        );
        assert_eq!(config["server"]["discovery_port"], 32227);
    }

    #[test]
    fn a_float_parses_and_a_bad_one_is_a_field_error() {
        let mut config = json!({ "serial": { "gain": 1.0 } });
        overlay(
            &mut config,
            "serial.gain",
            &form_from(&[("serial.gain", "2.5")]),
        );
        assert_eq!(config["serial"]["gain"], 2.5);
        let errors = overlay(
            &mut config,
            "serial.gain",
            &form_from(&[("serial.gain", "lots")]),
        );
        assert_eq!(errors[0].msg, "must be a number");
    }

    #[test]
    fn a_checkbox_group_collects_in_enum_order_and_rejects_strangers() {
        let mut config = json!({ "cooling": { "targets": [0] } });
        overlay(
            &mut config,
            "cooling.targets",
            &form_from(&[("cooling.targets", "0"), ("cooling.targets", "-10")]),
        );
        assert_eq!(config["cooling"]["targets"], json!([-10, 0]));
        overlay(&mut config, "cooling.targets", &form_from(&[]));
        assert_eq!(config["cooling"]["targets"], json!([]));
        let errors = overlay(
            &mut config,
            "cooling.targets",
            &form_from(&[("cooling.targets", "7")]),
        );
        assert_eq!(errors[0].path, "cooling.targets");
        assert_eq!(config["cooling"]["targets"], json!([]));
    }

    #[test]
    fn a_blank_secret_means_unchanged() {
        let mut config = json!({ "serial": { "key": "stored" } });
        overlay(&mut config, "serial.key", &form_from(&[("serial.key", "")]));
        assert_eq!(config["serial"]["key"], "stored");
        overlay(
            &mut config,
            "serial.key",
            &form_from(&[("serial.key", "fresh")]),
        );
        assert_eq!(config["serial"]["key"], "fresh");
    }

    fn transport_model() -> FieldModel {
        let schema = json!({
            "type": "object",
            "properties": {
                "transport": {
                    "oneOf": [
                        { "type": "object", "properties": { "kind": { "const": "usb" }, "port": { "type": "string" }, "baud_rate": { "type": "integer" } } },
                        { "type": "object", "properties": { "kind": { "const": "udp" }, "host": { "type": "string" }, "port": { "type": "integer" } } },
                    ],
                },
            },
        });
        FieldModel::from_schema(&schema)
    }

    #[test]
    fn a_variant_change_replaces_the_enum_object() {
        let model = transport_model();
        let spec = model.field("transport").unwrap();
        let mut config =
            json!({ "transport": { "kind": "usb", "port": "/dev/a", "baud_rate": 9600 } });
        let form = form_from(&[
            ("transport", "udp"),
            ("transport:udp.host", "mount.local"),
            ("transport:udp.port", "11880"),
            ("transport:usb.port", "/dev/ignored"),
        ]);
        let mut errors = Vec::new();
        overlay_field(&mut config, spec, &form, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            config["transport"],
            json!({ "kind": "udp", "host": "mount.local", "port": 11880 }),
            "the usb fields must not linger beside the udp tag"
        );
    }

    #[test]
    fn the_same_variant_keeps_its_unsubmitted_fields() {
        let model = transport_model();
        let spec = model.field("transport").unwrap();
        let mut config =
            json!({ "transport": { "kind": "usb", "port": "/dev/a", "baud_rate": 9600 } });
        let form = form_from(&[("transport", "usb"), ("transport:usb.port", "/dev/b")]);
        let mut errors = Vec::new();
        overlay_field(&mut config, spec, &form, &mut errors);
        assert_eq!(
            config["transport"],
            json!({ "kind": "usb", "port": "/dev/b", "baud_rate": 9600 })
        );
        let form = form_from(&[("transport", "serial")]);
        overlay_field(&mut config, spec, &form, &mut errors);
        assert_eq!(errors[0].path, "transport");
    }

    #[test]
    fn a_skipped_variant_field_keeps_what_the_file_has() {
        // A command-line override pins `transport.port`: the form still
        // carries the control, and the enum's overlay must leave it alone
        // while writing the variant's other fields.
        let model = transport_model();
        let spec = model.field("transport").unwrap();
        let mut config =
            json!({ "transport": { "kind": "usb", "port": "/dev/file", "baud_rate": 9600 } });
        let form = form_from(&[
            ("transport", "usb"),
            ("transport:usb.port", "/dev/tampered"),
            ("transport:usb.baud_rate", "4800"),
        ]);
        let mut errors = Vec::new();
        overlay_field_except(
            &mut config,
            spec,
            &form,
            &["transport:usb.port".to_string()],
            &mut errors,
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            config["transport"],
            json!({ "kind": "usb", "port": "/dev/file", "baud_rate": 4800 })
        );
    }

    #[test]
    fn a_file_only_leaf_is_never_overlaid() {
        let schema = json!({
            "type": "object",
            "properties": { "server": { "type": "object", "properties": {
                "tls": { "anyOf": [ { "type": "object", "properties": { "cert": { "type": "string" } } }, { "type": "null" } ] },
            } } },
        });
        let model = FieldModel::from_schema(&schema);
        let spec = model.field("server.tls").unwrap();
        let mut config = json!({ "server": { "tls": { "cert": "/etc/cert.pem" } } });
        let mut errors = Vec::new();
        overlay_field(
            &mut config,
            spec,
            &form_from(&[("server.tls", "gone")]),
            &mut errors,
        );
        assert_eq!(config["server"]["tls"]["cert"], "/etc/cert.pem");
    }

    #[test]
    fn value_helpers_read_what_an_input_shows() {
        let config = json!({ "a": { "s": "x", "n": 3, "b": true, "arr": [1, 2], "o": {} } });
        assert_eq!(text_at(&config, "/a/s"), "x");
        assert_eq!(text_at(&config, "/a/n"), "3");
        assert_eq!(text_at(&config, "/a/b"), "true");
        assert_eq!(text_at(&config, "/a/o"), "");
        assert_eq!(text_at(&config, "/a/missing"), "");
        assert!(bool_at(&config, "/a/b"));
        assert!(!bool_at(&config, "/a/s"));
        assert!(array_contains(&config, "/a/arr", 2));
        assert!(!array_contains(&config, "/a/arr", 5));
        assert_eq!(text_of(&json!(1.5)), "1.5");
        assert_eq!(text_of(&Value::Null), "");
        assert!(array_has(&json!([1, 2]), 1));
        assert!(!array_has(&json!("not an array"), 1));
    }

    #[test]
    fn canonical_json_sorts_keys_recursively() {
        let value = json!({ "b": 1, "a": { "y": 2, "x": 3 }, "c": [ { "n": 1, "m": 2 } ] });
        assert_eq!(
            canonical_json(&value),
            r#"{"a":{"x":3,"y":2},"b":1,"c":[{"m":2,"n":1}]}"#
        );
    }

    #[test]
    fn changed_paths_names_leaves_that_differ() {
        let before = json!({ "a": { "x": 1, "y": 2 }, "b": "same", "gone": 1 });
        let after = json!({ "a": { "x": 1, "y": 3 }, "b": "same", "new": 1 });
        assert_eq!(changed_paths(&before, &after), vec!["a.y", "gone", "new"]);
        assert_eq!(changed_paths(&before, &before), Vec::<String>::new());
    }
}
