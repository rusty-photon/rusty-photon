//! systemd's `rusty-photon-phd2` unit as PHD2's keeper
//! (`docs/services/phd2-guider.md` § "Linux").
//!
//! The unit's state comes from `systemctl show`, run as a bounded
//! subprocess found on `PATH`. A process's age and identity come from
//! `/proc/<pid>/stat` (its start time, field 22, in clock ticks since boot)
//! and `/proc/uptime`. A signal goes through a pidfd opened before the
//! process's identity is checked, so it reaches the process the restart
//! remembered or nothing: never a later process given the same pid.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::Command;
use std::time::Duration;

use async_trait::async_trait;
use rusty_photon_process::{Bounded, Capture, Outcome, OUTPUT_LIMIT, STDERR_TAIL};
use tracing::{debug, warn};

use super::{Keeper, Kept, ProcessId};

/// The unit that runs PHD2.
pub const UNIT: &str = "rusty-photon-phd2";

/// How long `systemctl show` may take. It answers in milliseconds; a
/// systemd that takes longer is not one a restart can rely on.
const SYSTEMCTL_DEADLINE: Duration = Duration::from_secs(10);

/// How long a process has after `SIGTERM` before it is sent `SIGKILL`.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// How often an ending process is checked for its exit.
const EXIT_POLL: Duration = Duration::from_millis(100);

/// The `rusty-photon-phd2` unit, read with `systemctl` and `/proc`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Systemd;

#[async_trait]
impl Keeper for Systemd {
    fn name(&self) -> &'static str {
        UNIT
    }

    async fn state(&self) -> Result<Kept, String> {
        let shown = parse_show(&systemctl_show().await?)?;
        let main = match shown.main_pid {
            Some(pid) => identify(pid)?,
            None => None,
        };
        debug!(state = %shown.active_state, ?main, "read {UNIT}");
        Ok(Kept {
            restarts: restarts(&shown.active_state),
            stopped: stopped(&shown.active_state),
            state: shown.active_state,
            main,
        })
    }

    async fn age(&self, process: ProcessId) -> Result<Option<Duration>, String> {
        age_of(process)
    }

    async fn end(&self, process: ProcessId) -> Result<(), String> {
        end_within(process, TERM_GRACE).await
    }
}

/// End `process`: `SIGTERM`, then `SIGKILL` once `grace` has passed, and
/// return once it no longer runs.
async fn end_within(process: ProcessId, grace: Duration) -> Result<(), String> {
    if signal(process, libc::SIGTERM)? == Signalled::Gone {
        return Ok(());
    }
    debug!(pid = process.pid, "sent PHD2 SIGTERM");
    if wait_gone(process, Some(grace)).await? {
        return Ok(());
    }
    warn!(
        pid = process.pid,
        "PHD2 did not exit within {} of SIGTERM: sending SIGKILL",
        humantime::format_duration(grace)
    );
    if signal(process, libc::SIGKILL)? == Signalled::Gone {
        return Ok(());
    }
    // A process in uninterruptible sleep (a read from a wedged camera) dies
    // only once that read returns; the restart's own bound limits this wait.
    wait_gone(process, None).await?;
    Ok(())
}

/// systemd's `ActiveState`s in which the unit runs PHD2 or is about to
/// start it again: `activating` is also the state of the 2 s between a
/// PHD2 that exited and the next (`Restart=always`).
fn restarts(active_state: &str) -> bool {
    matches!(active_state, "active" | "activating" | "reloading")
}

/// The `ActiveState`s of a unit that starts nothing again.
fn stopped(active_state: &str) -> bool {
    matches!(active_state, "inactive" | "failed")
}

/// What `systemctl show` says about the unit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shown {
    active_state: String,
    /// `MainPID`, `None` for systemd's `0` (no main process).
    main_pid: Option<u32>,
}

