//! Integration tests for the supervision module's spawn-based arms.
//!
//! These tests live in a `[[test]]` integration target (not in
//! `src/supervision.rs`'s `#[cfg(test)] mod tests`) because they need to
//! spawn the in-tree `mock_astap` binary, and `CARGO_BIN_EXE_*` is only
//! set by Cargo for `[[test]]` crates and is unset under Bazel.
//!
//! Discovery order (matches BDD's `world.rs` pattern):
//! 1. `MOCK_ASTAP_BINARY` env var (set by the Bazel test target).
//! 2. `option_env!("CARGO_BIN_EXE_mock_astap")` (set by Cargo for this
//!    `[[test]]` target).
//!
//! If neither resolves, the tests fail with a diagnostic naming both
//! mechanisms — `mock_astap` should always be present (Cargo provides
//! the env var automatically; Bazel sets it explicitly via the
//! BUILD.bazel `env` attribute).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    clippy::used_underscore_binding,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::float_cmp,
    clippy::similar_names,
    clippy::struct_excessive_bools
)]

use plate_solver::supervision::{spawn_with_deadline, SpawnOutcome, GRACE_PERIOD};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::process::Command;

/// Windows process-creation flag. Spawning the child in a new process
/// group is required for the supervision module's `CTRL_BREAK_EVENT`
/// to target only the child (and not propagate up to the test runner).
/// Same constant as `runner/astap.rs`.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

fn mock_astap_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("MOCK_ASTAP_BINARY") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }
    if let Some(p) = option_env!("CARGO_BIN_EXE_mock_astap") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn require_mock_astap() -> PathBuf {
    mock_astap_path().unwrap_or_else(|| {
        panic!(
            "mock_astap binary not found.\n  \
             Tried: MOCK_ASTAP_BINARY env var, then CARGO_BIN_EXE_mock_astap.\n  \
             Under Cargo: run `cargo build --tests -p plate-solver` first.\n  \
             Under Bazel: set MOCK_ASTAP_BINARY in the test target's env."
        )
    })
}

fn cmd_with_mode(mode: &str) -> Command {
    let bin = require_mock_astap();
    let mut cmd = Command::new(&bin);
    cmd.env("MOCK_ASTAP_MODE", mode);
    // Mock binary doesn't actually need -f for the modes we exercise here
    // (hang / ignore_sigterm), but pass one anyway so the argv shape is
    // representative of the real call.
    cmd.arg("-f").arg("/tmp/unused.fits");
    #[cfg(windows)]
    {
        // tokio::process::Command::creation_flags is inherent on
        // Windows — no trait import needed (would trip unused_imports
        // under -D warnings).
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    cmd
}

/// `ignore_sigterm` mode, but immune to SIGTERM from the child's very first
/// instruction rather than from the point its `main` installs `SIG_IGN`.
///
/// The mock installs `SIG_IGN` itself, but only after `exec`, the Rust runtime
/// start-up, an env read and a mode dispatch. The supervision deadline under
/// test is 100 ms, and `spawn_with_deadline` starts that clock inside itself —
/// there is no seam where a caller could wait for the child to report itself
/// ready. So SIGTERM arriving before the mock reaches its `signal` call finds
/// the *default* disposition and terminates the child, and the supervisor
/// correctly reports `TimedOutTerminated` where the test wants
/// `TimedOutKilled`. Measured on an idle 10-core arm64 host that start-up path
/// is ~5 ms against the 100 ms deadline, and ~19 ms with the CPU 4x
/// oversubscribed — a real margin, but a margin, and one that a
/// process-spawn-heavy test suite has been observed to close.
///
/// Setting the disposition in `pre_exec` removes the window rather than
/// widening it: `SIG_IGN` is preserved across `execve` (unlike handlers, which
/// reset to default), so the child image starts already ignoring SIGTERM. That
/// is also the more faithful subject for this assertion — the contract is
/// "child ignores the graceful signal", and a child that has been ignoring it
/// since before the deadline is exactly the real-world case.
#[cfg(unix)]
fn cmd_ignoring_sigterm_from_exec() -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = cmd_with_mode("ignore_sigterm");
    // SAFETY: `pre_exec` requires the closure to be async-signal-safe, because
    // it runs in the forked child between `fork` and `execve`. `signal(2)` is
    // on POSIX's async-signal-safe list, as is reading `errno`, and the closure
    // does nothing else.
    //
    // Returning the error rather than dropping it matters for diagnosis, not
    // for control flow: `SIG_ERR` here would mean SIGTERM was somehow
    // uncatchable, and swallowing it would surface as a `TimedOutTerminated`
    // outcome further down — the exact misleading symptom this helper exists to
    // remove. An `Err` from `pre_exec` fails the spawn instead, so the test's
    // `unwrap` panics at the real cause.
    unsafe {
        cmd.as_std_mut().pre_exec(|| {
            if libc::signal(libc::SIGTERM, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

#[tokio::test]
async fn exited_when_child_exits_within_deadline() {
    // `normal` mode would try to write a .wcs sidecar; we want a clean
    // quick exit instead. Use `no_wcs` mode (exits 0 immediately, no
    // side-effects).
    let cmd = cmd_with_mode("no_wcs");
    let outcome = spawn_with_deadline(cmd, Duration::from_secs(5))
        .await
        .unwrap();
    match outcome {
        SpawnOutcome::Exited { status, .. } => {
            assert!(status.success(), "expected zero exit, got {status}");
        }
        other => panic!("expected Exited, got {other:?}"),
    }
}

#[tokio::test]
async fn timed_out_terminated_when_child_responds_to_graceful_signal() {
    let cmd = cmd_with_mode("hang");
    let start = Instant::now();
    let outcome = spawn_with_deadline(cmd, Duration::from_millis(100))
        .await
        .unwrap();
    let elapsed = start.elapsed();
    match outcome {
        SpawnOutcome::TimedOutTerminated => {}
        other => panic!("expected TimedOutTerminated, got {other:?}"),
    }
    // Should have terminated well within the grace period of being
    // signaled — assert the total wall time is bounded.
    assert!(
        elapsed < Duration::from_millis(100) + GRACE_PERIOD,
        "supervision took longer than deadline + grace: {elapsed:?}"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn timed_out_killed_when_child_ignores_graceful_signal() {
    // The Windows mock currently uses SetConsoleCtrlHandler returning TRUE
    // to swallow CTRL_BREAK_EVENT, but tokio's force-kill on Windows
    // (TerminateProcess) bypasses that handler too — semantics are the
    // same. Gating to Unix here avoids spurious flakiness on Windows
    // CI runners with quirky console-attach behavior; the contract
    // assertion holds on both platforms by design.
    let cmd = cmd_ignoring_sigterm_from_exec();
    let start = Instant::now();
    let outcome = spawn_with_deadline(cmd, Duration::from_millis(100))
        .await
        .unwrap();
    let elapsed = start.elapsed();
    match outcome {
        SpawnOutcome::TimedOutKilled => {}
        other => panic!("expected TimedOutKilled, got {other:?}"),
    }
    // Total time = deadline (100ms) + grace (2s) + force-kill latency.
    // Bound generously so this isn't flaky on slow CI.
    assert!(
        elapsed >= Duration::from_millis(100) + GRACE_PERIOD,
        "force-kill should have waited at least the full grace period: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(100) + GRACE_PERIOD + Duration::from_secs(2),
        "supervision took unreasonably long: {elapsed:?}"
    );
}
