# `rusty-photon-process` Crate Design

Running a command to completion under a deadline. One shape: start a child,
drain its output while it runs, and if it has not finished when the deadline
comes, ask it to stop and then make it — so a wedged child costs the caller a
bounded wait and never a hang.

This is a workspace library, not a service. It depends on `libc` (Unix),
`thiserror` and `tracing`, and on no async runtime: it serves synchronous
callers (`rusty-photon-doctor-checks`, which every service links, and the
central doctor's platform inspectors) and asynchronous ones (`plate-solver`,
`sentinel`, doctor's aggregation and the `bdd-infra` ConformU runner) through
one implementation.

## Scope

- Run one command to completion under a wall-clock deadline.
- Capture each output stream whole (up to a limit), as its head or its tail,
  not at all, or pass it through to the caller's own, and optionally hand
  stdout to a callback line by line as it arrives.
- On the deadline, stop the child and everything it started: a graceful
  signal, a grace period, then a force-kill.
- Leave nothing running behind a run that was stopped, and leave no child
  unreaped behind any run.
- Build the platform shell invocation of a command line (`shell`), so the
  callers that run an operator's line — renewal hooks, restart commands — do
  not each carry their own copy of `cmd`'s quoting rules.

Out of scope: long-lived supervised children — `phd2-guider` holding the PHD2
process, `bdd-infra`'s `ServiceHandle`, OmniSim. Those need lifecycle state,
health and restart, which is a different problem; folding them in would give
the crate two unrelated jobs. Commands whose answer is immediate and whose
failure is harmless (`id -G` in `rusty-photon-config`, `which phd2`) do not
need it either.

## The contract

```rust
use rusty_photon_process::{Bounded, Capture, Outcome, OUTPUT_LIMIT, STDERR_TAIL};

let mut cmd = std::process::Command::new("systemctl");
cmd.args(["list-unit-files", "rusty-photon-*"]);

// Blocking:
let outcome = Bounded::new(&mut cmd, Duration::from_secs(30))
    .stdout(Capture::Full(OUTPUT_LIMIT))
    .stderr(Capture::Tail(STDERR_TAIL))
    .run()?;

// Async — the same run, awaited:
let outcome = Bounded::new(&mut cmd, deadline).spawn()?.await?;
```

`Bounded` borrows the `Command` only to spawn it; the running child owns
nothing of the caller's. Building the command — its program, arguments,
environment and working directory — stays with the caller. **The crate owns
the rest**: the child's stdio (stdin is always null; stdout and stderr are
piped, null or inherited as their `Capture` says), its session on Unix, and
its creation flags on Windows. Anything a caller sets there is overwritten.

A run ends one of three ways:

| Result | Meaning |
|---|---|
| `Ok(Outcome::Exited(output))` | The child exited **and** closed its output by the deadline. `output.status` is its exit status, success or not — a non-zero exit is the caller's to judge, not an error here. |
| `Ok(Outcome::TimedOut(stop))` | The deadline came first. The child has been stopped; `stop` says how (below). No output is returned: it is incomplete by definition. |
| `Err(error)` | The run could not be carried out: the child would not start, a stream could not be read (its reader failed, or a line callback panicked), a stream overflowed its `Capture::Full` limit, or the exit status could not be collected. Every error after the spawn force-stops the child before it is returned. |

### The deadline is on finishing, not on exiting

The deadline covers the whole run: the child must close its output **and**
exit within it. Waiting on exit alone deadlocks on a full pipe — a child that
writes more than the OS pipe buffer (~64 KiB) blocks until someone reads, and
never exits while nobody does — so a `system_profiler` on a machine with a
populated bus would be killed as wedged when it was only waiting to be read.
The crate reads every piped stream continuously on its own thread, so a child
never blocks on a pipe, and finishing means end-of-file on every stream plus
an exit status.

A child that finished while the run's last wait was still timing out
finished in time: before declaring a timeout, the run takes in whatever its
readers have already reported and looks once more for an exit.

A child that exits but leaves a descendant holding its output open has not
finished: the run waits for that output until the deadline, and then stops
the whole group, descendant included.

### Stopping escalates, and stops the whole tree

On the deadline:

1. **Graceful signal** to the child's process group — `SIGTERM` on Unix,
   `CTRL_BREAK_EVENT` on Windows.
2. **Grace period** (`GRACE_PERIOD`, 2 s, adjustable per run with
   `.grace()`): the run waits for the child to exit.
3. **Force-kill** of everything left — `SIGKILL` to the group on Unix,
   `TerminateJobObject` on Windows — whether or not the child itself
   answered the signal, so a descendant that ignored it does not survive.
4. **Reap** the child before returning — waiting at most 5 s (below).

`Stop::Terminated` means the child exited within the grace period;
`Stop::Killed` means it had to be force-killed. Callers that report the
difference (`plate-solver`'s `solve_timeout` message says `(terminated)` or
`(killed)`) read it from here rather than from timing — the two outcomes are
only a grace period apart, which is inside the spread a loaded host puts on a
run.