/// Parse `systemctl show --property=ActiveState --property=MainPID`'s
/// `Key=value` lines, in whatever order systemd prints them.
fn parse_show(output: &str) -> Result<Shown, String> {
    let mut active_state = None;
    let mut main_pid = None;
    for line in output.lines() {
        match line.trim().split_once('=') {
            Some(("ActiveState", value)) => active_state = Some(value.to_string()),
            Some(("MainPID", value)) => {
                let pid: u32 = value
                    .parse()
                    .map_err(|e| format!("systemctl show: MainPID {value:?}: {e}"))?;
                main_pid = Some(pid);
            }
            _ => {}
        }
    }
    let active_state = active_state
        .filter(|state| !state.is_empty())
        .ok_or_else(|| format!("systemctl show printed no ActiveState: {output:?}"))?;
    let main_pid =
        main_pid.ok_or_else(|| format!("systemctl show printed no MainPID: {output:?}"))?;
    Ok(Shown {
        active_state,
        main_pid: (main_pid != 0).then_some(main_pid),
    })
}

/// Run `systemctl show` for the unit's state and main process.
async fn systemctl_show() -> Result<String, String> {
    let mut command = Command::new("systemctl");
    command.args(["show", "--property=ActiveState", "--property=MainPID", UNIT]);
    let failed = |e: rusty_photon_process::Error| format!("cannot run systemctl show {UNIT}: {e}");
    let outcome = Bounded::new(&mut command, SYSTEMCTL_DEADLINE)
        .grace(Duration::ZERO)
        .stdout(Capture::Full(OUTPUT_LIMIT))
        .stderr(Capture::Tail(STDERR_TAIL))
        .spawn()
        .map_err(failed)?
        .await
        .map_err(failed)?;
    match outcome {
        Outcome::Exited(output) if output.status.success() => String::from_utf8(output.stdout)
            .map_err(|e| format!("systemctl show {UNIT} printed no UTF-8: {e}")),
        Outcome::Exited(output) => Err(format!(
            "systemctl show {UNIT} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Outcome::TimedOut(_) => Err(format!(
            "systemctl show {UNIT} did not answer within {}",
            humantime::format_duration(SYSTEMCTL_DEADLINE)
        )),
    }
}

/// The fields of `/proc/<pid>/stat` a restart reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stat {
    /// Field 3: `R`, `S`, `D`, `Z`, ...
    state: char,
    /// Field 22: when the process started, in clock ticks since boot.
    started: u64,
}

impl Stat {
    /// A zombie (`Z`) or dead (`X`) process runs no more: its exit has only
    /// not been collected yet.
    const fn runs(self) -> bool {
        !matches!(self.state, 'Z' | 'X')
    }
}

/// Parse `/proc/<pid>/stat`. The command name, field 2, sits in
/// parentheses and may hold spaces and parentheses itself, so the fields
/// are counted from the last `)`.
fn parse_stat(text: &str) -> Result<Stat, String> {
    let (_, rest) = text
        .rsplit_once(')')
        .ok_or_else(|| format!("no command name in /proc stat {text:?}"))?;
    let mut fields = rest.split_ascii_whitespace();
    let state = fields
        .next()
        .and_then(|field| field.chars().next())
        .ok_or_else(|| format!("no state in /proc stat {text:?}"))?;
    // Fields 4 to 21 lie between the state and the start time.
    let started = fields
        .nth(18)
        .ok_or_else(|| format!("no start time in /proc stat {text:?}"))?
        .parse()
        .map_err(|e| format!("start time in /proc stat {text:?}: {e}"))?;
    Ok(Stat { state, started })
}

/// Parse `/proc/uptime`: seconds since boot, then idle seconds.
fn parse_uptime(text: &str) -> Result<Duration, String> {
    let seconds: f64 = text
        .split_ascii_whitespace()
        .next()
        .ok_or_else(|| format!("empty /proc/uptime {text:?}"))?
        .parse()
        .map_err(|e| format!("/proc/uptime {text:?}: {e}"))?;
    Duration::try_from_secs_f64(seconds).map_err(|e| format!("/proc/uptime {text:?}: {e}"))
}

