//! The child's process tree: started at the root of a tree of its own, so
//! that stopping it reaches everything it started.
//!
//! Unix makes the child the leader of a new session — and so of a new
//! process group — and signals the group. Windows gives it a console process
//! group of its own — what `CTRL_BREAK_EVENT` addresses — and a job object,
//! which is what a force-kill terminates.

#[cfg(unix)]
pub use unix::{has_exited, prepare, reap_later, Tree};
#[cfg(windows)]
pub use windows::{has_exited, prepare, reap_later, Tree};

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};
    use std::thread;

    use tracing::debug;

    /// Start the child as the leader of a new session, which makes it the
    /// leader of a new process group whose id is its own pid.
    ///
    /// A new session rather than only a new process group: a session has no
    /// controlling terminal, so a child that wants one — `ssh` asking for a
    /// passphrase, `sudo` for a password — fails at once with an error to
    /// report. In a background group of the caller's session it would be
    /// stopped by `SIGTTIN` instead, and sit silent until its deadline.
    pub fn prepare(command: &mut Command) {
        // SAFETY: the closure runs in the forked child before `exec`, where
        // only async-signal-safe calls are allowed: setsid(2) is one, and
        // reading errno is too. `EPERM` means the child already leads a
        // group — a second `prepare` of the same command — which is what this
        // asks for.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::EPERM) {
                        return Err(err);
                    }
                }
                Ok(())
            });
        }
    }

    /// Reap a killed child that has not died yet, on a thread of its own,
    /// whenever it does. Until then it holds its pid, and so its group id,
    /// which is what the kill was sent to: no other group can be given it.
    pub fn reap_later(child: &Child) {
        let pid = child.id().cast_signed();
        let spawned = thread::Builder::new()
            .name("bounded-reap".to_string())
            .spawn(move || loop {
                // SAFETY: waitpid(2) with a null status pointer writes
                // nothing. The pid is this process's own unreaped child: its
                // `Child` is never waited on again.
                let ret = unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
                if ret != -1 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            });
        if let Err(e) = spawned {
            debug!(pid, "could not start a thread to reap the child later: {e}");
        }
    }

    /// The child's process group.
    #[derive(Debug)]
    pub struct Tree {
        group: libc::pid_t,
    }

    impl Tree {
        pub fn attach(child: &Child) -> Self {
            Self {
                group: child.id().cast_signed(),
            }
        }

        /// Ask the whole group to stop. Whether the signal was delivered.
        pub fn signal_graceful(&self) -> bool {
            self.signal(libc::SIGTERM)
        }

        /// Kill the whole group, falling back to the child alone when the
        /// group cannot be signalled.
        pub fn kill(&self, child: &mut Child) {
            if !self.signal(libc::SIGKILL) {
                drop(child.kill());
            }
        }

        fn signal(&self, signal: libc::c_int) -> bool {
            let Some(target) = self.group.checked_neg() else {
                return false;
            };
            // SAFETY: kill(2) takes no pointers. A negative pid addresses the
            // process group with that absolute id, which is the child's own:
            // the group exists for as long as the child is unreaped, and every
            // caller signals before reaping.
            let ret = unsafe { libc::kill(target, signal) };
            if ret == 0 {
                return true;
            }
            // errno is read here, not inside the macro: a log field runs only
            // once the callsite is known to be enabled, which puts the
            // subscriber's own work between the failing call and the read.
            let err = io::Error::last_os_error();
            debug!(
                group = self.group,
                signal, "could not signal the process group: {err}"
            );
            false
        }
    }

    /// Whether the child has exited, **without reaping it**.
    ///
    /// A reaped leader frees its pid, and with it the group id; a group
    /// signal sent after that could reach an unrelated group that happened to
    /// be given the same id. `WNOWAIT` leaves the child waitable, so the
    /// group id stays reserved until [`Child::wait`] reaps it.
    pub fn has_exited(child: &mut Child) -> io::Result<bool> {
        // SAFETY: `siginfo_t` is a plain C struct, for which all-zero is a
        // valid value.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid, writable `siginfo_t` that outlives the
        // call. WNOHANG returns at once, and WNOWAIT leaves the child's state
        // for std's own wait to collect.
        let ret = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &raw mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        // POSIX: under WNOHANG with nothing to report, `si_signo` is zero.
        Ok(info.si_signo != 0)
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};
    use std::ptr;

    use tracing::debug;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CTRL_BREAK_EVENT: u32 = 1;
    /// The exit code a force-killed tree reports — the one `Child::kill`
    /// gives a single process.
    const KILLED_EXIT_CODE: u32 = 1;

    type Handle = *mut c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn GenerateConsoleCtrlEvent(ctrl_event: u32, process_group_id: u32) -> i32;
    }

    /// Start the child in a console process group of its own, whose id is
    /// then the child's pid: `CTRL_BREAK_EVENT` is sent to a group, and this
    /// one holds the child and what it starts, never the caller.
    pub fn prepare(command: &mut Command) {
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    /// The child's console process group and, when one could be set up, the
    /// job object holding the child and everything it starts.
    #[derive(Debug)]
    pub struct Tree {
        group: u32,
        job: Option<Job>,
    }

    impl Tree {
        pub fn attach(child: &Child) -> Self {
            Self {
                group: child.id(),
                job: Job::holding(child),
            }
        }

        /// Ask the whole group to stop. Whether the event was delivered —
        /// it is not, for one, from a process with no console, such as a
        /// Windows service.
        pub fn signal_graceful(&self) -> bool {
            // SAFETY: GenerateConsoleCtrlEvent takes plain values. The group
            // is the child's own (`prepare`), so the event cannot reach the
            // caller's group.
            let ret = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.group) };
            if ret != 0 {
                return true;
            }
            // Read before the macro; see the Unix arm.
            let err = io::Error::last_os_error();
            debug!(group = self.group, "could not send CTRL_BREAK_EVENT: {err}");
            false
        }

        /// Terminate the job, falling back to the child alone when there is
        /// no job or it cannot be terminated.
        pub fn kill(&self, child: &mut Child) {
            if !self.job.as_ref().is_some_and(Job::terminate) {
                drop(child.kill());
            }
        }
    }

    /// An open job-object handle, closed on drop. Closing it does not
    /// terminate the job: it is created without
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so a descendant that outlives a
    /// run that finished normally is left alone, as on Unix.
    #[derive(Debug)]
    struct Job(Handle);

    // SAFETY: a job-object handle is a process-wide kernel handle; Win32 lets
    // any thread use or close it, and `Job` is its only owner.
    unsafe impl Send for Job {}

    impl Job {
        /// A new job holding `child`, or `None` (logged) when one cannot be
        /// created or the child cannot be placed in it — for instance under
        /// an enclosing job that forbids it.
        fn holding(child: &Child) -> Option<Self> {
            // SAFETY: null attributes and a null name ask for an unnamed job
            // with default security.
            let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
            if handle.is_null() {
                let err = io::Error::last_os_error();
                debug!("could not create a job object for the child: {err}");
                return None;
            }
            let job = Self(handle);
            // SAFETY: both handles are open — the job's is owned by `job`,
            // and the process's by `child`, which outlives the call.
            let ret = unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle()) };
            if ret == 0 {
                let err = io::Error::last_os_error();
                debug!(
                    pid = child.id(),
                    "could not place the child in a job object: {err}"
                );
                return None;
            }
            Some(job)
        }

        /// Terminate every process in the job. Whether that succeeded.
        fn terminate(&self) -> bool {
            // SAFETY: the handle is open for as long as `self` lives.
            let ret = unsafe { TerminateJobObject(self.0, KILLED_EXIT_CODE) };
            if ret != 0 {
                return true;
            }
            let err = io::Error::last_os_error();
            debug!("could not terminate the child's job object: {err}");
            false
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle is open, and owned only by `self`.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Whether the child has exited. A Windows process handle keeps the
    /// process's identity alive however its status is read, so there is no
    /// reaping to avoid.
    pub fn has_exited(child: &mut Child) -> io::Result<bool> {
        child.try_wait().map(|status| status.is_some())
    }

    /// Nothing to do: Windows has no zombies. Closing the child's handle when
    /// its `Child` drops is all the cleanup an exited process needs, and the
    /// kill already sent ends it once the I/O holding it returns.
    pub const fn reap_later(_child: &Child) {}
}
