//! Subprocess supervision: spawn under a wall-clock deadline; on expiry,
//! escalate from a graceful signal to a force-kill.
//!
//! The graceful signal is `SIGTERM` on Unix and `CTRL_BREAK_EVENT` on
//! Windows. The Windows path requires the child to have been spawned with
//! `CREATE_NEW_PROCESS_GROUP` so the event reaches only the child's group;
//! see `runner/astap.rs::AstapCliRunner::build_command` and the bdd-infra
//! pattern this mirrors (`crates/bdd-infra/src/lib.rs` `send_sigterm`).

use std::process::ExitStatus;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Grace period between graceful signal and force-kill.
///
/// Fixed constant — tuned to dominate signal-handling latency the child
/// might exhibit while staying short enough that a wedged child doesn't
/// tie up the single-flight semaphore.
pub const GRACE_PERIOD: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum SpawnOutcome {
    /// Child exited on its own within the deadline.
    Exited {
        status: ExitStatus,
        stderr_tail: String,
    },
    /// Deadline expired; child responded to the graceful signal within
    /// the grace period.
    TimedOutTerminated,
    /// Deadline expired; child ignored the graceful signal and was
    /// force-killed after the grace period.
    TimedOutKilled,
}

/// Spawn the command and race its exit against a wall-clock deadline.
///
/// On deadline expiry: send graceful signal → wait `GRACE_PERIOD` → force
/// kill. Always `wait()`s for the child fully before returning, so the
/// caller can rely on no orphaned child processes per the design contract.
///
/// # Errors
///
/// Returns an I/O error if the child cannot be spawned, reports no PID,
/// or its exit status cannot be collected on the natural-exit path.
/// Deadline expiry is not an error — it surfaces as the
/// [`SpawnOutcome`] timeout variants — and signal-delivery failures are
/// logged, never returned.
pub async fn spawn_with_deadline(
    mut cmd: Command,
    deadline: Duration,
) -> std::io::Result<SpawnOutcome> {
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| std::io::Error::other("spawned child has no PID"))?;

    // Drain stderr concurrently in a background task. If we instead read
    // it after `wait()`, a child writing >64 KiB to stderr would fill the
    // OS pipe buffer and block itself before exiting — `wait()` would
    // never return, and the deadline race could not save us. The drain
    // task captures up to STDERR_TAIL_BYTES into a buffer and discards
    // the rest (so the pipe stays drained without unbounded memory).
    let stderr_task = child.stderr.take().map(spawn_stderr_drain);

    let outcome = tokio::select! {
        biased;
        result = child.wait() => {
            let status = result?;
            SpawnOutcome::Exited { status, stderr_tail: String::new() }
        }
        () = tokio::time::sleep(deadline) => {
            // Deadline. Send graceful signal, wait grace period, escalate.
            send_graceful(pid);
            if let Ok(_status) = tokio::time::timeout(GRACE_PERIOD, child.wait()).await { SpawnOutcome::TimedOutTerminated } else {
                // Force-kill. tokio's Child::kill sends SIGKILL on Unix
                // and TerminateProcess on Windows.
                let _ = child.start_kill();
                let _ = child.wait().await;
                SpawnOutcome::TimedOutKilled
            }
        }
    };

    // Collect the drained stderr tail. Only carried in the Exited variant
    // because the timeout variants do not include stderr in their HTTP
    // response per the contract.
    let stderr_tail = match stderr_task {
        Some(t) => t.await.unwrap_or_default(),
        None => String::new(),
    };
    Ok(match outcome {
        SpawnOutcome::Exited { status, .. } => SpawnOutcome::Exited {
            status,
            stderr_tail,
        },
        other => other,
    })
}