/// `ticks` clock ticks at `per_second` ticks a second, to the millisecond.
fn ticks_to_duration(ticks: u64, per_second: u64) -> Option<Duration> {
    ticks
        .checked_mul(1000)
        .and_then(|ms| ms.checked_div(per_second))
        .map(Duration::from_millis)
}

/// `pid`'s stat, `None` when no process has that pid.
fn read_stat(pid: u32) -> Result<Option<Stat>, String> {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => parse_stat(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        // A process that exits while its stat is read.
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(None),
        Err(e) => Err(format!("cannot read /proc/{pid}/stat: {e}")),
    }
}

/// The process running as `pid` now, `None` when none runs.
fn identify(pid: u32) -> Result<Option<ProcessId>, String> {
    Ok(read_stat(pid)?
        .filter(|stat| stat.runs())
        .map(|stat| ProcessId {
            pid,
            started: stat.started,
        }))
}

/// The kernel's clock ticks a second, the unit of a process's start time.
fn clock_ticks_per_second() -> Result<u64, String> {
    // SAFETY: sysconf takes a name and returns a value; it touches no memory
    // of ours.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(ticks)
        .ok()
        .filter(|&ticks| ticks > 0)
        .ok_or_else(|| format!("sysconf(_SC_CLK_TCK) answered {ticks}"))
}

/// How long `process` has run, `None` once it no longer runs.
fn age_of(process: ProcessId) -> Result<Option<Duration>, String> {
    let Some(stat) = read_stat(process.pid)? else {
        return Ok(None);
    };
    if !stat.runs() || stat.started != process.started {
        return Ok(None);
    }
    let uptime = std::fs::read_to_string("/proc/uptime")
        .map_err(|e| format!("cannot read /proc/uptime: {e}"))
        .and_then(|text| parse_uptime(&text))?;
    let started = ticks_to_duration(stat.started, clock_ticks_per_second()?)
        .ok_or_else(|| format!("start time {} ticks is out of range", stat.started))?;
    Ok(Some(uptime.saturating_sub(started)))
}

/// Whether a signal reached its process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signalled {
    Sent,
    /// The process no longer runs; nothing was signalled.
    Gone,
}

/// Send `signal` to `process`, and to no other process.
///
/// The pidfd is opened first, and pins whichever process holds the pid at
/// that moment; the identity check after it then tells whether that is the
/// process remembered. A process that exits in between leaves the pidfd
/// pointing at nothing, and the send reports it gone.
fn signal(process: ProcessId, signal: libc::c_int) -> Result<Signalled, String> {
    let pid = libc::pid_t::try_from(process.pid)
        .map_err(|e| format!("pid {} is out of range: {e}", process.pid))?;
    let no_flags: libc::c_long = 0;
    // SAFETY: pidfd_open takes a pid and flags, and returns a new descriptor
    // or -1; it touches no memory of ours.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::c_long::from(pid), no_flags) };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(Signalled::Gone);
        }
        return Err(format!("pidfd_open({pid}): {error}"));
    }
    let fd = libc::c_int::try_from(fd).map_err(|e| format!("pidfd_open({pid}): {e}"))?;
    // SAFETY: `fd` is the descriptor pidfd_open has just returned, which
    // nothing else owns.
    let pidfd = unsafe { OwnedFd::from_raw_fd(fd) };
    if age_of(process)?.is_none() {
        return Ok(Signalled::Gone);
    }
    // SAFETY: a pidfd we own, a signal number, no siginfo and no flags; the
    // call reads no memory of ours.
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            libc::c_long::from(pidfd.as_raw_fd()),
            libc::c_long::from(signal),
            std::ptr::null::<libc::siginfo_t>(),
            no_flags,
        )
    };
    if sent < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(Signalled::Gone);
        }
        return Err(format!("pidfd_send_signal({pid}, {signal}): {error}"));
    }
    Ok(Signalled::Sent)
}

