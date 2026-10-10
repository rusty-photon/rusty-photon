#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
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
        clippy::struct_excessive_bools,
    )
)]
//! `rusty-photon-process` — running a command to completion under a deadline.
//!
//! [`Bounded`] starts a child, drains its output while it runs, and, if it
//! has not finished when the deadline comes, asks it to stop and then makes
//! it — reaching everything the child started, not only the child. A wedged
//! child costs its caller a bounded wait, never a hang.
//!
//! ```no_run
//! use std::process::Command;
//! use std::time::Duration;
//! use rusty_photon_process::{Bounded, Capture, Outcome, OUTPUT_LIMIT};
//!
//! let mut cmd = Command::new("systemctl");
//! cmd.args(["list-unit-files", "rusty-photon-*"]);
//! match Bounded::new(&mut cmd, Duration::from_secs(30))
//!     .stdout(Capture::Full(OUTPUT_LIMIT))
//!     .run()
//! {
//!     Ok(Outcome::Exited(output)) if output.status.success() => { /* parse output.stdout */ }
//!     Ok(Outcome::Exited(output)) => { /* the child failed: output.status */ }
//!     Ok(Outcome::TimedOut(_stop)) => { /* stopped at the deadline */ }
//!     Err(_error) => { /* could not be run */ }
//! }
//! ```
//!
//! The same run is awaitable — `Bounded::spawn` returns a [`Running`], which
//! is [`IntoFuture`](std::future::IntoFuture) — without an async runtime, so
//! synchronous crates use it as readily as `tokio` services do. See
//! `docs/crates/rusty-photon-process.md` for the contract.

mod drain;
mod future;
mod tree;

use std::fmt;
use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use tracing::debug;

pub use future::Finishing;

use drain::LineSink;
use tree::Tree;

/// How long a child is given to exit after the graceful signal before it is
/// force-killed, unless a run sets its own with [`Bounded::grace`].
///
/// Long enough to dominate a well-behaved child's signal handling, short
/// enough that a wedged one does not hold up whatever waits on it.
pub const GRACE_PERIOD: Duration = Duration::from_secs(2);

/// A ceiling for output a caller parses (16 MiB): far above any report a
/// well-behaved tool writes, far below anything that would strain a small
/// host.
pub const OUTPUT_LIMIT: usize = 16_777_216;

/// Enough of a stream's end (4 KiB) for the error context a diagnosis needs.
pub const STDERR_TAIL: usize = 4096;

/// The first and the longest interval between two looks at whether the
/// child has exited, once there is no output left to wait on. Doubling from
/// the first keeps a child that exits promptly from waiting out the longest.
const POLL_FIRST: Duration = Duration::from_millis(1);
const POLL_LONGEST: Duration = Duration::from_millis(50);

/// What a run keeps of one of the child's output streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Capture {
    /// Nothing: the stream is not connected — unless a line callback needs
    /// stdout, in which case it is read and dropped.
    #[default]
    Discard,
    /// Everything, up to this many bytes. A child that writes more is
    /// force-stopped and the run fails with [`Error::OutputLimit`]: output a
    /// caller parses must not arrive truncated and read as complete.
    Full(usize),
    /// The last this-many bytes; everything before them is read and dropped.
    Tail(usize),
}

/// One of the child's output streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl fmt::Display for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        })
    }
}

/// How a run that could be carried out ended.
#[derive(Debug)]
pub enum Outcome {
    /// The child exited and closed its output before the deadline. A
    /// non-zero exit is the caller's to judge.
    Exited(Output),
    /// The deadline came first. The child has been stopped and reaped; its
    /// output is not returned, being incomplete by definition.
    TimedOut(Stop),
}

/// What a child that finished left behind.
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    /// What [`Bounded::stdout`]'s capture kept.
    pub stdout: Vec<u8>,
    /// What [`Bounded::stderr`]'s capture kept.
    pub stderr: Vec<u8>,
}

/// How far a stop at the deadline had to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The child exited within the grace period after the graceful signal.
    Terminated,
    /// The child had to be force-killed: it ignored the signal, the signal
    /// could not be delivered, or the run's grace was zero.
    Killed,
}

