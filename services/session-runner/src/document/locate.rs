//! `config.workflow` → document path resolution, per
//! `docs/services/session-runner.md` § Invocation: a relative name
//! resolves to `<workflows_dir>/<name>.json` and must stay inside
//! `workflows_dir`; an absolute path (explicit operator intent) is used
//! as-is.

use std::path::{Component, Path, PathBuf};

/// Resolves a `config.workflow` value to a document path.
///
/// A relative `name` may include subdirectories and may spell out the
/// `.json` suffix (it is appended when absent), but any component that
/// could escape `workflows_dir` (`..`, a rooted segment) is rejected.
pub fn resolve_workflow_path(workflows_dir: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty() {
        return Err("workflow name is empty".to_owned());
    }
    if Path::new(name).is_absolute() {
        return Ok(PathBuf::from(name));
    }
    // Containment is checked on the raw name, before the `.json` suffix
    // is appended — appending would otherwise turn a bare `..` into the
    // ordinary file name `...json`.
    for component in Path::new(name).components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(format!(
                    "workflow name `{name}` resolves outside the workflows directory"
                ));
            }
        }
    }
    let file = if Path::new(name).extension() == Some(std::ffi::OsStr::new("json")) {
        name.to_owned()
    } else {
        format!("{name}.json")
    };
    Ok(workflows_dir.join(file))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        PathBuf::from("workflows")
    }

    #[test]
    fn test_bare_name_gets_the_json_suffix() {
        let p = resolve_workflow_path(&dir(), "deep_sky").unwrap();
        assert_eq!(p, dir().join("deep_sky.json"));
    }

    #[test]
    fn test_explicit_suffix_is_not_doubled() {
        let p = resolve_workflow_path(&dir(), "deep_sky.json").unwrap();
        assert_eq!(p, dir().join("deep_sky.json"));
    }

    #[test]
    fn test_subdirectories_are_allowed() {
        let p = resolve_workflow_path(&dir(), "flats/narrowband").unwrap();
        assert_eq!(p, dir().join("flats").join("narrowband.json"));
    }

    #[test]
    fn test_parent_traversal_is_rejected() {
        for name in ["../etc/passwd", "a/../../b", ".."] {
            let err = resolve_workflow_path(&dir(), name).unwrap_err();
            assert!(
                err.contains("outside the workflows directory"),
                "{name}: {err}"
            );
        }
    }

    #[test]
    fn test_absolute_paths_are_used_verbatim() {
        let abs = std::env::temp_dir().join("wf.json");
        let abs_str = abs.to_string_lossy().into_owned();
        let p = resolve_workflow_path(&dir(), &abs_str).unwrap();
        assert_eq!(p, abs);
    }

    #[test]
    fn test_empty_name_is_rejected() {
        assert!(resolve_workflow_path(&dir(), "").is_err());
    }
}