A graceful signal that cannot be delivered skips the grace period and goes
straight to the force-kill: there is nothing to wait for. On Windows that is
the normal case for a service, which has no console for `CTRL_BREAK_EVENT`
to travel through. A grace of zero does the same deliberately.

**A kill that does not land at once.** `SIGKILL` ends a running process at
once, but one in uninterruptible sleep — a read from a wedged USB device, the
very thing a USB inventory can hit — dies only when that I/O returns, and a
process the kill is not permitted to reach (a child that became another user)
does not die of it at all. Waiting on either would hang the caller, which is
the failure the crate exists to prevent. So the reap waits at most 5 s, and
past that hands the child to a background thread that reaps it whenever it
does exit; the run returns `Stop::Killed`. On Unix that unreaped child keeps
its pid, and so its group id, reserved until then — no unrelated group can be
given it. Windows has no zombies to reap.

**The tree, not just the child.** Commands that matter here are routinely
wrappers: `sh -c` / `cmd /C` around an operator's renewal hook or a restart
command, PowerShell around a CIM query. Killing the wrapper alone leaves its
children running and holding the output pipes. So each child is started at
the root of a tree of its own, and the stop reaches the tree:

- **Unix:** the child is the leader of a new session (`setsid`), which makes
  it the leader of a new process group, and both signals go to the group.
  The leader is checked for exit without being reaped (`waitid` with
  `WNOWAIT`), so its pid — which is also the group id — cannot be reused by
  an unrelated group before the force-kill lands.
- **Windows:** the child gets `CREATE_NEW_PROCESS_GROUP`, which is what
  `CTRL_BREAK_EVENT` addresses, and is placed in a job object right after it
  starts; the force-kill terminates the job. A grandchild started in the
  microseconds between the spawn and the assignment escapes the job — a
  window no shell or interpreter is fast enough to use. If the job cannot be
  created or assigned (an enclosing job that forbids it), the run degrades to
  terminating the child alone, logged at `debug`.

A run that **finishes normally** does not touch the tree: a descendant that
closed its output and outlived its parent did so on purpose.

**A session, not just a group, on Unix.** A child in a background process
group of the caller's session that reaches for the terminal — `ssh` asking
for a passphrase or to confirm a host key, `sudo` for a password — is stopped
by `SIGTTIN`/`SIGTTOU` and sits silent until its deadline. A new session has
no controlling terminal, so the same child fails at once, with an error of
its own on stderr for the caller to report. Unattended runs never had a
terminal to prompt on; an interactive one no longer pretends to.

**What a tree of its own costs.** Cleanup that works by process group no
longer reaches the child:

- A terminal's Ctrl+C goes to the foreground process group, so it reaches
  the caller but not the child. A caller killed outright — Ctrl+C on an
  interactive `doctor`, a crash — runs no destructor, and its child finishes
  on its own.
- launchd (Homebrew services on macOS) kills a job's leftover process group
  when the job exits. A service that exits normally drops its runs, and
  dropping a run stops it; but a service that crashes leaves its child to
  finish on its own.
- Nested runs: when a run's child is itself running a bounded child — the
  aggregation's per-service `doctor` running `system_profiler` — the stop
  reaches the first child's tree, not the second's, which is in a tree of its
  own. The inner run is bounded by its own deadline, and finishes under it.

systemd is unaffected: it stops a unit's whole cgroup, children included.

### Dropping a run stops it

A `Running` that is dropped without being waited for force-stops the tree
and reaps the child on the spot. A future that is dropped before it
completes — an HTTP request whose client went away, a `select!` that took
another branch — force-stops the run too, with no grace period, as `tokio`'s
`kill_on_drop` would; the caller that walked away does not wait even for
that, because the thread that was waiting for the child carries the stop out.

**Unless the run must not be cut off halfway.** A run built with
`.finish_if_abandoned()` is left to finish when its future is dropped —
still under its deadline, still reaped, its result discarded. Sentinel's
restart commands are built this way: a supervisor cancelled mid-restart must
not kill `Restart-Service` or `brew services restart` between its stop and
its start, which would leave the service down.

## Capturing output

| `Capture` | What the caller gets | Use |
|---|---|---|
| `Discard` (default) | an empty buffer; the stream is null, unless a line callback needs stdout | output nobody reads |
| `Full(limit)` | everything the child wrote; a child that writes more than `limit` bytes is force-stopped and the run fails with `Error::OutputLimit` | output a caller parses — a truncated report must not read as a short one |
| `Head(len)` | the first `len` bytes; later ones are read and dropped | diagnostics whose clue comes first — an argument parser's error is its first line |
| `Tail(len)` | the last `len` bytes; earlier ones are read and dropped | diagnostics whose clue comes last |
| `Inherit` | an empty buffer; the child writes straight to the caller's own stream, live, unless a line callback needs stdout | a harness whose log is where the child's output belongs |