/// Why a run could not be carried out. Every error after the spawn has
/// force-stopped the child's tree and reaped the child before it is returned.
///
/// The messages call the child "the child": its command line can carry text
/// a log should not repeat, so the caller adds the context it knows is safe.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not start the child: {0}")]
    Spawn(#[source] io::Error),
    #[error("could not start a thread to watch the child: {0}")]
    Thread(#[source] io::Error),
    #[error("could not read the child's {stream}: {source}")]
    Read {
        stream: Stream,
        #[source]
        source: io::Error,
    },
    #[error("the child wrote more than {limit} bytes to its {stream}")]
    OutputLimit { stream: Stream, limit: usize },
    #[error("could not collect the child's exit status: {0}")]
    Wait(#[source] io::Error),
}

/// A command to run to completion under a deadline.
///
/// The caller builds the command — program, arguments, environment, working
/// directory. `Bounded` owns the rest and overwrites whatever the caller set
/// there: stdin is always null, stdout and stderr are piped or null as their
/// [`Capture`] says, and the child starts at the root of a process group of
/// its own (on Windows, `CREATE_NEW_PROCESS_GROUP` replaces any creation
/// flags).
#[must_use = "a Bounded does nothing until it is run or spawned"]
pub struct Bounded<'a> {
    command: &'a mut Command,
    deadline: Duration,
    grace: Duration,
    stdout: Capture,
    stderr: Capture,
    on_stdout_line: Option<LineSink>,
}

impl fmt::Debug for Bounded<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bounded")
            .field("command", &self.command)
            .field("deadline", &self.deadline)
            .field("grace", &self.grace)
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .field("on_stdout_line", &self.on_stdout_line.is_some())
            .finish()
    }
}

impl<'a> Bounded<'a> {
    /// Run `command` under `deadline`, measured from the spawn. Both streams
    /// are discarded and the grace period is [`GRACE_PERIOD`] until set
    /// otherwise.
    pub fn new(command: &'a mut Command, deadline: Duration) -> Self {
        Self {
            command,
            deadline,
            grace: GRACE_PERIOD,
            stdout: Capture::Discard,
            stderr: Capture::Discard,
            on_stdout_line: None,
        }
    }

    /// How long the child gets to exit after the graceful signal. Zero skips
    /// the signal and force-kills at the deadline.
    pub const fn grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// What to keep of stdout.
    pub const fn stdout(mut self, capture: Capture) -> Self {
        self.stdout = capture;
        self
    }

    /// What to keep of stderr.
    pub const fn stderr(mut self, capture: Capture) -> Self {
        self.stderr = capture;
        self
    }

    /// Hand each line of stdout to `sink` as it arrives, on top of whatever
    /// [`Bounded::stdout`] keeps. Lines are split on `\n` with a trailing
    /// `\r` dropped and invalid UTF-8 replaced; an unterminated last line
    /// arrives at end-of-file, and a line over 64 KiB arrives in pieces.
    /// `sink` runs on the thread reading stdout.
    pub fn on_stdout_line(mut self, sink: impl FnMut(&str) + Send + 'static) -> Self {
        self.on_stdout_line = Some(Box::new(sink));
        self
    }

    /// Start the child; the deadline runs from here. Wait for it with
    /// [`Running::wait`], or `.await` it.
    ///
    /// # Errors
    ///
    /// [`Error::Spawn`] if the child cannot be started, [`Error::Thread`] if
    /// a thread to read its output cannot be — in which case the child is
    /// stopped again.
    pub fn spawn(self) -> Result<Running, Error> {
        let Self {
            command,
            deadline,
            grace,
            stdout,
            stderr,
            on_stdout_line,
        } = self;
        let stdout_piped = stdout != Capture::Discard || on_stdout_line.is_some();
        let stderr_piped = stderr != Capture::Discard;
        command
            .stdin(Stdio::null())
            .stdout(piped_or_null(stdout_piped))
            .stderr(piped_or_null(stderr_piped));
        tree::prepare(command);

        let mut child = command.spawn().map_err(Error::Spawn)?;
        let started = Instant::now();
        let tree = Tree::attach(&child);
        debug!(
            pid = child.id(),
            ?deadline,
            ?grace,
            "started a bounded child"
        );

        let (events, inbox) = mpsc::channel();
        let child_stdout = child.stdout.take();
        let child_stderr = child.stderr.take();
        // From here on the guard stops and reaps the child on any early
        // return, a reader that could not be started included.
        let guard = Guard {
            child,
            tree,
            reaped: false,
        };
        let stdout = match child_stdout {
            Some(source) => {
                read_on_a_thread(source, Stream::Stdout, stdout, on_stdout_line, &events)?;
                Drained::Waiting
            }
            None => Drained::Done(Vec::new()),
        };
        let stderr = match child_stderr {
            Some(source) => {
                read_on_a_thread(source, Stream::Stderr, stderr, None, &events)?;
                Drained::Waiting
            }
            None => Drained::Done(Vec::new()),
        };

        Ok(Running {
            guard,
            started,
            deadline,
            grace,
            inbox,
            events,
            stdout,
            stderr,
        })
    }

