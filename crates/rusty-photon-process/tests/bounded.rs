//! The run contract against real children, started through the platform
//! shell (`/bin/sh -c`, `cmd /C`) or as this test binary re-run as a helper.
//!
//! Each test asserts what the run produced — an outcome, the bytes kept, a
//! marker file a surviving process would have written — rather than how long
//! it took. The one timing assertion is a floor (a force-kill waited out the
//! grace), which load can only make longer.

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

use std::future::IntoFuture;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusty_photon_process::{Bounded, Capture, Error, Outcome, Output, Stop, Stream};

/// For runs expected to finish: generous, because only a hang reaches it.
const FINISHES: Duration = Duration::from_secs(30);

/// For runs expected to be stopped: short, so the suite stays quick.
const STOPPED: Duration = Duration::from_millis(300);

/// A short grace keeps the escalation tests quick; the default is 2 s.
const GRACE: Duration = Duration::from_millis(500);

/// Long enough after a stop that a process which survived it would have
/// left its marker.
const SURVIVOR_WOULD_HAVE_WRITTEN: Duration = Duration::from_secs(4);

#[cfg(unix)]
mod script {
    pub const WRITE_HELLO: &str = "printf hello";
    pub const WRITE_BOTH: &str = "printf out; printf err >&2";
    pub const WRITE_THEN_FAIL: &str = "printf partial; exit 3";
    pub const WRITE_LINES: &str = "printf 'one\\ntwo\\r\\nthree'";
    pub const COPY_BULK: &str = "cat bulk";
    pub const COPY_BULK_TO_BOTH: &str = "cat bulk; cat bulk >&2";
    pub const READ_STDIN: &str = "cat";
    pub const NEVER_FINISHES: &str = "sleep 30";
    /// Waits, then leaves a mark: stopped in time, the mark never appears.
    pub const WAIT_THEN_MARK: &str = "sleep 1; : > marker";
    /// A grandchild that leaves the mark, behind a child that outlives the
    /// deadline: only a stop that reaches the grandchild prevents the mark.
    pub const GRANDCHILD_MARKS: &str = "(sleep 2; : > marker) & sleep 30";
    /// Exits at once, leaving a grandchild that holds stdout open.
    pub const EXIT_LEAVING_STDOUT_OPEN: &str = "sleep 30 & printf done";
}

#[cfg(windows)]
mod script {
    pub const WRITE_HELLO: &str = "echo hello";
    pub const WRITE_BOTH: &str = "echo out& echo err 1>&2";
    pub const WRITE_THEN_FAIL: &str = "echo partial& exit 3";
    pub const WRITE_LINES: &str = "echo one& echo two& <nul set /p =three";
    pub const COPY_BULK: &str = "type bulk";
    pub const COPY_BULK_TO_BOTH: &str = "type bulk& type bulk 1>&2";
    pub const READ_STDIN: &str = "sort";
    pub const NEVER_FINISHES: &str = "ping -n 31 127.0.0.1 >nul";
    pub const WAIT_THEN_MARK: &str = "ping -n 3 127.0.0.1 >nul & type nul > marker";
    pub const GRANDCHILD_MARKS: &str =
        "start /b \"\" cmd /C \"ping -n 3 127.0.0.1 >nul & type nul > marker\" & ping -n 31 127.0.0.1 >nul";
    pub const EXIT_LEAVING_STDOUT_OPEN: &str = "start /b \"\" ping -n 31 127.0.0.1 & echo done";
}

#[cfg(unix)]
fn shell(script: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", script]);
    cmd
}

/// `cmd` does not understand the backslash-escaped quotes std would put
/// around an argument, so the line goes to it verbatim.
#[cfg(windows)]
fn shell(script: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("cmd");
    cmd.raw_arg(format!("/C {script}"));
    cmd
}

fn shell_in(script: &str, dir: &Path) -> Command {
    let mut cmd = shell(script);
    cmd.current_dir(dir);
    cmd
}

/// Selects the helper "test" below, and what it does, when this binary is
/// re-run as a child.
const HELPER: &str = "RUSTY_PHOTON_PROCESS_TEST_HELPER";

