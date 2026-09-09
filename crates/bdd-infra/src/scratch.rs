//! Scratch directories for tests that write files a service reads.
//!
//! A test's scratch path has to be unique *by construction*, not by naming.
//! Two processes running the same test on one machine — two worktrees, a
//! `cargo test` alongside a `bazel test`, `--runs_per_test`, a re-run
//! overlapping a straggler — pick the same fixed name, write the same files
//! into it, and whichever finishes first deletes the other's config out from
//! under a live child process. A random name component removes the shared
//! path, and the [`TempDir`] guard removes it again on drop, so the early
//! returns a `?` takes clean up as reliably as the success path does.
//!
//! The directory is created under Bazel's per-action `TEST_TMPDIR` when that
//! is set and under the system temp directory otherwise. Bazel re-points
//! `TEST_TMPDIR` only — `TMPDIR`, `TMP` and `TEMP` keep pointing at the
//! machine-wide temp directory on every OS — so a scratch path built from
//! [`std::env::temp_dir`] escapes the per-action tmpdir Bazel wipes between
//! runs and accumulates on the machine instead.

use std::io;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

/// Where scratch directories are created: `TEST_TMPDIR` under Bazel, the
/// system temp directory otherwise.
pub(crate) fn root() -> PathBuf {
    std::env::var_os("TEST_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from)
}

/// A fresh scratch directory named `<prefix><random>`, deleted with its
/// contents when the returned guard drops.
///
/// Hold the guard for as long as the paths under it are in use — a child
/// process reading a config written there keeps needing it until it exits.
///
/// # Errors
///
/// Returns an [`io::Error`] naming the root it tried if the directory cannot
/// be created. The root comes from the environment, so a caller that only
/// propagates the bare OS error ("Permission denied") reports nothing a
/// reader can act on.
pub fn new_dir(prefix: &str) -> io::Result<TempDir> {
    new_dir_in(&root(), prefix)
}

fn new_dir_in(root: &Path, prefix: &str) -> io::Result<TempDir> {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(root)
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("creating a scratch directory under {}: {e}", root.display()),
            )
        })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn new_dir_creates_a_directory_under_the_root() {
        let dir = new_dir("scratch-test-").unwrap();
        assert!(dir.path().is_dir(), "{} should exist", dir.path().display());
        assert_eq!(dir.path().parent().unwrap(), root().as_path());
    }

    #[test]
    fn new_dir_keeps_the_prefix_and_adds_a_random_component() {
        let dir = new_dir("scratch-test-").unwrap();
        let name = dir.path().file_name().unwrap().to_str().unwrap();
        let suffix = name.strip_prefix("scratch-test-").unwrap();
        assert!(
            !suffix.is_empty(),
            "the directory name needs a random component, got {name:?}"
        );
    }

    #[test]
    fn two_calls_with_one_prefix_get_distinct_directories() {
        let first = new_dir("scratch-test-").unwrap();
        let second = new_dir("scratch-test-").unwrap();
        assert_ne!(first.path(), second.path());
    }

    #[test]
    fn the_directory_is_gone_once_the_guard_drops() {
        let dir = new_dir("scratch-test-").unwrap();
        let path = dir.path().to_path_buf();
        std::fs::write(path.join("config.json"), "{}").unwrap();
        drop(dir);
        assert!(!path.exists(), "{} should be deleted", path.display());
    }

    #[test]
    fn a_root_that_does_not_exist_fails_with_the_root_in_the_message() {
        let missing = root().join("bdd-infra-scratch-no-such-root");
        let error = new_dir_in(&missing, "scratch-test-").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(
            error.to_string().contains(&missing.display().to_string()),
            "the message should name the root it tried, got {error}"
        );
    }

    #[test]
    fn root_follows_test_tmpdir_when_bazel_sets_it() {
        // Bazel sets TEST_TMPDIR for every test action, including this one,
        // so under Bazel the root is that directory and under cargo it is
        // the system temp directory. Both arms are asserted here rather
        // than mutating the process environment, which every other test in
        // this process would see.
        match std::env::var_os("TEST_TMPDIR") {
            Some(bazel_tmp) => assert_eq!(root(), PathBuf::from(bazel_tmp)),
            None => assert_eq!(root(), std::env::temp_dir()),
        }
    }
}