    /// Run the child to completion, blocking the calling thread.
    ///
    /// # Errors
    ///
    /// See [`Bounded::spawn`] and [`Running::wait`].
    pub fn run(self) -> Result<Outcome, Error> {
        self.spawn()?.wait()
    }
}

fn piped_or_null(piped: bool) -> Stdio {
    if piped {
        Stdio::piped()
    } else {
        Stdio::null()
    }
}

fn read_on_a_thread(
    source: impl Read + Send + 'static,
    stream: Stream,
    capture: Capture,
    sink: Option<LineSink>,
    events: &Sender<Event>,
) -> Result<(), Error> {
    let events = events.clone();
    thread::Builder::new()
        .name(format!("bounded-{stream}"))
        .spawn(move || {
            let drained = drain::drain(source, stream, capture, sink);
            // A closed channel means the run already ended without this
            // stream; nobody wants it any more.
            drop(events.send(Event::Drained(stream, drained)));
        })
        .map(drop)
        .map_err(Error::Thread)
}

/// What the run's wait is told while it waits.
#[derive(Debug)]
pub(crate) enum Event {
    /// A stream reached its end, or failed.
    Drained(Stream, Result<Vec<u8>, Error>),
    /// The future awaiting the run was dropped.
    Abandoned,
}

#[derive(Debug)]
enum Drained {
    Waiting,
    Done(Vec<u8>),
}

impl Drained {
    const fn is_waiting(&self) -> bool {
        matches!(self, Self::Waiting)
    }

    fn take(&mut self) -> Vec<u8> {
        match std::mem::replace(self, Self::Done(Vec::new())) {
            Self::Waiting => Vec::new(),
            Self::Done(bytes) => bytes,
        }
    }
}

/// A child running under its deadline. Wait for it with [`Running::wait`],
/// or `.await` it. Dropped unwaited, it force-stops the child's tree and
/// reaps the child.
#[derive(Debug)]
#[must_use = "a running child is stopped when its Running is dropped"]
pub struct Running {
    guard: Guard,
    started: Instant,
    deadline: Duration,
    grace: Duration,
    inbox: Receiver<Event>,
    /// Kept so the channel stays open whatever the readers do, and cloned
    /// for the future that may abandon the run.
    events: Sender<Event>,
    stdout: Drained,
    stderr: Drained,
}