/// Not a test of anything: a child some tests start by re-running this
/// binary filtered to this function. In a normal run it does nothing.
///
/// - `sleep`: sleep with every signal at its default disposition, which is
///   what the graceful stage needs on Windows — `ping`, `cmd` and PowerShell
///   each handle `CTRL_BREAK_EVENT` themselves.
/// - `bounded-read-stdin`: run a bounded child that reads its input, and exit
///   0 if it finished, 1 if it had to be stopped.
#[test]
fn helper() {
    match std::env::var(HELPER).as_deref() {
        Ok("sleep") => std::thread::sleep(Duration::from_secs(30)),
        Ok("bounded-read-stdin") => {
            let outcome = Bounded::new(&mut shell(script::READ_STDIN), Duration::from_secs(5))
                .grace(Duration::ZERO)
                .run()
                .unwrap();
            std::process::exit(i32::from(matches!(outcome, Outcome::TimedOut(_))));
        }
        _ => {}
    }
}

fn helper_child(mode: &str) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "helper", "--test-threads=1"])
        .env(HELPER, mode);
    cmd
}

fn exited(outcome: Outcome) -> Output {
    match outcome {
        Outcome::Exited(output) => output,
        Outcome::TimedOut(stop) => {
            panic!("expected the child to finish; it was stopped ({stop:?})")
        }
    }
}

fn timed_out(outcome: Outcome) -> Stop {
    match outcome {
        Outcome::TimedOut(stop) => stop,
        Outcome::Exited(output) => {
            panic!("expected the child to be stopped; it finished: {output:?}")
        }
    }
}

/// Trimmed: `cmd`'s `echo` appends a line ending where `printf` does not,
/// and which one ran is not what these tests are about.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

fn write_bulk(dir: &Path) -> usize {
    let mut bulk = vec![b'x'; 200_000];
    bulk.extend_from_slice(b"END");
    std::fs::write(dir.join("bulk"), &bulk).unwrap();
    bulk.len()
}

fn assert_never_marked(dir: &Path) {
    std::thread::sleep(SURVIVOR_WOULD_HAVE_WRITTEN);
    assert!(
        !dir.join("marker").exists(),
        "a process outlived the stop that should have ended it"
    );
}

// ---- finishing ------------------------------------------------------------

#[test]
fn test_a_finished_child_hands_back_what_it_wrote() {
    let output = exited(
        Bounded::new(&mut shell(script::WRITE_HELLO), FINISHES)
            .stdout(Capture::Full(1024))
            .run()
            .unwrap(),
    );
    assert!(output.status.success(), "{}", output.status);
    assert_eq!(text(&output.stdout), "hello");
}

#[test]
fn test_each_stream_is_kept_by_its_own_capture() {
    let output = exited(
        Bounded::new(&mut shell(script::WRITE_BOTH), FINISHES)
            .stdout(Capture::Full(1024))
            .stderr(Capture::Full(1024))
            .run()
            .unwrap(),
    );
    assert_eq!(text(&output.stdout), "out");
    assert_eq!(text(&output.stderr), "err");
}

#[test]
fn test_a_discarded_stream_comes_back_empty() {
    let output = exited(
        Bounded::new(&mut shell(script::WRITE_BOTH), FINISHES)
            .stderr(Capture::Full(1024))
            .run()
            .unwrap(),
    );
    assert_eq!(output.stdout, b"");
    assert_eq!(text(&output.stderr), "err");
}

/// A failing child is an outcome for the caller to judge, not an error — its
/// status and its output both arrive.
#[test]
fn test_a_nonzero_exit_is_an_outcome_with_its_status() {
    let output = exited(
        Bounded::new(&mut shell(script::WRITE_THEN_FAIL), FINISHES)
            .stdout(Capture::Full(1024))
            .run()
            .unwrap(),
    );
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(text(&output.stdout), "partial");
}

