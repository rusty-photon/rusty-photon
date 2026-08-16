//! Applying machine-applicable fixes (docs/services/doctor.md §Repair).
//!
//! Fixes are primitive JSON-pointer operations ([`FixOp`]) the checks
//! planned. They are grouped per service file and applied as one
//! read-modify-write through `rusty_photon_config::save` — the same atomic
//! temp→fsync→rename→fsync-dir path the services' own `config.apply` uses —
//! so a crash mid-fix never corrupts a config, and every field doctor does
//! not touch is preserved (the mutation is on the raw JSON value, not a
//! typed round-trip; `save` normalizes formatting to the same
//! pretty-printed shape `config.apply` writes). `save` also carries the
//! replaced file's owner and mode onto the new inode, so a sudo'd doctor —
//! the only way to reach a packaged install's `0750` config root — never
//! strands a config root-owned and unreadable by its service.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use tracing::{debug, warn};

use crate::report::{AppliedFix, Check, FixOp};

/// Apply every fix planned by `checks`, grouped per service file. Returns
/// the ops actually applied (an op whose target is already gone is skipped
/// silently — another fix round or a concurrent edit got there first).
/// A read or write error on one file aborts with an error: half-applied
/// repair must be reported, not glossed over.
pub fn apply_fixes(config_dir: &Path, checks: &[Check]) -> Result<Vec<AppliedFix>, String> {
    let mut ops = Vec::new();
    for check in checks {
        for op in &check.fixes {
            ops.push((check.name.clone(), op.clone()));
        }
    }
    apply_ops(config_dir, ops, false)
}

/// Apply a list of `(originating check name, op)` pairs, grouped per
/// service file — [`apply_fixes`]'s engine, also driven directly by the
/// provisioning pass and `doctor auth rotate`. `overwrite` governs
/// [`FixOp::SetObject`] only: `false` (everything but rotate) preserves
/// present blocks as operator intent.
pub fn apply_ops(
    config_dir: &Path,
    ops: Vec<(String, FixOp)>,
    overwrite: bool,
) -> Result<Vec<AppliedFix>, String> {
    let mut by_service: BTreeMap<String, Vec<(String, FixOp)>> = BTreeMap::new();
    for (check_name, op) in ops {
        let Some(service) = op.service() else {
            warn!("skipping a fix op this doctor build cannot apply: {op}");
            continue;
        };
        by_service
            .entry(service.to_string())
            .or_default()
            .push((check_name, op));
    }

    let mut applied = Vec::new();
    for (service, ops) in by_service {
        let path = config_dir.join(format!("{service}.json"));
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot re-read {} to fix it: {e}", path.display()))?;
        let mut value: Value = serde_json::from_str(&content)
            .map_err(|e| format!("{} is no longer valid JSON: {e}", path.display()))?;

        let mut wrote_any = false;
        for (check_name, op) in ops {
            if apply_op(&mut value, &op, overwrite) {
                debug!("applied: {op}");
                wrote_any = true;
                applied.push(AppliedFix {
                    check: check_name,
                    op,
                });
            } else {
                debug!("fix target already gone or kept, skipping: {op}");
            }
        }
        if wrote_any {
            rusty_photon_config::save(&path, &value)
                .map_err(|e| format!("could not write {}: {e}", path.display()))?;
        }
    }
    Ok(applied)
}

/// Apply one op to a config value. Returns whether anything changed. Ops
/// never create intermediate structure — the pointer was derived from the
/// same file moments ago, so a missing *parent* means the target is
/// already gone — but the final key is created when absent (the whole
/// point of the provisioning ops).
fn apply_op(value: &mut Value, op: &FixOp, overwrite: bool) -> bool {
    match op {
        FixOp::SetNumber {
            pointer, value: n, ..
        } => upsert(value, pointer, Value::from(*n), true),
        FixOp::SetString {
            pointer, value: s, ..
        } => upsert(value, pointer, Value::from(s.as_str()), true),
        FixOp::SetObject {
            pointer, value: v, ..
        } => upsert(value, pointer, v.clone(), overwrite),
        FixOp::RemoveKey { pointer, .. } => remove(value, pointer),
        // Provisioning actions are performed against the pki tree, never as
        // config-pointer ops; an unknown op from a newer binary cannot be
        // applied at all.
        FixOp::GenerateCa
        | FixOp::GenerateCert { .. }
        | FixOp::MintCredential
        | FixOp::RenewAcme { .. }
        | FixOp::Unknown => false,
    }
}