/// Send the platform's graceful-shutdown signal to a process. Best-effort:
/// signal failures log via `tracing::debug!` and do not propagate, so a
/// caller's deadline path is not derailed by signal-delivery transients.
fn send_graceful(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: libc::kill with a valid pid and SIGTERM is safe. This is
        // the same pattern bdd-infra uses; see send_sigterm there.
        let ret = unsafe { libc::kill(pid.cast_signed(), libc::SIGTERM) };
        if ret != 0 {
            tracing::debug!(
                "supervision: failed to send SIGTERM to pid {pid}: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    #[cfg(windows)]
    {
        // SAFETY: GenerateConsoleCtrlEvent with CTRL_BREAK_EVENT and a
        // valid process-group id is the documented graceful-shutdown
        // signal for a console process on Windows. The child must have
        // been spawned with CREATE_NEW_PROCESS_GROUP for this to target
        // only its group.
        #[allow(non_snake_case)]
        extern "system" {
            fn GenerateConsoleCtrlEvent(dw_ctrl_event: u32, dw_process_group_id: u32) -> i32;
        }
        const CTRL_BREAK_EVENT: u32 = 1;
        let ret = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
        if ret == 0 {
            tracing::debug!(
                "supervision: failed to send CTRL_BREAK_EVENT to process group {pid}: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Maximum bytes of stderr the drain task retains for the response.
/// The drain task keeps reading past this limit (to keep the OS pipe
/// drained), but only the **last** `STDERR_TAIL_BYTES` bytes are kept.
const STDERR_TAIL_BYTES: usize = 4096;

/// Spawn a background task that drains `stderr` and returns its **tail**
/// — the last `STDERR_TAIL_BYTES` bytes of output, where ASTAP's actual
/// error context lives. Returns a join handle resolving to the tail as
/// a `String` (lossy-decoded once, on a bounded byte slice, so no
/// UTF-8 boundary risk).
///
/// Two failure modes this avoids:
///
/// 1. **Pipe-fill deadlock** — a child writing >64 KiB to stderr would
///    fill the OS pipe buffer and block itself before exiting,
///    preventing `wait()` from returning. The drain task is always
///    active concurrently, so the pipe is kept clear regardless of
///    output volume.
/// 2. **Unbounded memory** — `read_to_end` would buffer the entire
///    stream before any truncation. This task keeps a sliding window
///    of at most `2 * STDERR_TAIL_BYTES` bytes and amortizes the
///    drain cost via a periodic shift.
fn spawn_stderr_drain(mut stderr: tokio::process::ChildStderr) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        // Sliding window: append new bytes to the end; when the buffer
        // exceeds 2 * STDERR_TAIL_BYTES, drain the leading half so the
        // tail (last STDERR_TAIL_BYTES) is preserved. Vec::drain's shift
        // cost is amortized by the doubling threshold.
        let mut buf: Vec<u8> = Vec::with_capacity(STDERR_TAIL_BYTES * 2);
        let mut chunk = [0u8; 1024];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // `read` returns at most the buffer's length; a
                    // violation degrades like a read error.
                    let Some(read) = chunk.get(..n) else { break };
                    buf.extend_from_slice(read);
                    if buf.len() > STDERR_TAIL_BYTES * 2 {
                        let drop = buf.len().saturating_sub(STDERR_TAIL_BYTES);
                        buf.drain(..drop);
                    }
                }
            }
        }
        // Final trim to the last STDERR_TAIL_BYTES bytes.
        if buf.len() > STDERR_TAIL_BYTES {
            let drop = buf.len().saturating_sub(STDERR_TAIL_BYTES);
            buf.drain(..drop);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn grace_period_is_two_seconds() {
        // The constant is part of the public supervision contract — the
        // design doc and plan both name 2s. This test exists so the
        // constant doesn't drift silently.
        assert_eq!(GRACE_PERIOD, Duration::from_secs(2));
    }

    #[tokio::test]
    async fn outcome_variants_are_constructible() {
        // Smoke test that the SpawnOutcome variants compile and can be
        // matched without spawning a real subprocess (those tests live in
        // tests/supervision_integration.rs; see plan §Phase 2).
        let exited = SpawnOutcome::Exited {
            status: std::process::Command::new("true")
                .status()
                .unwrap_or_else(|_| {
                    std::process::Command::new("cmd")
                        .args(["/C", "exit", "0"])
                        .status()
                        .expect("a no-op exit-0 command must work")
                }),
            stderr_tail: String::new(),
        };
        match exited {
            SpawnOutcome::Exited { .. } => {}
            _ => panic!("wrong variant"),
        }

        let term = SpawnOutcome::TimedOutTerminated;
        let kill = SpawnOutcome::TimedOutKilled;
        match (term, kill) {
            (SpawnOutcome::TimedOutTerminated, SpawnOutcome::TimedOutKilled) => {}
            _ => panic!("wrong variants"),
        }
    }
}