/// Why the deadline is on draining and not on exit: a child whose output
/// exceeds the pipe buffer blocks writing until someone reads, so a
/// wait-then-read run would deadlock here.
#[test]
fn test_output_larger_than_a_pipe_buffer_is_drained() {
    let dir = tempfile::tempdir().unwrap();
    let len = write_bulk(dir.path());
    let output = exited(
        Bounded::new(&mut shell_in(script::COPY_BULK, dir.path()), FINISHES)
            .stdout(Capture::Full(1_000_000))
            .run()
            .unwrap(),
    );
    assert_eq!(output.stdout.trim_ascii().len(), len);
}

/// Both pipes are drained at once: a child that fills stderr while only
/// stdout is being read would block before ever closing stdout.
#[test]
fn test_both_streams_are_drained_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let len = write_bulk(dir.path());
    let output = exited(
        Bounded::new(
            &mut shell_in(script::COPY_BULK_TO_BOTH, dir.path()),
            FINISHES,
        )
        .stdout(Capture::Full(1_000_000))
        .stderr(Capture::Tail(3))
        .run()
        .unwrap(),
    );
    assert_eq!(output.stdout.trim_ascii().len(), len);
    assert_eq!(output.stderr, b"END");
}

/// A truncated report must not read as a short one.
#[test]
fn test_output_past_a_full_limit_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    write_bulk(dir.path());
    let error = Bounded::new(&mut shell_in(script::COPY_BULK, dir.path()), FINISHES)
        .stdout(Capture::Full(1000))
        .run()
        .unwrap_err();
    assert!(
        matches!(
            error,
            Error::OutputLimit {
                stream: Stream::Stdout,
                limit: 1000
            }
        ),
        "{error:?}"
    );
}

/// A child that reads its input gets end-of-file, never the caller's own
/// stdin — an interactive `doctor` must not have a child waiting on a
/// keypress. Test runners give a test a null stdin already, so the caller
/// here is a helper whose stdin is a pipe this test holds open: a child that
/// inherited it would block reading until its deadline.
#[test]
fn test_the_child_reads_no_input() {
    let mut caller = helper_child("bounded-read-stdin")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let held_open = caller.stdin.take();
    let status = caller.wait().unwrap();
    drop(held_open);
    assert!(
        status.success(),
        "the bounded child waited on its caller's stdin: {status}"
    );
}

#[test]
fn test_stdout_lines_reach_the_callback_and_the_capture() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let output = exited(
        Bounded::new(&mut shell(script::WRITE_LINES), FINISHES)
            .stdout(Capture::Full(1024))
            .on_stdout_line(move |line| sink.lock().unwrap().push(line.to_string()))
            .run()
            .unwrap(),
    );
    let lines: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|l| l.trim().to_string())
        .collect();
    assert_eq!(lines, ["one", "two", "three"]);
    assert!(
        text(&output.stdout).ends_with("three"),
        "{:?}",
        output.stdout
    );
}

#[test]
fn test_a_child_that_cannot_start_is_a_spawn_error() {
    let error = Bounded::new(
        &mut Command::new("/nonexistent/rusty-photon-process-test"),
        FINISHES,
    )
    .run()
    .unwrap_err();
    assert!(matches!(error, Error::Spawn(_)), "{error:?}");
}

// ---- stopping -------------------------------------------------------------

/// The wedged-child case the crate exists for: an outcome at the deadline,
/// not a hang. The child answers the graceful signal.
#[test]
fn test_a_child_past_its_deadline_is_terminated_gracefully() {
    let stop = timed_out(
        Bounded::new(&mut helper_child("sleep"), STOPPED)
            .grace(Duration::from_secs(10))
            .run()
            .unwrap(),
    );
    assert_eq!(stop, Stop::Terminated);
}

