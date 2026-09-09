//! The per-process scratch directory every harness-written file lives in:
//! rp configs and data directories.
//!
//! One directory per test process, created on first use with a random name
//! component by [`crate::scratch::new_dir`], so two paths can only collide
//! if they were minted by the same process — a machine-wide temp directory
//! can therefore never hand a scenario another process's leftovers. That
//! matters for the data directory in particular: frames and the target store
//! survive a stop on purpose (progress is derived from them), so a directory
//! left by an earlier process at a name a later process reuses — same PID,
//! same sequence number, which Windows recycles freely — would be *reused*,
//! and the scenario would inherit progress it never captured.

use std::path::Path;
use std::sync::OnceLock;

use tempfile::TempDir;

/// The process-wide scratch directory. Created on first call; the same
/// path for every later call in this process. Never deleted from inside
/// the process — the guard lives for the process's lifetime so the paths
/// stay valid for every child rp the process spawns.
pub(super) fn scratch_dir() -> &'static Path {
    static DIR: OnceLock<TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        crate::scratch::new_dir("rp-test-")
            .unwrap_or_else(|e| panic!("cannot create the harness scratch directory: {e}"))
    })
    .path()
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
        assert_eq!(dir.parent().unwrap(), crate::scratch::root().as_path());
        let name = dir.file_name().unwrap().to_str().unwrap();
        let suffix = name.strip_prefix("rp-test-").unwrap();
        assert!(
            !suffix.is_empty(),
            "the directory name needs a random component, got {name:?}"
        );
    }
}
