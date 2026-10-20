//! `cargo ci repin-bazel-lock`: refreshes `MODULE.bazel.lock` after a change
//! to `Cargo.lock` or any workspace `Cargo.toml` (the root's or a member's),
//! and stages it.
//!
//! Bazel's `crate_universe` reads the Cargo files, and the lock records a hash
//! of each one, so any edit to them leaves the lock stale and CI's
//! `--lockfile_mode=error` rejects it (docs/skills/pre-push.md, "Refreshing
//! MODULE.bazel.lock"). The pre-commit hook catches the miss through
//! `tools/ci/check_bazel_lock.py`; this is the fix it asks for.
//!
//! The "Repin crate index" step of .github/workflows/repin-bazel.yml runs the
//! same repin on dependabot PRs. Keep the two in step.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::process::{Cmd, Error, Runner};

const LOCK: &str = "MODULE.bazel.lock";

/// The files `crate_universe` hashes, as git pathspecs.
const CARGO_FILES: [&str; 2] = ["Cargo.lock", "*Cargo.toml"];

/// A Bazel run of the repin, and whether it forces `crate_universe` to
/// re-resolve.
struct Step {
    args: &'static [&'static str],
    forced: bool,
}

const STEPS: [Step; 4] = [
    // Re-resolve the crate index against the current Cargo files.
    Step {
        args: &["mod", "tidy"],
        forced: true,
    },
    // The second, un-forced run resets the lock's recorded CARGO_BAZEL_REPIN
    // fingerprint to null, so the committed lock does not churn on later
    // plain `bazel` runs.
    Step {
        args: &["mod", "tidy"],
        forced: false,
    },
    // `bazel mod tidy` only fixes up extensions it reaches through an
    // explicit `use_repo()` in MODULE.bazel, so one pulled in transitively,
    // with no `use_repo()` of its own, can be left out of the lock on some
    // hosts. A `--lockfile_mode=update` build does the full target-graph
    // analysis and fills the gap; without it the lock can look fine here and
    // still be rejected by CI's `--lockfile_mode=error` on x86_64.
    Step {
        args: &["build", "--nobuild", "--lockfile_mode=update", "//..."],
        forced: false,
    },
    // Resolve once more in CI's mode. On this host only: it cannot see a gap
    // that shows up on another one, which is what the step above is for.
    Step {
        args: &["build", "--nobuild", "--lockfile_mode=error", "//..."],
        forced: false,
    },
];

/// Whether `name` is one of `crate_universe`'s environment variables. The lock
/// records them, and CI sets none of them, so one left exported in the
/// developer's shell would produce a lock that passes every step here and is
/// still rejected on the PR. `tools/ci/check_bazel_lock.py` refuses such a lock
/// by the same names.
fn is_crate_universe_var(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name == "REPIN" || name.starts_with("CARGO_BAZEL_"))
}

/// Refreshes and stages the lock, and returns what to tell the developer.
/// `env` names the variables in the environment the commands would inherit.
///
/// # Errors
///
/// When a command cannot be started or fails. Nothing after it runs, so a
/// failed repin never stages the lock.
pub fn repin(
    runner: &mut impl Runner,
    env: impl IntoIterator<Item = OsString>,
) -> Result<String, Error> {
    let root = runner.read(&Cmd::new("git", &["rev-parse", "--show-toplevel"]))?;
    let root = PathBuf::from(root.trim_end());
    let scrubbed: Vec<OsString> = env
        .into_iter()
        .filter(|name| is_crate_universe_var(name))
        .collect();

    for step in &STEPS {
        let mut cmd = Cmd::new("bazel", step.args)
            .dir(&root)
            .env_remove(&scrubbed);
        if step.forced {
            cmd = cmd.env("CARGO_BAZEL_REPIN", "1");
        }
        runner.run(&cmd)?;
    }
    runner.run(&Cmd::new("git", &["add", LOCK]).dir(&root))?;

    // The repin hashed the working tree, so the lock matches the Cargo files
    // as they are on disk. Any of them left unstaged would make the commit
    // disagree with its own lock; the generator can also have rewritten
    // Cargo.lock itself. Untracked manifests count too: a new member's
    // Cargo.toml is one until added.
    let modified = runner.read(
        &Cmd::new(
            "git",
            &["diff", "--name-only", "--", CARGO_FILES[0], CARGO_FILES[1]],
        )
        .dir(&root),
    )?;
    let untracked = runner.read(
        &Cmd::new(
            "git",
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "--",
                CARGO_FILES[0],
                CARGO_FILES[1],
            ],
        )
        .dir(&root),
    )?;
    Ok(report(&unstaged(&modified, &untracked)))
}

/// The Cargo files git lists as modified and not staged, then those it lists
/// as untracked.
fn unstaged<'a>(modified: &'a str, untracked: &'a str) -> Vec<&'a str> {
    modified
        .lines()
        .chain(untracked.lines())
        .filter(|path| !path.is_empty())
        .collect()
}