/// Set `pointer` to `new`, creating the final key when absent (the parent
/// must exist and be an object). With `overwrite` false an existing
/// non-null value is left alone — a present block is operator intent.
fn upsert(value: &mut Value, pointer: &str, new: Value, overwrite: bool) -> bool {
    let Some((parent_ptr, key)) = pointer.rsplit_once('/') else {
        return false;
    };
    let key = key.replace("~1", "/").replace("~0", "~");
    match value.pointer_mut(parent_ptr) {
        Some(Value::Object(map)) => match map.get(&key) {
            Some(existing) if *existing == new => false,
            Some(existing) if !existing.is_null() && !overwrite => false,
            _ => {
                map.insert(key, new);
                true
            }
        },
        _ => false,
    }
}

fn remove(value: &mut Value, pointer: &str) -> bool {
    let Some((parent_ptr, key)) = pointer.rsplit_once('/') else {
        return false;
    };
    let key = key.replace("~1", "/").replace("~0", "~");
    match value.pointer_mut(parent_ptr) {
        Some(Value::Object(map)) => map.remove(&key).is_some(),
        _ => false,
    }
}

/// Escape one JSON-pointer reference token (RFC 6901).
#[must_use]
pub fn escape_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn sample() -> Value {
        serde_json::json!({
            "server": { "port": 11119 },
            "drivers": { "a/b": { "base_url": "http://x" } },
            "keep": [1, 2, 3]
        })
    }

    #[test]
    fn test_upsert_changes_and_reports_noop() {
        let mut v = sample();
        assert!(upsert(&mut v, "/server/port", Value::from(11113u64), true));
        assert_eq!(v["server"]["port"], 11113);
        assert!(
            !upsert(&mut v, "/server/port", Value::from(11113u64), true),
            "setting the same value is a no-op"
        );
        assert!(
            !upsert(&mut v, "/absent/port", Value::from(1u64), true),
            "a missing parent is never created"
        );
    }

    #[test]
    fn test_upsert_creates_the_final_key_but_respects_present_values() {
        let mut v = sample();
        let block = serde_json::json!({ "cert": "/p/c.pem", "key": "/p/k.pem" });
        assert!(
            upsert(&mut v, "/server/tls", block.clone(), false),
            "an absent key is created"
        );
        assert_eq!(v["server"]["tls"], block);

        let other = serde_json::json!({ "cert": "/other.pem", "key": "/other-key.pem" });
        assert!(
            !upsert(&mut v, "/server/tls", other.clone(), false),
            "a present block is operator intent"
        );
        assert_eq!(v["server"]["tls"], block);

        assert!(
            upsert(&mut v, "/server/tls", other.clone(), true),
            "overwrite (rotation) replaces it"
        );
        assert_eq!(v["server"]["tls"], other);

        v["server"]["auth"] = Value::Null;
        let auth = serde_json::json!({ "username": "observatory", "password_hash": "h" });
        assert!(
            upsert(&mut v, "/server/auth", auth.clone(), false),
            "an explicit null still means absent"
        );
        assert_eq!(v["server"]["auth"], auth);
    }

    #[test]
    fn test_remove_deletes_and_tolerates_absent() {
        let mut v = sample();
        assert!(remove(&mut v, "/server/port"));
        assert!(v["server"].get("port").is_none());
        assert!(!remove(&mut v, "/server/port"), "second removal is a no-op");
        assert!(!remove(&mut v, "/absent/key"));
        assert!(!remove(&mut v, "/keep/0"), "array elements are not keys");
        assert!(!remove(&mut v, "no-slash"), "a token without a parent");
    }

    #[test]
    fn test_apply_op_covers_every_variant() {
        let mut v = sample();
        assert!(apply_op(
            &mut v,
            &FixOp::SetNumber {
                service: "x".to_string(),
                pointer: "/server/port".to_string(),
                value: 1,
            },
            false,
        ));
        assert!(apply_op(
            &mut v,
            &FixOp::SetString {
                service: "x".to_string(),
                pointer: "/drivers/a~1b/base_url".to_string(),
                value: "http://y".to_string(),
            },
            false,
        ));
        assert!(apply_op(
            &mut v,
            &FixOp::SetObject {
                service: "x".to_string(),
                pointer: "/server/tls".to_string(),
                value: serde_json::json!({ "cert": "c", "key": "k" }),
            },
            false,
        ));
        assert!(apply_op(
            &mut v,
            &FixOp::RemoveKey {
                service: "x".to_string(),
                pointer: "/keep".to_string(),
            },
            false,
        ));
        for op in [
            FixOp::GenerateCa,
            FixOp::MintCredential,
            FixOp::RenewAcme {
                domain: "observatory.example.com".to_string(),
            },
            FixOp::Unknown,
        ] {
            assert!(
                !apply_op(&mut v, &op, true),
                "{op} is never a config-pointer application"
            );
        }
        assert!(
            !apply_op(
                &mut v,
                &FixOp::GenerateCert {
                    service: "x".to_string()
                },
                true
            ),
            "certificate issuance is not a config-pointer application"
        );
    }

    #[test]
    fn test_remove_unescapes_pointer_tokens() {
        let mut v = sample();
        assert!(remove(&mut v, &format!("/drivers/{}", escape_token("a/b"))));
        assert!(v["drivers"].get("a/b").is_none());
    }

    #[test]
    fn test_apply_fixes_groups_writes_and_preserves_untouched_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qhy-focuser.json");
        std::fs::write(
            &path,
            r#"{ "server": { "port": 11119 }, "device_overrides": { "keep": true } }"#,
        )
        .unwrap();
        let checks = vec![Check::fail(
            "ports.collision",
            Some("qhy-focuser".to_string()),
            "collision",
            None,
        )
        .with_fixes(vec![FixOp::SetNumber {
            service: "qhy-focuser".to_string(),
            pointer: "/server/port".to_string(),
            value: 11113,
        }])];

        let applied = apply_fixes(dir.path(), &checks).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].check, "ports.collision");

        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["server"]["port"], 11113);
        assert_eq!(
            back["device_overrides"]["keep"], true,
            "untouched fields survive"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_apply_fixes_preserves_owner_and_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sentinel.json");
        std::fs::write(&path, r#"{ "server": {}, "services": {} }"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        // Root (privileged CI) exercises the real failure shape: a sudo'd
        // doctor rewriting a config owned by the service user. Unprivileged
        // runs still pin owner and mode across the inode swap.
        let cross_owner = std::os::unix::fs::chown(&path, Some(12345), Some(12345)).is_ok();
        let before = std::fs::metadata(&path).unwrap();

        let checks = vec![Check::fail(
            "config.retired-keys",
            Some("sentinel".to_string()),
            "retired",
            None,
        )
        .with_fixes(vec![FixOp::RemoveKey {
            service: "sentinel".to_string(),
            pointer: "/services".to_string(),
        }])];
        let applied = apply_fixes(dir.path(), &checks).unwrap();
        assert_eq!(applied.len(), 1);

        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(
            after.permissions().mode() & 0o7777,
            0o640,
            "mode must survive the fix write"
        );
        assert_eq!(
            (after.uid(), after.gid()),
            (before.uid(), before.gid()),
            "owner must survive the fix write (cross-owner run: {cross_owner})"
        );
        if cross_owner {
            assert_eq!((after.uid(), after.gid()), (12345, 12345));
        }
    }

    #[test]
    fn test_apply_fixes_skips_already_gone_targets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sentinel.json"), r#"{ "server": {} }"#).unwrap();
        let checks = vec![Check::fail(
            "config.retired-keys",
            Some("sentinel".to_string()),
            "retired",
            None,
        )
        .with_fixes(vec![FixOp::RemoveKey {
            service: "sentinel".to_string(),
            pointer: "/services".to_string(),
        }])];
        let applied = apply_fixes(dir.path(), &checks).unwrap();
        assert!(applied.is_empty(), "nothing to remove, nothing recorded");
    }

    #[cfg(unix)]
    #[test]
    fn test_apply_fixes_errors_when_the_write_fails() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sentinel.json"), r#"{ "services": {} }"#).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let checks = vec![Check::fail(
            "config.retired-keys",
            Some("sentinel".to_string()),
            "retired",
            None,
        )
        .with_fixes(vec![FixOp::RemoveKey {
            service: "sentinel".to_string(),
            pointer: "/services".to_string(),
        }])];
        let result = apply_fixes(dir.path(), &checks);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        match result {
            // Running privileged (root CI containers) writes anyway.
            Ok(applied) => assert_eq!(applied.len(), 1),
            Err(err) => assert!(err.contains("could not write"), "{err}"),
        }
    }

    #[test]
    fn test_apply_fixes_errors_when_the_file_vanished() {
        let dir = tempfile::tempdir().unwrap();
        let checks = vec![Check::fail(
            "config.retired-keys",
            Some("sentinel".to_string()),
            "retired",
            None,
        )
        .with_fixes(vec![FixOp::RemoveKey {
            service: "sentinel".to_string(),
            pointer: "/services".to_string(),
        }])];
        let err = apply_fixes(dir.path(), &checks).unwrap_err();
        assert!(err.contains("sentinel.json"), "{err}");
    }
}