/// A child that ignores the graceful signal is force-killed once the grace
/// is spent, and not before. Unix-only because the fixture is: the signal is
/// ignored from before `exec` (`SIG_IGN` survives it), so no startup window
/// can let the default disposition answer instead.
#[cfg(unix)]
#[test]
fn test_a_child_ignoring_the_signal_is_killed_after_the_grace() {
    use std::os::unix::process::CommandExt;
    use std::time::Instant;
    let mut cmd = helper_child("sleep");
    // SAFETY: the closure runs between fork and exec, so it must be
    // async-signal-safe; signal(2) and reading errno are, and it does nothing
    // else. An error fails the spawn, so the test stops at the real cause
    // rather than at a misleading `Terminated`.
    unsafe {
        cmd.pre_exec(|| {
            if libc::signal(libc::SIGTERM, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let started = Instant::now();
    let stop = timed_out(Bounded::new(&mut cmd, STOPPED).grace(GRACE).run().unwrap());
    let elapsed = started.elapsed();
    assert_eq!(stop, Stop::Killed);
    assert!(
        elapsed >= STOPPED + GRACE,
        "killed after {elapsed:?}, before the grace was out"
    );
}

#[test]
fn test_a_zero_grace_kills_at_the_deadline() {
    let stop = timed_out(
        Bounded::new(&mut shell(script::NEVER_FINISHES), STOPPED)
            .grace(Duration::ZERO)
            .run()
            .unwrap(),
    );
    assert_eq!(stop, Stop::Killed);
}

/// The stop reaches what the child started, not only the child: the
/// grandchild that would leave the mark dies with it.
#[test]
fn test_stopping_a_child_stops_its_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = shell_in(script::GRANDCHILD_MARKS, dir.path());
    timed_out(Bounded::new(&mut cmd, STOPPED).grace(GRACE).run().unwrap());
    assert_never_marked(dir.path());
}

/// The child answering the graceful signal does not end the stop: a
/// descendant that ignored it is force-killed all the same. Unix-only for
/// the fixture: a subshell that ignores `SIGTERM` is one line of `sh`.
#[cfg(unix)]
#[test]
fn test_a_descendant_ignoring_the_signal_is_killed_after_the_child_answers() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = shell_in("(trap '' TERM; sleep 2; : > marker) & sleep 30", dir.path());
    let stop = timed_out(Bounded::new(&mut cmd, STOPPED).grace(GRACE).run().unwrap());
    assert_eq!(stop, Stop::Terminated);
    assert_never_marked(dir.path());
}

/// A child is not finished while a descendant holds its output open, even
/// once the child itself has exited: the run waits for that output, and
/// stops the whole group at the deadline.
#[test]
fn test_output_held_open_by_a_descendant_runs_into_the_deadline() {
    timed_out(
        Bounded::new(&mut shell(script::EXIT_LEAVING_STDOUT_OPEN), STOPPED)
            .stdout(Capture::Full(1024))
            .grace(GRACE)
            .run()
            .unwrap(),
    );
}

#[test]
fn test_dropping_a_running_child_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    let running = Bounded::new(&mut shell_in(script::WAIT_THEN_MARK, dir.path()), FINISHES)
        .spawn()
        .unwrap();
    drop(running);
    assert_never_marked(dir.path());
}

// ---- awaiting -------------------------------------------------------------

#[tokio::test]
async fn test_an_awaited_run_finishes_like_a_blocking_one() {
    let output = exited(
        Bounded::new(&mut shell(script::WRITE_HELLO), FINISHES)
            .stdout(Capture::Full(1024))
            .spawn()
            .unwrap()
            .await
            .unwrap(),
    );
    assert_eq!(text(&output.stdout), "hello");
}

#[tokio::test]
async fn test_an_awaited_run_is_stopped_at_its_deadline() {
    let stop = timed_out(
        Bounded::new(&mut shell(script::NEVER_FINISHES), STOPPED)
            .grace(Duration::ZERO)
            .spawn()
            .unwrap()
            .await
            .unwrap(),
    );
    assert_eq!(stop, Stop::Killed);
}

/// A caller that stops waiting — a dropped request, a `select!` that took
/// another branch — takes the child with it.
#[tokio::test]
async fn test_dropping_the_future_stops_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let finishing = Bounded::new(&mut shell_in(script::WAIT_THEN_MARK, dir.path()), FINISHES)
        .spawn()
        .unwrap()
        .into_future();
    let waited = tokio::time::timeout(Duration::from_millis(200), finishing).await;
    assert!(
        waited.is_err(),
        "the child finished before the future was dropped"
    );
    tokio::task::spawn_blocking({
        let dir = dir.path().to_path_buf();
        move || assert_never_marked(&dir)
    })
    .await
    .unwrap();
}