fn report(unstaged: &[&str]) -> String {
    if unstaged.is_empty() {
        return format!("{LOCK} refreshed and staged.");
    }
    let mut report =
        format!("{LOCK} refreshed and staged, from Cargo files that are not staged:\n");
    for path in unstaged {
        report.push_str("  ");
        report.push_str(path);
        report.push('\n');
    }
    report.push_str("Stage them too (git add), or the pre-commit check will refuse the commit.");
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::Fake;
    use std::path::Path;

    const ROOT: &str = "/home/dev/rusty-photon\n";

    #[test]
    fn runs_the_repin_then_stages_the_lock_then_looks_for_unstaged_cargo_files() {
        let mut fake = Fake::answering(&[ROOT]);

        repin(&mut fake, []).unwrap();

        assert_eq!(
            fake.lines(),
            [
                "git rev-parse --show-toplevel",
                "CARGO_BAZEL_REPIN=1 bazel mod tidy",
                "bazel mod tidy",
                "bazel build --nobuild --lockfile_mode=update //...",
                "bazel build --nobuild --lockfile_mode=error //...",
                "git add MODULE.bazel.lock",
                "git diff --name-only -- Cargo.lock *Cargo.toml",
                "git ls-files --others --exclude-standard -- Cargo.lock *Cargo.toml",
            ]
        );
    }

    #[test]
    fn runs_everything_after_finding_the_root_in_the_root() {
        let mut fake = Fake::answering(&[ROOT]);

        repin(&mut fake, []).unwrap();

        let (find_root, rest) = fake.ran.split_first().unwrap();
        assert_eq!(find_root.dir, None);
        for cmd in rest {
            assert_eq!(
                cmd.dir.as_deref(),
                Some(Path::new("/home/dev/rusty-photon")),
                "{cmd}"
            );
        }
    }

    #[test]
    fn takes_crate_universes_variables_out_of_every_bazel_run() {
        let mut fake = Fake::answering(&[ROOT]);
        let env = [
            "PATH",
            "CARGO_BAZEL_REPIN",
            "CARGO_HOME",
            "REPIN",
            "CARGO_BAZEL_ISOLATED",
        ]
        .map(OsString::from);

        repin(&mut fake, env).unwrap();

        let bazel: Vec<&Cmd> = fake
            .ran
            .iter()
            .filter(|cmd| cmd.program == "bazel")
            .collect();
        assert_eq!(bazel.len(), STEPS.len());
        for cmd in bazel {
            assert_eq!(
                cmd.env_remove,
                ["CARGO_BAZEL_REPIN", "REPIN", "CARGO_BAZEL_ISOLATED"].map(OsString::from),
                "{cmd}"
            );
        }
    }

    #[test]
    fn crate_universe_variables_are_repin_and_the_cargo_bazel_family() {
        for name in ["REPIN", "CARGO_BAZEL_REPIN", "CARGO_BAZEL_GENERATOR_URL"] {
            assert!(is_crate_universe_var(OsStr::new(name)), "{name}");
        }
        for name in ["CARGO_HOME", "CARGO_BAZEL", "REPINNED", "repin", "PATH"] {
            assert!(!is_crate_universe_var(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn a_failed_step_stops_the_repin_before_the_lock_is_staged() {
        let mut fake = Fake {
            fail_at: Some(2),
            ..Fake::answering(&[ROOT])
        };

        let error = repin(&mut fake, []).unwrap_err();

        assert_eq!(error.to_string(), "`bazel mod tidy` failed: exit status: 1");
        assert_eq!(fake.ran.len(), 3, "{:?}", fake.lines());
    }

    #[test]
    fn reports_a_staged_lock_when_every_cargo_file_is_staged() {
        let mut fake = Fake::answering(&[ROOT, "", ""]);

        let report = repin(&mut fake, []).unwrap();

        assert_eq!(report, "MODULE.bazel.lock refreshed and staged.");
    }

    #[test]
    fn reports_modified_then_untracked_cargo_files_left_unstaged() {
        let mut fake = Fake::answering(&[
            ROOT,
            "Cargo.lock\nservices/rp/Cargo.toml\n",
            "crates/new-member/Cargo.toml\n",
        ]);

        let report = repin(&mut fake, []).unwrap();

        assert_eq!(
            report,
            "MODULE.bazel.lock refreshed and staged, from Cargo files that are not staged:\n\
             \x20 Cargo.lock\n\
             \x20 services/rp/Cargo.toml\n\
             \x20 crates/new-member/Cargo.toml\n\
             Stage them too (git add), or the pre-commit check will refuse the commit."
        );
    }

    #[test]
    fn unstaged_skips_blank_lines() {
        assert_eq!(unstaged("\nCargo.lock\n\n", "\r\n"), ["Cargo.lock"]);
    }
}