`OUTPUT_LIMIT` (16 MiB) is a ceiling for output a caller parses, far above
any report a well-behaved tool writes and far below anything that would
strain a small host. `STDERR_TAIL` (4 KiB) is enough for the error context a
diagnosis needs. Both are defaults to reach for, not limits the crate
enforces on its own.

`on_stdout_line` hands each line of stdout to a callback as it arrives —
the ConformU runner streams progress into the test log, and collects the
lines it judges, this way — on top of whatever `stdout`'s `Capture` keeps.
Lines are split on `\n`, a trailing `\r` is dropped, invalid UTF-8 is
replaced, an unterminated last line is delivered at end-of-file, and a line
longer than 64 KiB is delivered in pieces rather than buffered without bound
(its newline then ends the last piece, not an empty line of its own). The
callback runs on the stdout reader thread, so it must be `Send + 'static`; a
slow callback slows only the reading (the deadline still holds), and one that
panics fails the run with `Error::Read` at once rather than leaving it to wait
out its deadline on a stream nobody reads.

## Sync and async from one implementation

`run()` blocks the calling thread. `spawn()` starts the child and returns a
`Running`; `Running::wait()` blocks, and `Running` is also `IntoFuture`, so
`.await` waits without blocking an executor. The future moves the same wait
onto a thread of its own and wakes the task when it ends. It needs no
runtime, so the crate carries no `tokio` dependency into
`rusty-photon-doctor-checks`, and Cargo and Bazel build the same single
library.

The cost is threads: one per piped stream, plus one per awaited run. Every
caller runs a handful of children at a time at most, so this is a few
threads, never a pool.

## Deadlines per caller

The deadline is each caller's, sized to sit above the slowest legitimate run
and below anything that would stall what waits on it:

| Caller | Deadline | Output | Why that deadline |
|---|---|---|---|
| `rusty-photon-doctor-checks` USB inventory (`system_profiler`, PowerShell) | 10 s | stdout `Full` | A passive scan is a handful of cached reads; anything slower is a wedged child. It runs at camera-service startup. |
| doctor's platform inspectors (`systemctl`, PowerShell CIM, `brew services`) | 30 s | stdout `Full`, stderr `Tail` (`is-active`: exit status only) | Above `systemctl`'s own 25 s D-Bus timeout, so when systemd is the problem `systemctl`'s error arrives first. |
| doctor's aggregation (`<svc> doctor --json`) | 1 min | stdout `Full`, stderr `Head` | An SDK bus scan takes seconds. The report names stderr's first line. |
| doctor's post-renewal hooks | 5 min | stderr `Tail` | Hooks copy certificates to other machines; an `scp` to an unreachable host takes about two minutes for TCP to give up. |
| `plate-solver` ASTAP | the request's `timeout` | stderr `Tail` | Per request, see [plate-solver.md](../services/plate-solver.md#subprocess-supervision). |
| `sentinel` discovery listings | 10 s, zero grace | stdout `Full`, stderr `Tail` | Listings are small and the tools quick. |
| `sentinel` restart commands and recovery checks | their share of the restart budget, zero grace, finish if abandoned | stderr `Tail` | The budget is the caller's whole allowance for a restart and its recovery, so nothing runs past it; see [sentinel.md](../services/sentinel.md). |
| `bdd-infra` ConformU runner, per mode | 30 min | stdout line callback, stderr `Inherit` | The `conformu.yml` step's own limit: no run is allowed longer anywhere CI runs it, and a local `cargo test` against a wedged ConformU now fails with the mode named instead of hanging. |

## Errors

`Error` is a `thiserror` enum. Its messages describe the child as "the
child", never by its command line: arguments can carry an operator's hook
text or a path a log should not repeat, so the caller adds the context it
knows is safe.

## Testing

Mechanism tests spawn real children through `shell` and assert outcomes
rather than timings: a marker file a stopped grandchild would have written
proves the tree was stopped (and one an abandoned-but-finishing run did write
proves it was not), a payload larger than a pipe buffer proves the drain,
`Stop::Terminated` versus `Stop::Killed` proves which stage the stop reached,
and on Linux `/proc` proves the child leads its own session. The one timing
assertion is a floor — a force-kill waited out the grace — which load can
only make longer (see
[testing.md §5.10](../skills/testing.md#510-assert-the-effect-not-the-timing-when-a-failure-mode-is-the-os-killed-it)).
Pure parts — the head and tail windows, the `Full` limit, line splitting,
which stream is piped — are unit tests.

The bounded reap is not tested: no portable fixture makes a kill fail to
land. It is a timeout around a `try_wait` loop, read rather than run.

## Consumers

`rusty-photon-doctor-checks`, `doctor`, `plate-solver`, `sentinel`, and
`bdd-infra` (behind its `conformu` feature).