impl Running {
    /// Block until the child finishes, or until the deadline has passed and
    /// the child has been stopped.
    ///
    /// # Errors
    ///
    /// [`Error::Read`] or [`Error::OutputLimit`] if a stream could not be
    /// read to its end within its capture, [`Error::Wait`] if the exit
    /// status could not be collected. The child is stopped before any of
    /// them is returned.
    pub fn wait(mut self) -> Result<Outcome, Error> {
        let mut poll = POLL_FIRST;
        loop {
            let Some(remaining) = self
                .deadline
                .checked_sub(self.started.elapsed())
                .filter(|remaining| !remaining.is_zero())
            else {
                debug!(pid = self.guard.child.id(), "the child missed its deadline");
                return Ok(Outcome::TimedOut(self.guard.stop(self.grace)));
            };

            // Output first: a child is not finished while a stream is open,
            // and its exit is not worth polling for until then.
            if self.stdout.is_waiting() || self.stderr.is_waiting() {
                if let Some(ended) = self.receive(remaining) {
                    return ended;
                }
                continue;
            }

            match self.guard.child.try_wait() {
                Ok(Some(status)) => {
                    self.guard.reaped = true;
                    debug!(pid = self.guard.child.id(), %status, "the bounded child finished");
                    return Ok(Outcome::Exited(Output {
                        status,
                        stdout: self.stdout.take(),
                        stderr: self.stderr.take(),
                    }));
                }
                Ok(None) => {}
                Err(e) => {
                    self.guard.kill();
                    return Err(Error::Wait(e));
                }
            }
            // Waiting on the channel rather than sleeping, so an abandoned
            // run is stopped at once.
            if let Some(ended) = self.receive(poll.min(remaining)) {
                return ended;
            }
            poll = poll.saturating_mul(2).min(POLL_LONGEST);
        }
    }

    /// Wait up to `timeout` for the next event. `Some` when it ends the run.
    fn receive(&mut self, timeout: Duration) -> Option<Result<Outcome, Error>> {
        let event = match self.inbox.recv_timeout(timeout) {
            Ok(event) => event,
            // `self.events` keeps the channel open, so only the timeout is
            // reachable; either way there is nothing to act on.
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
        };
        match event {
            Event::Drained(Stream::Stdout, Ok(bytes)) => {
                self.stdout = Drained::Done(bytes);
                None
            }
            Event::Drained(Stream::Stderr, Ok(bytes)) => {
                self.stderr = Drained::Done(bytes);
                None
            }
            Event::Drained(_, Err(error)) => {
                debug!(pid = self.guard.child.id(), "stopping the child: {error}");
                self.guard.kill();
                Some(Err(error))
            }
            Event::Abandoned => {
                debug!(pid = self.guard.child.id(), "stopping an abandoned child");
                self.guard.kill();
                // Nobody is left to read this.
                Some(Ok(Outcome::TimedOut(Stop::Killed)))
            }
        }
    }
}

/// Owns the child until it is reaped, and stops its tree if it is dropped
/// before then.
#[derive(Debug)]
struct Guard {
    child: Child,
    tree: Tree,
    reaped: bool,
}

impl Guard {
    /// The deadline's stop: the graceful signal, the grace period, then a
    /// force-kill of whatever is left of the tree — the child too, if it did
    /// not answer — and the reap.
    fn stop(&mut self, grace: Duration) -> Stop {
        let answered = !grace.is_zero() && self.tree.signal_graceful() && self.exits_within(grace);
        // Even after the child answered: a descendant that ignored the
        // signal must not outlive the run.
        self.tree.kill(&mut self.child);
        self.reap();
        let stop = if answered {
            Stop::Terminated
        } else {
            Stop::Killed
        };
        debug!(pid = self.child.id(), ?stop, "stopped the bounded child");
        stop
    }

    /// Force-kill the tree and reap the child.
    fn kill(&mut self) {
        self.tree.kill(&mut self.child);
        self.reap();
    }

    /// Whether the child exits within `grace`, looked at without reaping it.
    fn exits_within(&mut self, grace: Duration) -> bool {
        let since = Instant::now();
        let mut poll = POLL_FIRST;
        loop {
            match tree::has_exited(&mut self.child) {
                Ok(true) => return true,
                Ok(false) => {}
                Err(e) => {
                    debug!(
                        pid = self.child.id(),
                        "could not check whether the child exited: {e}"
                    );
                    return false;
                }
            }
            let Some(left) = grace
                .checked_sub(since.elapsed())
                .filter(|left| !left.is_zero())
            else {
                return false;
            };
            thread::sleep(poll.min(left));
            poll = poll.saturating_mul(2).min(POLL_LONGEST);
        }
    }

    fn reap(&mut self) {
        if let Err(e) = self.child.wait() {
            debug!(pid = self.child.id(), "could not reap the child: {e}");
        }
        self.reaped = true;
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.reaped {
            debug!(
                pid = self.child.id(),
                "stopping a child dropped before it finished"
            );
            self.kill();
        }
    }
}
