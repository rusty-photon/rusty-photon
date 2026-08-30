//! How a persisted config spells "unset".
//!
//! An unset optional field is spelled by the **absence** of its key, never
//! by an explicit `null`. Every optional field a service persists carries
//! `#[serde(skip_serializing_if = "Option::is_none")]` so that
//! `serde_json::to_value` — the round-trip `config.apply` persists through —
//! drops it instead of writing a null.
//!
//! Four things ride on that:
//!
//! 1. **A round-trip writes back only what the operator authored.** Without
//!    it, `config.apply` re-materialises every unset key as a null, so a key
//!    an operator deleted comes back on the next apply.
//! 2. **Cross-version parsing.** Every config shape is
//!    `deny_unknown_fields`, so an explicit null is a *present* key that a
//!    reader one schema-generation behind rejects — over a key carrying no
//!    information at all.
//! 3. **`null` and absent diverge outside the owning struct.** Doctor steps
//!    around most blocks as opaque values, ui-htmx reads other services'
//!    files, and operators use `jq`: `.server.tls != null` and `has("tls")`
//!    answer differently.
//! 4. **Legibility.** A file listing only what is set can be read to answer
//!    "what is configured here?". A null reads as deliberate when it means
//!    untouched.
//!
//! [`explicit_nulls`] is the guard: each service that persists a config
//! serialises its own default through it in a unit test, so a service that
//! grows an optional field without the attribute fails its own test rather
//! than shipping config files full of nulls.
//!
//! A field whose serde default is **not** `None` is the one shape where
//! `null` and absent genuinely differ — there the null is the only spelling
//! that means "off", and dropping the key would silently restore the
//! default. Such a field keeps its null and is documented at its definition
//! (`planetarium-bridge`'s `report_altitude_floor_deg` is the only one).
//! The guard does not trip over it, because a default config serialises it
//! as a value rather than as a null.

use serde_json::Value;

/// Every JSON pointer in `value` whose member is an explicit `null`.
///
/// Walks objects and arrays; the returned pointers are RFC 6901 (e.g.
/// `/server/tls`, `/cameras/0/auth`) and come back in document order.
#[must_use]
pub fn explicit_nulls(value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    walk(value, &mut String::new(), &mut found);
    found
}

fn walk(value: &Value, at: &mut String, found: &mut Vec<String>) {
    match value {
        Value::Null => found.push(at.clone()),
        Value::Object(map) => {
            for (key, child) in map {
                let restore = at.len();
                at.push('/');
                at.push_str(&escape(key));
                walk(child, at, found);
                at.truncate(restore);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                let restore = at.len();
                at.push('/');
                at.push_str(&index.to_string());
                walk(child, at, found);
                at.truncate(restore);
            }
        }
        _ => {}
    }
}

/// RFC 6901 token escaping: `~` becomes `~0` and `/` becomes `~1`, so a key
/// containing either cannot forge a pointer separator.
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_config_without_nulls_reports_none() {
        assert!(explicit_nulls(&json!({"server": {"port": 11115}})).is_empty());
    }

    #[test]
    fn a_null_member_is_reported_by_pointer() {
        assert_eq!(
            explicit_nulls(&json!({"server": {"tls": null, "port": 1}})),
            vec!["/server/tls".to_string()]
        );
    }

    #[test]
    fn nulls_inside_arrays_are_reported_by_index() {
        assert_eq!(
            explicit_nulls(&json!({"cameras": [{"auth": null}, {"auth": {}}]})),
            vec!["/cameras/0/auth".to_string()]
        );
    }

    #[test]
    fn every_null_is_reported_not_just_the_first() {
        assert_eq!(
            explicit_nulls(&json!({"a": null, "b": null})),
            vec!["/a".to_string(), "/b".to_string()]
        );
    }

    #[test]
    fn a_top_level_null_is_reported_at_the_root_pointer() {
        assert_eq!(explicit_nulls(&Value::Null), vec![String::new()]);
    }

    #[test]
    fn a_key_containing_a_separator_is_escaped_not_split() {
        // Without escaping this would report `/a/b`, indistinguishable from a
        // null at `b` nested inside `a`.
        assert_eq!(
            explicit_nulls(&json!({"a/b": null})),
            vec!["/a~1b".to_string()]
        );
    }

    #[test]
    fn a_key_containing_a_tilde_is_escaped() {
        assert_eq!(
            explicit_nulls(&json!({"a~b": null})),
            vec!["/a~0b".to_string()]
        );
    }
}