/// Wait until `process` no longer runs, at most `limit` (`None`: for as
/// long as it takes). Whether it is gone.
async fn wait_gone(process: ProcessId, limit: Option<Duration>) -> Result<bool, String> {
    let deadline = limit.and_then(|limit| tokio::time::Instant::now().checked_add(limit));
    loop {
        if age_of(process)?.is_none() {
            return Ok(true);
        }
        if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            return Ok(false);
        }
        tokio::time::sleep(EXIT_POLL).await;
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn systemctl_show_parses_in_either_order() {
        let shown = parse_show("MainPID=4242\nActiveState=active\n").unwrap();
        assert_eq!(
            shown,
            Shown {
                active_state: "active".to_string(),
                main_pid: Some(4242),
            }
        );
        let shown = parse_show("ActiveState=active\nMainPID=4242\n").unwrap();
        assert_eq!(shown.main_pid, Some(4242));
    }

    #[test]
    fn a_main_pid_of_zero_is_no_process() {
        let shown = parse_show("ActiveState=activating\nMainPID=0\n").unwrap();
        assert_eq!(shown.active_state, "activating");
        assert_eq!(shown.main_pid, None);
    }

    #[test]
    fn systemctl_show_without_a_property_is_an_error() {
        let err = parse_show("ActiveState=active\n").unwrap_err();
        assert!(err.contains("no MainPID"), "{err}");
        let err = parse_show("MainPID=12\n").unwrap_err();
        assert!(err.contains("no ActiveState"), "{err}");
        let err = parse_show("ActiveState=\nMainPID=12\n").unwrap_err();
        assert!(err.contains("no ActiveState"), "{err}");
        let err = parse_show("ActiveState=active\nMainPID=twelve\n").unwrap_err();
        assert!(err.contains("MainPID \"twelve\""), "{err}");
    }

    #[test]
    fn a_unit_running_or_between_two_phd2s_restarts_it() {
        for state in ["active", "activating", "reloading"] {
            assert!(restarts(state), "{state}");
            assert!(!stopped(state), "{state}");
        }
        for state in ["inactive", "failed"] {
            assert!(!restarts(state), "{state}");
            assert!(stopped(state), "{state}");
        }
        // Stopping, or a state this build does not know: neither, so a
        // restart refuses it and a start waits on.
        for state in ["deactivating", "maintenance"] {
            assert!(!restarts(state), "{state}");
            assert!(!stopped(state), "{state}");
        }
    }

    /// A `/proc/<pid>/stat` line whose command name holds spaces and
    /// parentheses, started at tick 98765.
    const STAT: &str = "4242 (phd2 (x) y) S 1 4242 4242 0 -1 4194560 2000 0 0 0 \
                        150 30 0 0 20 0 9 0 98765 1000000 5000 18446744073709551615";

    #[test]
    fn the_stat_line_yields_the_state_and_the_start_time() {
        let stat = parse_stat(STAT).unwrap();
        assert_eq!(
            stat,
            Stat {
                state: 'S',
                started: 98765,
            }
        );
        assert!(stat.runs());
    }

    #[test]
    fn a_zombie_runs_no_more() {
        let zombie = parse_stat(&STAT.replace(") S ", ") Z ")).unwrap();
        assert!(!zombie.runs());
        let dead = parse_stat(&STAT.replace(") S ", ") X ")).unwrap();
        assert!(!dead.runs());
    }

    #[test]
    fn a_short_or_garbled_stat_line_is_an_error() {
        let err = parse_stat("4242 (phd2) S 1 2 3").unwrap_err();
        assert!(err.contains("no start time"), "{err}");
        let err = parse_stat("4242 phd2 S").unwrap_err();
        assert!(err.contains("no command name"), "{err}");
        let err = parse_stat(&STAT.replace(" 98765 ", " soon ")).unwrap_err();
        assert!(err.contains("start time"), "{err}");
    }

    #[test]
    fn the_uptime_is_its_first_field() {
        assert_eq!(
            parse_uptime("12345.67 54321.00\n").unwrap(),
            Duration::from_millis(12_345_670)
        );
        assert!(parse_uptime("").is_err());
        assert!(parse_uptime("-1.0 2.0").is_err());
    }

    #[test]
    fn clock_ticks_become_milliseconds() {
        assert_eq!(
            ticks_to_duration(98765, 100),
            Some(Duration::from_millis(987_650))
        );
        assert_eq!(ticks_to_duration(1, 0), None);
        assert_eq!(ticks_to_duration(u64::MAX, 100), None);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // reads /proc
    fn this_process_is_identified_and_has_run_a_while() {
        let me = identify(std::process::id()).unwrap().unwrap();
        let age = age_of(me).unwrap().unwrap();
        assert!(age < Duration::from_hours(24), "{age:?}");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // reads /proc
    fn a_pid_with_another_start_time_is_not_the_process_remembered() {
        let me = identify(std::process::id()).unwrap().unwrap();
        let reused = ProcessId {
            started: me.started.wrapping_add(1),
            ..me
        };
        assert_eq!(age_of(reused).unwrap(), None);
    }

    /// A child that has exited and been reaped.
    fn reaped_child() -> ProcessId {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        let process = identify(pid)
            .unwrap()
            .unwrap_or(ProcessId { pid, started: 0 });
        child.wait().unwrap();
        process
    }

    #[test]
    #[cfg_attr(miri, ignore)] // spawns a process
    fn a_process_gone_is_neither_aged_nor_signalled() {
        let gone = reaped_child();
        assert_eq!(age_of(gone).unwrap(), None);
        assert_eq!(signal(gone, libc::SIGTERM).unwrap(), Signalled::Gone);
    }

    /// A child that ignores `SIGTERM` (`trap '' TERM`), and the
    /// [`ProcessId`] the keeper knows it by once it has set the trap.
    fn child_ignoring_sigterm() -> (std::process::Child, ProcessId) {
        let mut child = Command::new("sh")
            .args(["-c", "trap '' TERM; echo ready; exec sleep 30"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut ready,
        )
        .unwrap();
        assert_eq!(ready.trim(), "ready");
        let process = identify(child.id()).unwrap().unwrap();
        (child, process)
    }

    #[test]
    #[cfg_attr(miri, ignore)] // spawns a process
    fn sigterm_reaches_the_process_remembered() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let process = identify(child.id()).unwrap().unwrap();
        assert_eq!(signal(process, libc::SIGTERM).unwrap(), Signalled::Sent);
        let status = child.wait().unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(libc::SIGTERM)
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)] // spawns a process
    fn a_pid_now_another_process_is_not_signalled() {
        let (mut child, process) = child_ignoring_sigterm();
        let stranger = ProcessId {
            started: process.started.wrapping_add(1),
            ..process
        };
        assert_eq!(signal(stranger, libc::SIGKILL).unwrap(), Signalled::Gone);
        assert_eq!(child.try_wait().unwrap(), None, "the child was signalled");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// End `child`, known as `process`, within `grace`, reaping it as it
    /// dies (systemd reaps PHD2): how it died.
    async fn end_and_reap(
        mut child: std::process::Child,
        process: ProcessId,
        grace: Duration,
    ) -> Option<i32> {
        let reaper = std::thread::spawn(move || child.wait().unwrap());
        end_within(process, grace).await.unwrap();
        std::os::unix::process::ExitStatusExt::signal(&reaper.join().unwrap())
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // spawns a process
    async fn ending_a_process_that_answers_sigterm_sends_no_sigkill() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let process = identify(child.id()).unwrap().unwrap();
        let died_of = end_and_reap(child, process, Duration::from_secs(30)).await;
        assert_eq!(died_of, Some(libc::SIGTERM));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // spawns a process
    async fn ending_a_process_that_ignores_sigterm_kills_it_after_the_grace() {
        let (child, process) = child_ignoring_sigterm();
        let begun = std::time::Instant::now();
        let grace = Duration::from_millis(300);
        let died_of = end_and_reap(child, process, grace).await;
        assert_eq!(died_of, Some(libc::SIGKILL));
        assert!(begun.elapsed() >= grace, "SIGKILL came before the grace");
    }
}
