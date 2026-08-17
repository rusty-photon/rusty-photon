//! The per-process scratch directory every harness-written file lives in:
//! rp configs, session registries, data directories.
//!
//! One directory per test process, created on first use with a random name
//! component, so two paths can only collide if they were minted by the same
//! process — a machine-wide temp directory (which is what `temp_dir()` is
//! under Bazel on every OS: `TEST_TMPDIR` is set, `TMP`/`TEMP`/`TMPDIR` are
//! not re-pointed) can therefore never hand a scenario another process's
//! leftovers. That matters for the session registry in particular: rp keeps
//! an active registry across a stop on purpose (that is its restart
//! recovery), so a registry left by an earlier process at a name a later
//! process reuses — same PID, same sequence number, which Windows recycles
//! freely — would be *restored*, and the scenario would start inside a
//! session it never created.
//!
//! The directory sits under Bazel's per-action `TEST_TMPDIR` when present
//! (Bazel wipes it before every run) and under the system temp directory
//! otherwise, where it accumulates like any other cargo-run scratch.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tempfile::TempDir;

/// The process-wide scratch directory. Created on first call; the same
/// path for every later call in this process. Never deleted from inside
/// the process — the guard lives for the process's lifetime so the paths
/// stay valid for every child rp the process spawns.
pub(super) fn scratch_dir() -> &'static Path {
    static DIR: OnceLock<TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let root = scratch_root();
        tempfile::Builder::new()
            .prefix("rp-test-")
            .tempdir_in(&root)
            .unwrap_or_else(|e| {
                panic!(
                    "cannot create the harness scratch directory under {}: {e}",
                    root.display()
                )
            })
    })
    .path()
}

/// Where the scratch directory is created: `TEST_TMPDIR` under Bazel, the
/// system temp directory otherwise.
fn scratch_root() -> PathBuf {
    std::env::var_os("TEST_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn scratch_dir_is_created_once_and_reused() {
        let first = scratch_dir();
        let second = scratch_dir();
        assert_eq!(first, second);
        assert!(first.is_dir(), "{} should exist", first.display());
    }

    #[test]
    fn scratch_dir_lives_under_the_root_with_a_random_component() {
        let dir = scratch_dir();
        let root = scratch_root();
        assert_eq!(dir.parent().unwrap(), root.as_path());
        let name = dir.file_name().unwrap().to_str().unwrap();
        let suffix = name.strip_prefix("rp-test-").unwrap();
        assert!(
            !suffix.is_empty(),
            "the directory name needs a random component, got {name:?}"
        );
    }
}
