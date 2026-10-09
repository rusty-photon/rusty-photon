# Plan: shared-transport reconnect ownership — every reconnect attempt has one owner

**Status: OBSOLETE (archived 2026-10-08).** The plan was never
followed as written. Its one bug that production demonstrably reaches,
the stale reply PR 0 targeted, is fixed by a different mechanism under
[#1448](https://github.com/rusty-photon/rusty-photon/issues/1448). PR 6,
PR 7 and one of PR 1's pins land in the same pull request; the rest is
dropped.

- **How #1448 fixes it.** An exchange runs to completion once it holds
  the command lock, so an abandoned request reads its own reply instead
  of leaving it for the next request to discard. That supersedes
  decision 7. On rig2's FP2 the fix saw 89 hang-ups land mid-exchange
  and 0 misattributed replies in 2,435 exchanges. The installed build
  shifted on the first hang-up in 4 runs out of 4.
- **Delivered in the same PR,
  [#1449](https://github.com/rusty-photon/rusty-photon/pull/1449):**
  - PR 7. `reconnect_now` is a `test-util` hook that production builds
    do not compile, so "nothing in production calls it" is a compile
    error rather than a grep result.
  - PR 6. `shutdown` closes what the slot holds when it takes it, which
    closes W1-LEAK for any publisher.
  - From PR 1, the pin of the post-join re-store
    (`a_recovery_published_inside_the_shutdown_join_is_withdrawn_after_it`).
- **Why the rest is not pursued.** With `reconnect_now` out of
  production builds, the lifecycle windows the plan was written for (W1,
  W2, W3 and the Lazy variant, the subject of
  [#1243](https://github.com/rusty-photon/rusty-photon/issues/1243)) are
  reachable only through W5. That needs a reconnect attempt that does not
  yield for 5 s, such as a wedged synchronous serial open, before the
  supervisor's join gives up on it. PR 8's single-owner rewrite is
  therefore not pursued. What W5 still allows, unchanged by this PR:
  - the orphaned attempt holds the port while it opens and handshakes,
    so a reload opening the same port races it;
  - if it then publishes into the lifecycle being torn down, PR 6
    closes that conduit, so it keeps no port afterwards;
  - a poll task it respawns after the teardown drained the old one (W1)
    keeps polling that closed conduit, its requests failing as closed,
    until the process exits.
- **Decision 6.** Its reason no longer holds: a poll task aborted
  mid-request no longer strands a reply.
- **Dropped:**
  - the rest of PR 1, the pins of guarantees that already hold;
  - PR 2, bug B, which has no effect today;
  - PR 3, bug A, latent: every poll body only inquires;
  - PR 4, W5 itself, which needs the 5 s stall above;
  - PR 5, W4b, which costs availability only.

Decisions were settled by Igor on 2026-10-03 and revised on review on
2026-10-04 (decisions 6 and 7).

In `rusty-photon-shared-transport`, a reconnect attempt can outlive the
lifecycle it started in. It can then publish a conduit, respawn a poll
task or advertise a recovery into a transport that has already been
torn down. The issue proposed a lifecycle generation: `shutdown()`
bumps a counter, and every step of an attempt checks it.

This plan reaches the same guarantee by removing the reason a counter
would be needed. Every attempt becomes a child of the reconnect
supervisor, so a teardown that cancels and joins the supervisor has
joined every attempt. The supervisor's `CancellationToken` serves as
the generation, and the attempt checks it at three commit points.

Three smaller changes complete the fix:

- the last-client cleanup gets a wake of its own;
- `shutdown` closes whatever the slot holds at the moment it empties
  it;
- `reconnect_now` stays as a test hook that production builds do not
  compile.

Two bugs found during the investigation ride along: the background
poll ignores the safety debt, and an open path that fails drops its
conduit instead of closing it.

A third fix lands first, as PR 0: a request abandoned after its send
leaves its reply unread, and today every later request on that conduit
reads the reply meant for the one before it. After PR 0 the abandoned
request owes its reply, and the next request discards it.

## Problem

This is the state on `main` at `d201506c`. Each window was reproduced
or refuted with a scratch test and an adversarial review. The labels
follow the investigation.

| Window | What goes wrong | Reachable in production | Closed by |
|--------|-----------------|-------------------------|-----------|
| **W1-LEAK** (port leak) | `shutdown()` reads the cell without taking it. It closes the conduit it cloned *before* its hook ran, and empties the slot only afterwards; this came in with `6fab4d02`, inside #1241. An attempt that publishes while the hook runs therefore passes `still_in_slot`, and its replacement is never closed. The result: the port stays held, the respawned poll task keeps polling a live device, and the reload is refused with "Access is denied". The issue body's "neither window leaks a port" is no longer true. | Only through W5, or through `reconnect_now` | PR 4 (supervisor route), PR 6 (any publisher), PR 8 |
| **W1** (poll task after shutdown) | The attempt respawns `while_open` after `shutdown` has drained `while_open_state`, and nothing drains it again. | No: needs `reconnect_now`, or the W5 corner | PR 8 |
| **W2** (lying flag) | `reconnect_now`'s `available = true` lands after teardown. `acquire()` then returns "refcount > 0 but slot empty", and `start()` takes its idempotent early return. The supervisor's own variant (W2s) is neutralised by the post-join re-store, but no test pins that re-store. | No: needs `reconnect_now` | PR 1 pins the re-store; PR 8 |
| **W3** (overlapping manual reconnects) | Two `reconnect_now` calls write their flags outside `attempt_reconnect_lock`, so the writes interleave. | No: needs `reconnect_now` | PR 8 |
| **W4b** (cleanup permit) | For a stop that did not land, `run_cleanup_locked` raises `reconnect_signal` (shared.rs:2152, from #1286). If the attempt already in flight pays that debt, the permit outlives it, and the supervisor closes and reopens the healthy replacement once for nothing. | Yes, on GTi only: a window of microseconds, costing availability only | PR 5 |
| **W5** (abandoned attempt) | A supervisor that misses the 5 s join is aborted. Aborting the parent does not stop its child attempt, which goes on opening or handshaking while the reload opens the same port. | Conditional: needs an attempt that does not yield for 5 s, such as a synchronous serial open in a wedged driver | PR 4 |
| **Lazy variant** | `reconnect_now` publishes during the LazyAcquire 1→0 cleanup, and the next `start()` promotes onto an empty slot. | No: needs `reconnect_now`, and no service runs LazyAcquire | PR 8 (made unreachable) |
| **Bug A** (poll ignores the debt) | `WhileOpen::request` skips the safety-debt check that `Session::request` makes. "No request reaches the device while a stop is owed" therefore holds for sessions only. | Yes, on GTi after a failed last-client stop. Latent: every poll body today only inquires. | PR 3 |
| **Bug B** (drop on a failed handshake) | When a handshake fails after `factory.open()`, the code drops `new_conn` instead of closing it. This happens in the attempt, in the cold start, and in the lazy 0→1. Related: a `while_open` constructor that passes its `WhileOpen` on and then panics leaks the port on a cold start or a lazy 0→1, because those two paths do not catch the panic. | No effect today: the opener holds the only `Arc`, so dropping it is the same as closing it. The constructor case is latent: no production constructor can panic | PR 2 |
| **Stale reply** (found on review) | `Connection::request_timed` is not cancel-safe. A request dropped after its send leaves its reply unread. Nothing discards it, and a stray frame never signals a reconnect: a decode error or `SkipExhausted` does not signal, and under the default `matches` the stray is simply taken as the reply. So every later request on that conduit reads the reply meant for the one before it, until a request times out or the conduit is reopened. GTi and dsd-fp2 cannot tell a stray from a reply at all, and dsd-fp2's `(OK)`, `(0)` and `(1)` replies are byte-identical across commands, so wrong values are taken silently. | Yes: a lifecycle abort mid-request, or an HTTP handler dropped mid-exchange (not reproduced). §Deliberately left names the routes, R1–R4 | PR 0 |

Two windows are already closed and are not reopened here:

- W4 as filed, closed by #1241 (close-before-open plus the
  post-handshake drain).
- W6 as a safety issue, closed by #1286 (`publish_recovery`'s undo
  plus the Session debt gate).

PR 1 pins both as regression tests.

**What production can reach today.** All seven shared-transport
services run ServiceLifetime, and every reload builds a fresh
`SharedTransport`. A wrong flag on the old instance therefore dies with
that instance. The harm that reaches the successor is a held port,
which is W1-LEAK and W5.

`reconnect_now` has no production caller. Comment 4 on the issue warns
that this is a snapshot, not a property of the code. That is why the
windows it opens are closed in the code (PR 8), and why the hook
becomes a compile-time test option (PR 7), instead of being declared
unreachable.

## Implementation Status

| PR | Description | Status | Branch / PR |
|----|-------------|--------|-------------|
| 0 | A request abandoned after its send owes its reply, and the next request discards it | Not started | |
| 1 | Pin the guarantees that already hold: the post-join re-store, W4 as filed, W6 (tests only) | Not started | |
| 2 | Bug B: an open that fails before the publish closes the conduit it opened | Not started | |
| 3 | Bug A: the poll task answers to the safety debt too | Not started | |
| 4 | W5: a teardown waits for the reconnect attempt instead of abandoning it | Not started | |
| 5 | W4b: a missed last-client stop wakes the supervisor on its own signal | Not started | |
| 6 | W1-LEAK: `shutdown` closes what the slot holds when it empties it | Not started | |
| 7 | `reconnect_now` becomes a `test-util` hook | Not started | |
| 8 | One owner for every reconnect attempt; the token as the generation | Not started | |

The order follows production risk:

- **PR 0** comes first (decision 7). Its bug is the one here that needs
  no teardown, it is `Connection` work that none of the lifecycle PRs
  touch, and it is what lets PR 8 keep aborting a poll task mid-request
  (decision 6).
- **PR 1** puts a net under the join that PR 4 edits.
- **PR 2** comes before the other fixes. It changes nothing a production
  service can observe, and it leaves `WhileOpen` built in one place,
  so PR 3 changes one construction site instead of three. It must also
  precede PR 8, so that commit point C2 goes in as a two-line insertion
  ahead of `handshake_or_close`.
- **PRs 3–6** close everything production can reach.
- **PR 7** turns "nothing in production calls `reconnect_now`" from a
  grep result into a compile error. It is independent of the others
  and can move earlier at no cost. It edits the crate's `Cargo.toml`,
  so it needs `scripts/repin-bazel-lock.sh`, merged in
  [#1391](https://github.com/rusty-photon/rusty-photon/pull/1391).
- **PR 8** closes W1, W2, W3 and the Lazy variant in the code.

Each PR is a separate change that leaves `main` green, but only when it
lands on top of its predecessors. Land them in order. The one
exception is PR 7, which needs nothing else in the series and can land
at any point. The dependencies:
- PR 0 needs nothing else in the series. PR 8 relies on it: the abort
  of a poll task mid-request that PR 8 keeps strands no reply only once
  PR 0 has landed.
- PR 3 builds on PR 2's `build_while_open`.
- PRs 4, 5, 6 and 8 edit lines that PR 3 renames.
- PR 1's tests are the net PR 4 edits under.
- PR 8 builds on PRs 2 through 7:
  - C2 goes in ahead of PR 2's `handshake_or_close`;
  - PR 3's `SafetyDebt`;
  - PR 4's `Supervisor` and `retire_supervisor`;
  - PR 5's `stop_owed` arm;
  - PR 6's close-at-take;
  - PR 7's `test-util` gate, which lets PR 8 carry no `!`.

Hardware validation gates PRs 4 and 8.

The crate has no design doc under `docs/crates/`. Its module rustdoc is
where the invariants live, so a PR's rustdoc change is its design-doc
change under rule 2 and
[development-workflow.md](../../skills/development-workflow.md).

## Decisions (settled 2026-10-03; 6 revised and 7 added on 2026-10-04)

1. **One owner with a cancellation token, instead of a counter
   generation.**

   Comment 4 on the issue names the cause: the supervisor's attempt is
   safe because it is *joined*, and "a `reconnect_now()` from another
   task is joined by nothing". A counter would make a second owner's
   attempts detect a teardown. Removing the second owner means there
   is nothing left to detect: every attempt is a child of the
   supervisor, and every teardown joins the supervisor.

   The supervisor's `CancellationToken` already behaves like the
   generation:
   - a teardown cancels it before touching any state;
   - a new lifecycle gets a new token;
   - the attempt carries a clone and checks it at its three commit
     points.

   This also closes W3 by construction, because attempts queue on one
   task. A counter would have needed the attempt lock widened, and
   would still have left the LazyAcquire residual.

   The module doc records the two conditions under which a real
   counter would become necessary:
   - a second owner of attempts;
   - a requirement that `shutdown` finish in bounded time while an
     attempt is wedged.

2. **A teardown waits for the attempt without a bound, warning at
   5 s.**

   Today `shutdown` gives up after 5 s and aborts the supervisor. That
   leaves the child attempt running with the port in its hands, and
   the reload then collides with it.

   Waiting costs nothing in the ordinary case. The supervisor answers
   cancellation at every await of its own and aborts its child itself,
   so the only thing a teardown can still be waiting on past 5 s is a
   call that has not yielded since:
   - If that stall ends, the reload gets a clean port.
   - If it never ends (a kernel D-state, say), it would also block the
     runtime from dropping at process exit, so a bound recovers
     nothing. `TimeoutStopSec` and the Windows SCM stop timeout still
     apply to a final stop.

   At 5 s the teardown logs one `warn!` naming the wait. When the join
   returns, it logs a `debug!` with the total.

3. **`reconnect_now` stays for the tests, compiled only under
   `test-util`.**

   About 60 call sites in the integration tests (`reconnect.rs` 57,
   `lifecycle.rs` 5) use it to drive a reconnect deterministically, so
   it stays.

   It moves out of the production API behind a cargo feature that only
   this crate's own test targets enable. A future operator CLI would
   then need an explicit un-gating, and that is the point to add a
   cadence floor for manual kicks.

   The gate does not replace decision 1. It only keeps the hook out of
   production builds. The windows the hook opens are closed in the
   code by PR 8, so un-gating it later reopens nothing.

4. **The cleanup keeps its wake, on its own `stop_owed` signal.**

   A stop the device refused, or a hook that panicked, raises no wire
   failure. Without a wake, the replay of such a stop waits up to one
   reconnect interval (5 s by default). The Session debt gate keeps
   the mount safe throughout that wait, but the wait is still avoidable.

   The trouble today is that the wake rides `reconnect_signal`, whose
   meaning is "a request failed on the wire; take the transport out of
   service". A permit that outlives its debt therefore cycles a healthy
   replacement (W4b).

   `stop_owed` is a separate `Notify`:
   - The supervisor acts on it only while `reconnecting` is set, and
     never writes a flag for it.
   - While a debt stands, `reconnecting` is always set, so the wake is
     never ignored.
   - `UnlandedStateGuard` raises it as well. A panicked stop, which
     today waits for the next link failure or the next tick, wakes the
     supervisor at once too.
   - The wake does not skip the cadence floor. If no attempt started
     within the last reconnect interval, the replay starts at once.
     Otherwise it waits out the rest of that interval: at most one
     interval, which is 5 s in production, because no service changes
     the default. Letting the wake skip the floor was rejected on
     review (§Rejected alternatives).

5. **Two bug fixes ride along.**

   *Bug A.* The crate's own invariant — while a stop is owed, no
   request reaches the device — holds for `Session` only. The poll
   task's `WhileOpen` bypasses it. That is reachable on GTi after a
   failed last-client stop. It is harmless today only because every
   poll body inquires; a future poll that actuates (temperature
   compensation, auto-dew writes) would command a mount whose halt
   never landed.

   *Bug B, as filed,* changes nothing observable. The opener holds the
   only `Arc`, so dropping the conduit releases it exactly as closing
   it would. It ships anyway, as consistency hardening with the
   crate's convention: close explicitly, never rely on the last `Arc`.
   The investigation behind B also found a related latent leak: a
   `while_open` constructor that hands its `WhileOpen` elsewhere and
   then panics. Closing that one changes public behaviour, so it ships
   as the PR's second commit, approved on 2026-10-03.

6. **A teardown may abort a poll task mid-request, because PR 0 counts
   the reply it strands** (revised on review, 2026-10-04).

   As first merged, this decision kept the poll task registered in
   `while_open_state` until a join of it returned, so that no teardown
   aborted it mid-request. The case is an attempt that the supervisor's
   cancel arm aborts inside its own `cancel_while_open`.
   `AbortDetachedGuard` then aborts the poll task, which can drop a
   request between its send and its receive. Without PR 0, the
   stranded reply answers the shutdown hook's first stop.

   PR 0 removes that reason. The dropped request owes its reply, and the
   hook's first stop discards it before reading its own. On UDP, a
   reorder can still hand that stop the poll's reply, which fails its
   decode and so fails safe (decision 7). So
   `cancel_while_open` and `AbortDetachedGuard` stay as they are, and
   PR 8 drops the lock held across the join, the invariant that a task
   leaves `while_open_state` only after its join returns, and the three
   tests that pinned it. This also removes that version's cost: an
   abandoned teardown no longer leaves a stubborn poll task running
   until the next teardown, or detached if the instance is dropped
   first.

7. **A cancel-safe exchange lands first, as PR 0** (settled on review,
   2026-10-04).

   `Connection` counts the reply frames that abandoned exchanges owe:
   - **A request dropped after its send completes owes one frame.** That
     covers a drop while it waits for the reply, a drop part-way through
     reading it, and a drop while it skips frames under `max_skip`.
   - **A request dropped part-way through its send owes one frame too.**
     Whether the device answers a torn frame is unknown, and owing one
     is the direction that heals (below).
   - **A receive timeout owes nothing.** A reply that misses its read
     timeout is more often lost than late, especially over UDP, and the
     timeout already raises a reconnect.

   The next exchange sends at once, reads and discards the owed frames,
   and only then reads its own reply under the usual `matches` and
   `max_skip`. Discarded frames are not decoded and do not count
   against `max_skip`.

   Why this shape:
   - **No send waits.** A stop goes out as soon as it has the lock.
   - **It works on whole frames.** A half-read reply's remainder ends at
     the terminator, so it is one frame. Bytes already in the serial
     transport's `BufReader`, or in the read-ahead buffer tokio-serial
     keeps on Windows, are read and discarded like any other frame. No
     `FrameTransport` method is added.
   - **Counting too many heals; counting too few would not.** If an
     owed reply never comes (a lost UDP datagram, a torn frame the device
     ignores), the next exchange discards its own reply and times out.
     That raises a reconnect, and the replacement conduit starts at zero.
     That is what a lost reply does today. A count that is too low would
     leave the conduit one frame behind indefinitely, which is the bug.
   - **The cost of counting too many lands on the stop only by
     coincidence.** If the exchange that pays is GTi's safety stop,
     `:L1` still goes out at once, `:L2` waits out one read timeout, and
     the verdict is `NotAsserted`, so the debt replays the stop. That
     needs an abandoned exchange and the loss of its reply together.

   **It assumes replies arrive in order.** Serial delivers them in
   order. UDP can drop, reorder or duplicate them
   ([star-adventurer-gti.md](../../services/star-adventurer-gti.md) says
   so where it explains why a cache of firmware state is unsafe over
   UDP), and GTi over UDP is the one such conduit in the workspace.
   - **What a reorder costs.** If an owed reply arrives after the next
     request's own, that request discards its own reply as the owed
     one, and takes the owed reply as its answer. Both frames have now
     been read, so the conduit is back in step from the next request
     on. That is one misattributed reply, where today every later
     reply is misattributed.
   - **When it can happen.** The owed reply must still be in flight when
     the next request is sent, and the two datagrams must then cross on
     the link. That needs the abandonment to fall within about one round
     trip of the next send.
   - **What it does to GTi's safety stop.** A stray poll reply read in
     place of `:L1`'s ack fails the decode, because an ack carries no
     payload. The verdict is `NotAsserted` and the debt replays the
     stop, the same fail-safe outcome as an uncounted stray today. A
     stray ack read in place of `:L1`'s ack is accepted, but `:L1`'s
     own ack did arrive, as the frame that was discarded, so the
     verdict still holds. Either way all three stop frames go out.
   - **Why not more.** GTi's replies carry no tag, so no codec can
     correlate a reply with its request. Poisoning the conduit instead
     would close the gap on UDP, since a fresh socket receives nothing
     meant for the old one, but at the costs §Rejected alternatives
     lists, including the stop that most needs to go out.

   This decision moved the work ahead of PR 1. The plan as first merged
   listed it as a follow-up issue. It goes first for two reasons. The
   stale reply is the one problem here that needs no teardown: an HTTP
   handler dropped mid-exchange can leave one in any of the seven
   services (R3, not reproduced), and it then lasts until a request
   times out or the conduit is reopened. And PR 0 lets decision 6 be
   undone.

### Rejected alternatives

- **Counter generation plus a commit lock.** Two attempt owners
  remain, and so does the LazyAcquire residual. Staged alone, its
  first step wedges a newer lifecycle. It also adds `parking_lot` and
  a discipline of keeping the flags behind a `Commit`.
- **A lifecycle object that replaces the state flags.** Roughly +2.7k
  and −1.25k lines, rewriting every transition in the crate that
  carries the GTi halt and the Windows port-release fixes. Its
  guard-only sync sections trip a denied nursery lint.
- **A `test-util` gate *alone* as the fix.** Every window stays in the
  code and reopens the day the hook is un-gated. Decision 3 gates the
  API *on top of* single ownership, and it is single ownership that
  closes the windows.
- **Taking `acquire_lock` inside `attempt_reconnect`.** This deadlocks:
  `shutdown` holds that lock while it joins the supervisor running the
  attempt.
- **Holding the slot guard across the respawn.** This nests
  `while_open_state` inside `slot`, the reverse of the cold-start and
  lazy publishes, which take `while_open_state` and then `slot`.
  Taking both at C3 in the publishes' order is a separate alternative,
  also rejected (below).
- **Keeping a poll task registered until its join returns** (decision 6
  as first merged; undone on review, 2026-10-04). After PR 0 the reply
  an abort mid-request strands is counted and discarded. Keeping it
  would leave the lock held
  across a join and an extra invariant for no remaining benefit.
- **For PR 0: discard pending input before the next send.** It needs a
  new `FrameTransport` method. It can only discard what has already
  arrived, so to be sure it must wait for the line to go quiet, up to a
  read timeout, and that includes before a stop. `PurgeComm` cannot
  reach the bytes tokio-serial has already read ahead on Windows. The
  GTi stop-coast probe drains this way, and it deliberately never
  drains before a stop.
- **For PR 0: poison the conduit and reconnect.** Every abandoned
  exchange would cost a reconnect: up to one reconnect interval, then
  the handshake (ten requests on GTi). A stop on a poisoned conduit
  would fail without being sent and wait for the replay. On UDP it
  would also close the reorder gap that owing leaves (decision 7). That
  does not justify it: a reorder costs one misattributed reply, while
  poisoning would hold back the shutdown stop after every aborted poll.
- **For PR 0: codec matching alone.** The codecs differ in how much
  they can tell apart:
  - qhy-focuser matches every reply to its command, by `cmd_id`;
  - ppba-driver and upbv2-driver prefix-match a set command's echo
    against the command string, and otherwise match the reply's class;
  - pa-falcon-rotator and pa-scops-oag match the reply's shape, and
    check an echo's content above the codec;
  - GTi and dsd-fp2 use the default `matches`, which accepts any frame.

  None of them can reject an earlier reply to the same command.
- **For PR 0: owing a frame on a receive timeout.** When the reply was
  lost rather than late, the next exchange would discard its own reply
  and stall for a read timeout. Until the reconnect replaces the
  conduit, that exchange is the poll or a stop hook. GTi's `:L2` would
  then wait 2 s.
- **Gating supervisor wakes on the debt or on `reconnecting`.** This
  drops genuine wire wakes.
- **A bounded teardown that detaches the attempt.** This hands the
  reload a certain collision, and leaves behind an attempt that can
  still publish.
- **Deleting the cleanup's wake outright.** This closes W4b, but a
  refused or panicked stop then waits up to one interval.
- **A pending-attempt counter in `reconnect_now`.** It closes the first
  W3 ordering and leaves the second untouched, as the issue itself
  notes.
- **Respawning the poll task inside C3** (rejected on review,
  2026-10-04). It would take `while_open_state` and then `slot` at C3,
  in the publishes' order. That closes the stale child's late
  registration (§Deliberately left, abandoned teardown), and lets the
  publishes assert that nothing is registered. That corner needs an
  abandoned teardown, which production never does, plus a multi-thread
  race of microseconds. It does not justify nesting two locks at the
  attempt's last commit point.
- **Letting a `stop_owed` wake skip the cadence floor after a
  recovery** (rejected on review, 2026-10-04). The floor delays the
  replay only when the stop is refused within one interval of a
  recovery's attempt, and then only by the rest of that interval. A
  stop refused that soon usually means the link has dropped again, so
  an immediate attempt would most likely fail and fall back to the
  floor anyway. Skipping it would also let a client that connects and
  disconnects in a loop cycle the port at the client's rate, against a
  device that refuses its stop and then accepts the replay. "Every
  attempt waits out the floor" stays the one rule.

## Design

### State after PR 8

- **`supervisor_state: Mutex<Option<Supervisor<C>>>`**, with
  `Supervisor { task, cancel, kicks }`. `kicks` is the sending half of
  an unbounded mpsc channel of oneshot reply senders.
  - Only `spawn_supervisor` (register) and `take_supervisor`
    (deregister) write it, and every caller of either holds
    `acquire_lock`.
  - The test-only `reconnect_now` takes it alone, and only for a
    synchronous `send`.
- **The kick receiver lives inside the supervisor future.** When the
  supervisor ends, queued kicks are dropped, and their callers hear
  "the reconnect supervisor stopped before the attempt finished".
- **`cancel` is the lifecycle generation.**
  - Every teardown cancels it before it touches the slot, the cell,
    `while_open_state` or any flag other than its own first
    `available = false` (invariant 3).
  - A cold start or a promotion gets a fresh token.
  - An attempt receives a clone when it is spawned.
- **`stop_owed: Notify`** (PR 5) is raised only by `run_cleanup_locked`
  (ServiceLifetime, stop not landed) and by `UnlandedStateGuard`
  (ServiceLifetime). Both raise it after the debt increment and the
  `reconnecting = true` / `available = false` stores, with no await in
  between. Only the supervisor consumes it.
- **`reconnect_signal`** has one raiser: `Connection::request` on a
  wire failure. The supervisor still takes the transport out of
  service unconditionally when it fires.
- **`safety_debt: Arc<SafetyDebt>`** (PR 3) replaces the two loose
  counters. `WhileOpen` holds a clone of it.
- **Removed:** `supervisor_live` and `ManualReconnectGuard`.
- **Never added:** the design panel's `IN_ATTEMPT` task-local (see
  below).
- **Kept:** `attempt_reconnect_lock`. Its doc narrows to the one thing
  it still does: serialising a stale child, left behind by an
  abandoned teardown, against the current one.

### Invariants

1. **One owner.** Every reconnect attempt is a child task of the
   registered supervisor. Nothing else calls `attempt_reconnect`.
2. **Who puts the transport back in service.** `available = true` /
   `reconnecting = false` are written only by:
   - holders of `acquire_lock`: the cold-start publish, the lazy 0→1
     publish, `shutdown`, and `ColdStartGuard`;
   - the supervisor's `publish_recovery`.

   `reconnecting = true` / `available = false` are written only by:
   - the supervisor's wire wakes and kicks (`take_out_of_service`);
   - the ServiceLifetime cleanup and `UnlandedStateGuard`, debt first;
   - `publish_recovery`'s undo.

   While a supervisor is registered, nothing outside its task can put
   the transport back in service. Every teardown joins that task to
   completion while it holds `acquire_lock`.
3. **The token moves first.** Every teardown cancels the lifecycle
   token before it touches the slot, the cell, `while_open_state` or
   the flags. An attempt that sees its token cancelled commits
   nothing.

   There is one exemption, `shutdown`'s step 2 store of
   `available = false`, which comes before the cancel (§Teardown).
   - It only takes the transport out of service, so sessions are
     refused from that moment rather than from the end of the join.
     Nothing an attempt could commit depends on it.
   - A publish that lands between it and the cancel is undone by
     step 5's re-store after the join, which PR 1 pins. That re-store
     comes after the cancel, so it needs no exemption.
   - `release_any_held_conduit` writes no flag before its cancel.
4. **Nothing aborts the supervisor.** A teardown cancels it and joins
   it: `warn!` at 5 s, then wait without a bound. Its handle is never
   wrapped in `AbortDetachedGuard`, because aborting the parent is
   exactly what drops a still-running child's handle.
5. **The debt is the authority for both request handles.** `Session`
   and `WhileOpen` each refuse a request while
   `safety_debt.outstanding()`. The flags only publish that fact.
6. **The opener closes what it abandons.** A conduit that an open path
   gives up before the publish is closed by that path: handshake
   failure, constructor panic, C2 and C3. The one exception is a
   cancellation point, where the conduit is dropped instead. That
   still releases it, because nothing else holds it yet.
7. **A request abandoned after its send owes its reply** (PR 0). The
   next request on that conduit sends, discards the owed frames, and
   only then reads its own reply. A receive timeout owes nothing; its
   late reply, if one comes, is left to the reconnect that the timeout
   raises.

### Lock order

Outer to inner:

`acquire_lock` → `supervisor_state` → `while_open_state` → `slot` → the
cell's `RwLock` → the `Connection` command lock.

- `supervisor_state` is taken only for a synchronous take, register or
  send. It is never held across a join.
- `last_attempt` and `reconnect_interval` are leaves, held for a single
  statement.
- The supervisor and its child never take `acquire_lock` or
  `supervisor_state`.
- The child takes `slot` and then `cell.write` for the publish,
  releases both, and only then takes `while_open_state`. That is the
  same as today, and causes no inversion.
- `reconnect_now` takes `supervisor_state` alone, then awaits its
  oneshot with no lock held.
- PR 0 adds no lock. The owed count lives with the conduit, behind the
  `Connection` command lock, so only a holder of that lock reads or
  changes it.

**Why the unbounded join cannot deadlock.** While `shutdown` waits, it
holds only `acquire_lock`. A child blocked on any async wait, including
a hook that calls `acquire()`, is `Pending`, so the supervisor's
cancel arm drops it at once. Only a stall that never yields extends
the wait, and such a stall holds the port whatever the teardown does.

### The attempt and its three commit points

The attempt runs as a child task and holds `attempt_reconnect_lock`
for its whole life:

1. Stamp `last_attempt`, read the no-client snapshot, run
   `cancel_while_open`, and read the cell from the slot.
2. **C1:** if the token is cancelled, return `Err` with no side
   effects. Nothing has been closed, so the teardown's GTi hook runs on
   the live conduit, and a stale child cannot close a newer
   lifecycle's conduit.
3. Close the old conduit (close-before-open), then call
   `factory.open()`.
4. **C2:** if the token is cancelled, close `new_conn` and return
   `Err`. A handshake never runs on a lifecycle that has ended.
5. Run `handshake_or_close` (PR 2), take the failure baseline, run the
   post-handshake drain, and build the `while_open` future inside
   `catch_unwind`.
6. **C3, under the slot guard:** compute
   `!cancelled && ptr_eq(slot, cell)`. If true, write the cell. If
   not, close `new_conn` and return `Err`.
7. If this attempt will replay a stop, record it as owed now (PR 3).
   Then respawn `while_open`, replay, and run `commit_replacement`.
8. Return to the supervisor. Its join select is biased cancel-first: if
   cancel fired, the outcome is discarded and nothing is advertised.
   Otherwise `publish_recovery` runs on the supervisor task, and then
   the kick, if there was one, gets its reply.

```rust
if lifecycle.is_cancelled() { return Err(torn_down()); }                          // C1
cell.read().await.close().await;                                                  // close-before-open
let raw = self.factory.open().await.map_err(SessionError::Transport)?;
let new_conn = Arc::new(Connection::new(raw, self.codec.clone()) /* .with_reconnect_signal(…) */);
if lifecycle.is_cancelled() { new_conn.close().await; return Err(torn_down()); }  // C2
self.handshake_or_close(&new_conn).await?;
// … baseline, drain, while_open future …
let slot = self.slot.lock().await;
let current = !lifecycle.is_cancelled() && slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &cell)); // C3
```

### Teardown

`shutdown` after PR 8:

1. Take `acquire_lock`. In LazyAcquire, return.
2. Store `available = false`. This is invariant 3's one exemption: a
   flag write before the cancel.
3. Call `take_supervisor`. The kick sender leaves with it.
4. Call `retire_supervisor`:
   - cancel the token (the generation moves here);
   - wait up to 5 s for the join; past that, `warn!` once and keep
     waiting;
   - inside the supervisor: the biased cancel arm aborts the child,
     awaits it, and breaks, which drops the kick inbox;
   - a child aborted inside its own `cancel_while_open` aborts the poll
     task through `AbortDetachedGuard`. A request that abort cuts off
     owes its reply (PR 0), and step 7's first stop discards it.
5. Store `available = false` again. This re-store undoes a publish
   that landed between step 2 and the cancel; PR 1 pins it.
6. Run `cancel_while_open`. Nothing can respawn the poll task any more.
7. Read the cell without taking it, run `Hooks::shutdown` on a clone,
   and close that clone. If the hook panics, the slot still names the
   conduit.
8. Take the slot, then clone and close whatever that cell holds at
   this moment (PR 6). A publish holds the slot guard across its cell
   write. It therefore either landed before this take, and is closed
   here, or it finds the slot empty and closes its own conduit (C3).
9. Store `reconnecting = false`, forgive the debt, and set
   `last_attempt = None`.

`release_any_held_conduit`, which runs before a cold start or a lazy
0→1, follows the same order: take the supervisor, retire it, run
`cancel_while_open`, reset `last_attempt`, then take the slot and close
what it holds.

`cancel_while_open` itself is unchanged (decision 6). It takes the
handle out of `while_open_state` and joins it inside
`AbortDetachedGuard`, which aborts the task if the caller is dropped
mid-join. The 5 s bound aborts a task that has not returned.

```rust
async fn retire_supervisor(&self, sup: Supervisor<C>, context: &'static str) {
    // Name `kicks`: `..` or `kicks: _` would leave the field dead in production.
    let Supervisor { mut task, cancel, kicks } = sup;
    cancel.cancel();
    drop(kicks);
    if tokio::time::timeout(WHILE_OPEN_TEARDOWN_TIMEOUT, &mut task).await.is_err() {
        warn!(context, "a reconnect attempt has not yielded since it was cancelled; waiting for it to release the port");
        let _ = task.await; // never abort: that would leave the child running with the port
    }
}
```

The join result is discarded on a single line. That keeps the path
deterministic to cover.

### The supervisor loop

Both selects, the wake select and the attempt join, are `biased;` with
cancel first.

On a kick, the supervisor:
1. skips the kick if its caller has gone (`reply.is_closed()`);
2. takes the transport out of service;
3. attempts without the cadence floor;
4. replies.

A kick that arrives during the cadence wait cuts the wait short.

Kicks are not coalesced. A kick queued while an attempt runs gets its
own attempt, after the running one has published. In between, the
transport is advertised on a conduit that is live and handshaken, which
is true at that moment. The queued kick then takes the transport out of
service before its attempt closes that conduit, exactly as a wire wake
right after a recovery would. A request that passed its gate before
that is held to the same one-check-before-the-lock bound as after any
wire wake. What W3 was, two writers interleaving the flags, cannot
happen: the flags have one writer, in program order.
`wait_out_cadence` reduces to a single `last_attempt` read. The dead
re-read after it is deleted.

The spawn guard checks `cancel.is_cancelled()`. Today an unbiased
select can start an attempt after cancel whenever `wait_out_cadence`
returns early. With the check, no attempt starts once cancel has fired.

```rust
tokio::select! {
    biased;
    () = cancel.cancelled() => break,
    Some(r) = kicks.recv() => reply = Some(r).filter(|r| !r.is_closed()),
    () = self.reconnect_signal.notified() => self.take_out_of_service(), // a request failed on the wire
    () = self.stop_owed.notified() => {}                                  // look again; never flips flags
    () = tokio::time::sleep(interval) => {}
}
```

The attempt's outcome reaches the kick's caller unchanged. A
`SessionError::Codec` from a failed handshake stays a `Codec` error, and
PR 8 pins this. A panicked attempt is reported as "reconnect attempt
panicked". An attempt that succeeds but whose `publish_recovery`
refuses (a stop came due) is reported as an error.

The failure path writes no flags. `reconnecting` stays set, and the
next tick retries.

### The `stop_owed` wake

The supervisor ignores a `stop_owed` wake only when `reconnecting` is
false. While a supervisor is registered, the only thing that clears
`reconnecting` is `publish_recovery`, and its re-read and undo leave the
flag clear only when no debt is outstanding. So any wake it ignores
belongs to a stop that has already been replayed. Wire wakes are left
untouched, which honours the investigation's warning not to gate them.

A wake it acts on still passes through `wait_out_cadence`, the same as
a wire wake. A stop refused just after a recovery is therefore replayed
when the interval since that recovery's attempt has run out, not at
once (decision 4). Skipping the floor here was rejected on review.

### `reconnect_now` as a test-only hook

**Cargo.** A new feature on the crate, switched on for its own tests by
a self dev-dependency:

```toml
[features]
default = []
# Test hooks outside the production API: today `SharedTransport::reconnect_now`.
# No service enables it. Unrelated to tokio's dev-dependency feature of the same name.
test-util = []

[dev-dependencies]
# Turns `test-util` on for this package's own test targets only (resolver 2).
rusty-photon-shared-transport = { path = ".", features = ["test-util"] }
```

With this, a plain `cargo test -p rusty-photon-shared-transport` builds
and runs every test with no flag: 140 at `d201506c`, matching the
attribute count per file. The shipped `.deb`, `.rpm`, tarball and MSI
artifacts are built by Cargo
(`cargo build --release -p …` in `build-packages.sh`,
`build-tarballs.sh` and `build-msi.ps1`). Resolver 2 never enables a
dev-dependency's features in a lib or bin build, so those artifacts
cannot contain the hook. The Cargo.toml comment says which mechanism
guards which artifact.

An invocation that also builds this package's tests, such as
`cargo test --workspace`, unifies `test-util` into that build. That is
harmless, because the feature only adds API.

Mechanisms that were measured and rejected:

- **`#[cfg(test)]`** cannot reach `tests/*.rs`. Those link the library
  as an ordinary rlib, and the gated method then fails with E0599 in
  42 places.
- **`[[test]] required-features`** silently skips all six integration
  targets (96 tests) under a plain `cargo test`.
- **A feature enabled only by CI flags** breaks every local
  `--all-targets` build.

**Bazel.** Following the testonly twin precedent of
`crates/rusty-photon-doctor-checks:rusty-photon-doctor-checks_mock`,
the package gains a twin library:

```starlark
rust_library(
    name = "rusty-photon-shared-transport_test_util",
    testonly = True,
    srcs = glob(["src/**/*.rs"]),
    crate_features = ["test-util"],
    crate_name = "rusty_photon_shared_transport",
    # aliases, edition, deps as the production library
)
```

- **The six integration tests** depend on the twin. Each also carries
  `crate_features = ["test-util"]`: Cargo compiles `tests/*.rs` with
  that cfg as well, so this keeps a test gated on the feature from
  existing under Cargo and silently vanishing under Bazel.
- **The unit test** compiles with `test-util` too. Every test target
  then builds exactly as it does under Cargo, which removes the
  divergence and the rule it would need.
- **The production configuration** is still compiled with `-Dwarnings`
  by `bazel build //...`, and exercised by every service's tests and
  BDD.
- **Services** keep depending on `:rusty-photon-shared-transport`.
  `testonly` makes it a Bazel analysis error for any production target
  to link the twin. The BUILD comment says this guarantee is
  Bazel-only.

**Coverage.** `bazel coverage` instruments both library variants and
merges the results per source line. Gated lines are therefore covered
by the integration tests, and compiled-out code is absent from the
report rather than counted as uncovered. This was verified locally.

**What is compiled out.** Only `reconnect_now`, with its two error
strings inlined in it. Before PR 8, `ManualReconnectGuard` and its
`Drop` are gated too: three attributes in all. PR 8 then deletes them.

PR 8 keeps the following in both builds:
- the kick channel;
- the `Supervisor.kicks` field;
- the receiver;
- the `Some(r) = kicks.recv()` arm.

In production nothing can send on the channel: the sender is a private
field, and its only user is compiled out. The arm is therefore inert,
and no source line exists in only one build. That avoids dead-code
warnings in the default-features clippy pass without any `#[expect]`.
It was measured clean, in a scratch crate, on stable and beta clippy
against the workspace lint table, in both the `--lib` and the
`--all-targets --all-features` shape.

The one spelling trap: `retire_supervisor` must bind `kicks` by name.
`..` or `kicks: _` does not count as a read, and the production build
would then warn `dead_code`.

A fallback was also verified clean: a one-impl `NextKick` trait plus
`PhantomData` fields, which compiles the channel out entirely. It costs
more machinery than it buys, so it is not the default.

**No `IN_ATTEMPT`.** The design panel scoped a task-local around the
attempt so that a `reconnect_now` called from a hook would return `Err`
instead of waiting on itself. Its only reader is now test-only, and it
never covered `while_open` bodies or tasks a hook spawns anyway. It is
dropped. The rustdoc says instead: never call `reconnect_now` from a
hook, a `while_open` body or a task they spawn. From a hook, such a
call waits until teardown. From a `while_open` body, the attempt it
starts joins that body, which is waiting on the attempt, so the body is
aborted at the 5 s bound. A verified test-util-only form exists if one
is wanted later.

**Not gated:**
- `set_reconnect_interval`: it is the public knob for the supervisor's
  own cadence.
- `MIN_RECONNECT_INTERVAL` and `DEFAULT_RECONNECT_INTERVAL`.
- `is_available` and `is_reconnecting`.

### Bug A: the poll answers to the debt

The two loose `AtomicU32` counters become one ledger type, which
`SharedTransport` holds behind an `Arc` and hands to every `WhileOpen`.
`WhileOpen::new` is `pub(crate)`, so the public API does not change.

The alternatives were weighed and set aside:
- **`Arc<SharedTransport>` in `WhileOpen`:** the poll task would then
  keep alive the transport that owns its `JoinHandle`.
- **`Weak<SharedTransport>`:** adds an upgrade-failure branch that needs
  a test of its own.

```rust
pub(crate) struct SafetyDebt { pub(crate) incurred: AtomicU32, pub(crate) paid: AtomicU32 }
impl SafetyDebt {
    pub(crate) fn outstanding(&self) -> bool {
        // `paid` first: see "In what order it loads" below.
        let paid = self.paid.load(SeqCst);
        self.incurred.load(SeqCst) != paid
    }
}

// WhileOpen::request_timed; `request` goes through it
if self.safety_debt.outstanding() {
    return Err(SessionError::Transport(TransportError::Reconnecting));
}
self.connection.request_timed(cmd).await
```

How the gate behaves:

- **What it reads.** The debt only, not `reconnecting` or `available`:
  those would refuse every poll until `publish_recovery`, which is
  broader than the invariant.
- **What it returns.** The same retryable error `Session` gives.
- **What it touches.** Nothing on the `Connection`, so a refusal raises
  no `reconnect_signal`, bumps no `wire_failures`, and cannot fail
  `commit_replacement`.
- **When it checks.** Once, before the command lock. That is the same
  bound `Session` has: a debt recorded after a poll has queued on the
  lock does not stop it.
- **In what order it loads.** `paid` first, then `incurred`. Both
  counters only grow, and `paid` never passes `incurred`. Under that
  order, `false` means no debt stood at the moment of the second load,
  so the check counts as made at that later moment. A concurrent incur
  reads as owed. A concurrent payment can at worst read as owed one
  check too long, which costs one skipped tick. The reverse order, which
  `safety_debt_outstanding` uses today, can return `false` when a
  cleanup incurs between the two loads. The check then counts as made at
  the first load. That still sits inside the one-check bound above, but
  it widens the window for no gain. `Session`'s gates read the same
  ledger, so they get the same order.

**The debt is recorded before the respawn.** In `attempt_reconnect`,
the `owed`/`replayed` decision and the debt increment move up to just
after the slot check, ahead of the respawn. The hook call stays where
it is, so the order becomes:

publish → record → respawn → replay → `commit_replacement`

Without the move there is a residual. In a no-client re-assert with
nothing owed beforehand, on a multi-thread runtime, a poll scheduled
between the spawn and the record passes the gate. It can then go out
between the replay's commands. Today that needs tokio's LIFO-slot
optimisation to be off, which is a scheduler detail, not a contract.

Nothing awaits between the publish and the new record point. A
cancellation at the respawn's lock leaves the debt standing, which is
today's "owed until proven landed" behaviour. The move was decided on
2026-10-03, so the guarantee is exact rather than documented as a
residual.

**Effect on the services.**
- *GTi:* a refused poll logs at `debug!` and skips the tick. Coordinate
  reads already project from the snapshot for at most four polls, and
  clients are refused by the same debt through `Session`.
- *The five services whose stop hook does nothing:* they can meet a
  refusal only inside a replay's await-free bookkeeping on a
  multi-thread runtime.
- *pa-falcon-rotator:* has no `while_open`.
- No service code changes.

### Bug B: the opener closes what it abandons

A private helper replaces the three
`(self.hooks.handshake)(&x).await.map_err(SessionError::Codec)?`
sites: in `start`, in `attempt_reconnect` and in the lazy 0→1
`acquire`.

```rust
async fn handshake_or_close(&self, connection: &Connection<C>) -> Result<(), SessionError<C::Error>> {
    if let Err(e) = (self.hooks.handshake)(connection).await {
        connection.close().await;
        return Err(SessionError::Codec(e));
    }
    Ok(())
}
```

Its rustdoc is honest that this changes nothing today, and says why:

- `HandshakeFn` only borrows the conduit. A hook that tries to keep it
  fails to compile (E0521).
- `Connection` has no `Drop` impl. `close()` takes and drops the same
  boxed stream under the command lock.
- On Windows, `CloseHandle` is deferred to the reactor whether the
  stream is closed or dropped, and `open_serial_port`'s retry ladder
  rides it out either way.

The close exists so that every open path that abandons a conduit
closes it.

At a cancellation point the conduit is dropped, not closed. Closing it
there is *unnecessary*, not impossible: a guard could spawn a detached
close, as `Session::drop` does. Nothing else holds the conduit yet, so
the drop releases it.

The same PR corrects two pieces of text:
- `connection.rs`'s `close()` rustdoc, which implies a close releases a
  Windows handle sooner than a drop does;
- the attempt-path comment saying a drop "on Windows is not a release".

**Commit 2.** A `while_open` constructor that hands its
`WhileOpen` elsewhere and then panics leaks the port on a cold start or
a lazy 0→1. Those two sites do not catch the panic. The reconnect path
already does. Reproduced: the retry start is refused with "Access is
denied".

Commit 2 generalises the reconnect path's `catch_unwind` + close +
`Io("while_open constructor panicked {context}")` into a private
`build_while_open` used at all three sites.

This changes public behaviour: `start()` and `acquire()` return `Err`
on a constructor panic instead of unwinding. `ColdStartGuard` and
`RollbackGuard` fire exactly as before, and the debt was already paid
by the assertion that precedes the constructor. A `Hooks::while_open`
paragraph states the contract: give the `WhileOpen` to the returned
future and keep no other copy.

No production constructor can panic or keep its context. The catch is
therefore defence in depth, and makes all three open paths behave
alike. It also leaves `WhileOpen` constructed in one place,
`build_while_open`, which is where PR 3 hands it the debt ledger.

## PR plan

**Rules for every PR:**

- **Self-contained.** Each PR ships its own docs (rule 2).
- **No issue numbers in code comments.**
- **Logging.** `debug!()` throughout. `warn!` appears only on the
  teardown wait and the constructor-panic branch.
- **No production `#[expect]`/`#[allow]`.** If nursery
  `significant_drop_tightening` fires on a guard section, restructure
  with explicit `drop()`s, as the existing publish code does.
- **No coverage exclusions.** Every new production line is reached by a
  deterministic test (`uncovered-diff-lines` is required). That is why
  race-only checks fold into existing lines and unreachable branches
  are deleted rather than left uncovered.
- **Test style** follows [testing.md](../../skills/testing.md):
  - `current_thread` with FIFO wakeups for every gated interleaving;
  - `multi_thread` only where a stall that never yields is the subject;
  - never assert on captured tracing (§6.8);
  - assert a negative over a watch window, never by sampling after a
    nap (§6.9);
  - `unwrap()`/`unwrap_err()`;
  - one behaviour per test.
- **Mutation check.** Every new or changed test gets one, recorded in
  the PR description: reintroduce the defect and watch the test go red.
- **Test placement.** New tests go into the existing `reconnect`,
  `lifecycle`, `while_open` and `rollback` targets or the in-crate
  module, so `BUILD.bazel` changes only in PR 7. Size "small" covers
  the one ~5.5 s test.
- **Gate before pushing.**
  - `bazel build //... && bazel test //...` (`--local_test_jobs=8`
    locally);
  - `cargo fmt`;
  - `cargo clippy --all-targets --all-features -- -D warnings`;
  - `cargo clippy --workspace --lib --bins -- -D warnings`.
- **Private target directory.** Use a private `CARGO_TARGET_DIR` per
  worktree when running test binaries. Cargo hashes a path package
  relative to its workspace root, so two worktrees of this repo
  overwrite each other's test binaries in a shared target directory.
- **Inverted repros.** The investigation's scratch repros (not
  committed) assert the buggy outcomes. The inverted tests below are
  their committed form, and each must fail against the code before its
  PR:
  - W5 → PR 4;
  - W4 after-drain → PR 5;
  - W1, W2, W3 and Lazy → PR 8.

### PR 0 — `fix(shared-transport): a request abandoned after its send owes its reply, and the next request discards it`

**Scope** (decision 7).
- `Connection`'s mutex guards the transport and an owed-frame count
  together, so the count lives and dies with the conduit. A fresh
  conduit is a fresh `Connection`, and starts at zero.
- In `request_timed`, a guard owns the lock for the whole exchange. It
  is armed when the send starts. If the future is dropped while the
  guard is armed, its `Drop` adds one to the count before the lock is
  released, so the next holder sees it.
- After the send, the exchange reads and discards owed frames,
  decrementing the count as each one arrives. A drop part-way through
  therefore leaves the frames not yet read still owed, plus one for
  itself. Discarded frames are not decoded and do not count against
  `max_skip`.
- The guard is disarmed when the exchange returns:
  - with its reply;
  - with a codec error or `SkipExhausted`, which have consumed the
    frame they judged;
  - with a send or receive error. That one already raises a reconnect,
    and decision 7 has a receive timeout owe nothing.

  A receive error while discarding leaves the frames not yet read still
  owed, and adds nothing for this exchange.
- A request dropped while it waits for the lock owes nothing: it has
  not sent.
- Each discarded frame gets the existing `wire recv` trace event,
  marked as discarded, and the exchange logs one `debug!` with the
  number it discarded.

**Tests.** They need a device double that behaves like a line: it
queues each answer when the frame is written, and a read parks
*before* it pops, so a dropped read leaves its answer for the next
reader. It offers `park_next_read`, `wait_inside_read` and
`release_read`, and `park_next_send` with its own wait and release. A
parked send has already handed its frame to the device, so its answer
is queued. Neither `EchoTransport` nor `ScriptedReplies` can do
this: the first keeps a single slot that each send overwrites, and the
second ignores sends.
- `Connection::new` is crate-private, so the `connection.rs` tests get
  the double in-module, next to `ScriptedReplies`.
- `tests/common` gets it as `LineTransport` and `LineFactory`, for the
  `while_open` test below and for PR 8's.
- The torn-frame test instead runs a real `SerialFrameTransport` over a
  `tokio::io::duplex` pair, so it exercises the `BufReader`.
- `connection.rs::tests::a_request_dropped_while_awaiting_its_reply_leaves_it_for_the_next_request_to_discard`.
  A is dropped while parked in its read. B returns B's answer.

  Mutation: never arm the guard → B returns A's answer.
- `…::a_request_dropped_while_its_send_is_pending_owes_its_reply`. A is
  dropped while parked inside `send_frame`, after the device has taken
  its frame. B discards A's answer and returns its own.

  Mutation: arm the guard only after `send_frame` returns → B returns
  A's answer.
- `…::a_request_dropped_part_way_through_a_frame_discards_the_rest_of_it`
  (duplex). The device writes half of A's answer, A is dropped, and the
  device writes the rest and then B's answer. B returns B's answer.
- `…::a_request_dropped_while_skipping_still_owes_its_own_reply`
  (`max_skip` 1). A is dropped while reading the frame after a skipped
  one. B returns its own answer.
- `…::a_request_dropped_while_discarding_keeps_the_count`. With one
  frame owed, B is dropped while discarding it. C discards two frames
  and returns its own answer.
- `…::a_request_dropped_before_it_takes_the_lock_owes_nothing`. B is
  dropped while A holds the lock. C returns its own answer.

  Mutation: arm the guard before the lock → C times out.
- `…::a_receive_timeout_owes_nothing`. The device never answers A, so
  A times out. B returns its own answer.

  Mutation: owe a frame on a timeout → B discards its own answer and
  times out.
- `…::an_owed_reply_that_never_comes_makes_the_next_request_time_out_and_signal`.
  A is dropped after its send, and the device never answers it. B
  discards its own answer, times out, and raises the reconnect signal.
  This pins "counting too many heals".
- `…::discarded_frames_do_not_count_against_the_skip_budget`
  (`max_skip` 1, one frame owed, then one unmatched frame). The request
  returns its own answer.
- `…::a_reordered_owed_reply_costs_one_misattributed_reply`. A is
  dropped after its send, and the double holds A's answer back until
  B's is queued, as a UDP link may reorder them. B returns A's answer,
  and C then returns its own. This pins the bound decision 7 states for
  an unordered conduit: one misattributed reply, then back in step.

  Mutation: never arm the guard → C returns A's answer.
- `tests/while_open.rs::a_poll_aborted_mid_request_leaves_the_shutdown_hook_its_own_reply`
  (`start_paused`). This is R1 (§Deliberately left), today's route
  through the 5 s bound.
  The poll is parked in its read, which has no timeout of its own, when
  `shutdown` runs. So `cancel_while_open`'s join reaches the bound and
  aborts it mid-request. The shutdown hook sends
  `BYE` and records what it reads back. It asserts `BYE`.

  Mutation: never arm the guard → the hook reads `POLL`.

**Docs.**
- `request_timed`'s rustdoc gains a cancel-safety section: what is
  owed and when, and that a receive timeout owes nothing.
- The `WireTiming` doc: a frame left over from an abandoned request is
  now discarded; one that arrives after its request timed out can still
  answer the next request.
- The `Codec` trait doc: owed frames are discarded before `matches` and
  `max_skip` see anything.
- The crate's module rustdoc gains invariant 7.
- [star-adventurer-gti.md](../../services/star-adventurer-gti.md), where
  it discusses a stale ack: a reply left by an abandoned request is
  discarded. A duplicated datagram, or a reply that arrives after its
  request timed out, still is not. An owed reply that UDP delivers
  after the next request's own is taken by that one request.
- `watcher_poll_with_retry`'s doc in `star-adventurer-gti` claims the
  backoff lets the read "flush whatever junk". No code reads during the
  sleep, so the comment is corrected.

**Hardware.** None. With nothing owed, an exchange sends and reads
exactly as it does today, and only an abandoned request makes anything
owed. The tests drive every owed path.

### PR 1 — `test(shared-transport): pin the post-join re-store and the reconnect guarantees that already hold`

**Scope.** Tests only, and green on `d201506c`. This PR is the net
under PR 4's edits to the same join. `ScriptedFactory` and
`ScriptedTransport` move into `tests/common`. One test lives in
`star-adventurer-gti`, because what it pins is the GTi decoder's
behaviour, not the transport's.

**Tests.**
- `shared.rs::tests::a_recovery_published_inside_the_shutdown_join_is_withdrawn_after_it`
  (W2s). The test holds `supervisor_state`, so `shutdown` blocks in
  `take_supervisor` after its first store; the marker is
  `acquire_lock.try_lock().is_err()`. The supervisor's parked
  handshake is released, and `is_available()` reading true is the
  precondition. After the guard is dropped:
  - `!is_available()` and `!is_reconnecting()`;
  - `start()` cold-starts and serves.

  `!is_available()` alone would be vacuous as a marker, because
  `take_out_of_service` has already cleared it.

  Mutation: delete the post-join re-store → red.
- `a_late_failure_on_the_dying_conduit_does_not_cycle_its_replacement`
  (W4 as filed).

  Mutation: delete the post-handshake drain → a third open.
- `a_stop_owed_before_the_drain_is_paid_without_a_cycle` (its control).

  Same mutation. Record whether it goes red too.
- `a_last_disconnect_inside_the_attempt_join_is_replayed_after_a_panic`,
  `…_after_a_refusal` and `…_after_a_halt_on_a_closed_conduit` (W6,
  from the scratch join-gap tests, rewritten to assert on state only).
  Each asserts:
  - out of service right after;
  - a racing session is refused with `Reconnecting`;
  - the replay lands and the transport recovers within ten intervals.

  Mutation: delete `publish_recovery`'s re-read and undo → red.
- `star-adventurer-gti` `manager.rs::tests::a_stale_poll_reply_ahead_of_the_safety_stop_reads_as_not_asserted`.
  It pins what §Deliberately left says about a stale poll reply ahead
  of the safety stop. PR 0 counts what every abandoned request owes,
  but not a stray it cannot count or place: a late reply after a
  receive timeout, a duplicated UDP datagram, or an owed UDP reply that
  arrives after the stop's own. This pin covers those.
  A transport that wraps `CapturingMockFactory`'s hands
  back one stale `:j` reply before the mock's own replies. A
  `SharedTransport` on it has an `on_last_disconnect` hook that calls
  `safety_stop` and records the verdict, so the test reads the verdict
  rather than a log line. Closing the only session runs it. The test
  asserts:
  - all three stop frames (`:L1`, `:L2`, `:K1`) were written;
  - the verdict is `NotAsserted`.

  Mutation: let `Response::decode` accept a payload on an ack → the
  verdict reads `Asserted`, the false assertion this test exists to
  rule out.

**Docs.** None.

**Hardware.** None.

### PR 2 — `fix(shared-transport): an open that fails before the publish closes the conduit it opened` (bug B)

**Commit 1: `handshake_or_close` at the three handshake sites.**

Tests:
- `reconnect.rs::a_reconnect_whose_handshake_fails_releases_the_port`:
  the first test of a handshake error on the attempt path.
- `lifecycle.rs::a_cold_start_whose_handshake_fails_releases_the_port`:
  the first test of one in `start()`.

Both pass on `main` and after the change. They are regression pins,
and the PR says so.

Mutations:
- Deleting the close leaves the suite green. That is the expected
  signature of a close that is identical to the drop.
- Inserting `std::mem::forget(Arc::clone(&new_conn))` before the
  attempt's handshake turns the first test red without the helper and
  green with it. That shows what the close buys against a future
  refactor that shares the conduit early.

`handshake_failing_on(n)` goes into `tests/common` and uses
`saturating_add`, because `common/mod.rs` does not allow arithmetic
side effects itself.

**Commit 2: `build_while_open` at all three constructor sites.**

Tests:
- `lifecycle.rs::a_cold_start_whose_constructor_panics_releases_a_conduit_it_passed_on`
  and `rollback.rs::a_lazy_open_whose_constructor_panics_releases_a_conduit_it_passed_on`:
  both red on `main`.
- `reconnect.rs::a_reconnect_whose_constructor_panics_releases_a_conduit_it_passed_on`:
  green on `main`, but the first test that fails if that existing close
  is lost.
- These use a helper,
  `ctor_keeping_its_context_and_panicking_on(n, stash)`, in common.
- `while_open_constructor_panic_rolls_back_state_fully` is updated to
  the new contract: `unwrap_err` containing "while_open constructor
  panicked", with `dropped_count` 1 then 2 and `opens` 1 then 2.

Mutation: delete the close in `build_while_open` → the three new tests
go red.

**Docs.**
- `session.rs` `Hooks::handshake`: "transport dropped" becomes
  "transport closed".
- The `Hooks::while_open` constructor contract.
- `start()` and `acquire()` `# Errors` (commit 2).
- The `connection.rs` `close()` rustdoc and the attempt-path comment,
  as above.

**Hardware.** None. Commit 1 is behaviour-identical, and commit 2's
branch is unreachable from production hooks.

### PR 3 — `fix(shared-transport): the poll task answers to the safety debt too` (bug A)

**Scope.**
- The `SafetyDebt` ledger, with every old-name site renamed (19 sites,
  including the five in-crate test lines and anything PR 1 added).
  `outstanding` loads `paid` before `incurred` (see §Bug A).
- The gate on `WhileOpen::request_timed`.
- `Arc::clone(&self.safety_debt)` passed at the one `WhileOpen::new`
  site, inside PR 2's `build_while_open`.
- The debt record moved ahead of the respawn in `attempt_reconnect`.

It comes right after PR 2, ahead of the lifecycle fixes, for two
reasons. It is production-reachable on GTi, and it lands the rename
before PRs 4, 5, 6 and 8 edit the lines it touches. No ordering change
elsewhere, no dependency and no BUILD change.

**Tests.**
- `shared.rs::tests::a_poll_request_is_refused_while_a_stop_is_owed_whatever_the_flags_say`.
  The debt is written by hand while the flags read healthy, which pins
  the GTi failed-1→0 path without depending on scheduling. Steps: the
  poll returns `Ok`; incur the debt; the poll gets `Reconnecting`; pay
  it; the poll returns `Ok` again.
- `while_open.rs::the_poll_is_refused_while_a_reconnect_is_still_replaying_the_stop`.
  This one is driven by the supervisor, not `reconnect_now`, so it
  needs no `test-util` wiring and survives PRs 4–8 unchanged. Steps:
  - set up `SafetyStopHooks::parking_after(1)` with a `PokedPoll`
    `while_open`, and call `start()`;
  - poke: `Ok`;
  - arm `fail_recvs` and poke, so the poll's own request fails on the
    wire and wakes the supervisor;
  - `wait_inside_hook()`, then assert `reached_the_wire == 2`;
  - poke: `Reconnecting`;
  - release the hook and wait for `is_available()`;
  - poke: `Ok`.

  This test failed on the unfixed source and passed 200/200 with the
  fix.
- `PokedPoll::request_now` is bounded by `tokio::time::timeout` with an
  explicit message. Unbounded, a missing poll task would hang the test
  rather than fail it.

**Mutations.**
- Neuter the gate → both tests red.
- Revert the record move → expected to be unobservable on
  `current_thread`. The spawned poll cannot run before the attempt's
  first yield, and that yield comes after the record either way. The PR
  says so rather than shipping a test that cannot discriminate.
- Swap the load order back → also unobservable. It takes an incur
  landing between two adjacent atomic loads, which no test here can
  schedule. The ledger's rustdoc carries the argument instead.

**Docs.**
- `session.rs`:
  - module doc;
  - `WhileOpen` struct doc;
  - `WhileOpen::request` doc. Its `# Errors` says `Reconnecting` is
    retryable and a poll loop should treat it as a skipped tick. It
    also states the one-check-before-the-lock bound.
  - `Hooks::while_open`: "its requests answer to the safety debt".
- `shared.rs`: the `SafetyDebt`, field and `safety_debt_outstanding`
  docs. The `SafetyDebt` doc includes why `outstanding` loads `paid`
  first.
- [star-adventurer-gti.md](../../services/star-adventurer-gti.md),
  §Safety stop across a reconnect: a paragraph on the poll being held
  to the same debt. It covers:
  - when the poll is refused;
  - that it only inquires, so the gate keeps the rule true for every
    handle rather than being what keeps the mount safe today;
  - that the snapshot's age cap bounds staleness;
  - that the replay itself is never held back.
- The `Connected = false` row now says the poll's "requests are
  refused while a stop that did not land is still owed".

**Hardware.** None. The change adds no wire traffic and does not touch
the connect path. GTi lib tests and BDD run with `--features mock`.

### PR 4 — `fix(shared-transport): a teardown waits for the reconnect attempt instead of abandoning it` (W5)

**Scope.**
- Introduce `Supervisor { task, cancel }` and `retire_supervisor`:
  cancel, then a 5 s join, then `warn!`, then an unbounded wait, then
  a `debug!` with the total; never abort. It replaces the three copies
  of the join in `shutdown`, `release_any_held_conduit` and
  `spawn_supervisor`.
- Add three `debug!` events, which §Validation needs to show that a
  teardown landed inside an attempt:
  - `attempt_reconnect` starting;
  - `shutdown` starting;
  - an attempt that the cancel ended.
- Delete `spawn_supervisor`'s unreachable replace branch. Every caller
  has already retired the previous supervisor under `acquire_lock`,
  so a `debug_assert!` takes the branch's place. Register under the
  lock, then spawn.
- Make the wake select and the attempt-join select `biased;`
  cancel-first.
- Fold `cancel.is_cancelled()` into the spawn guard.
- Delete the stale comment and the broken `warn!` literal, which
  carries a lost line continuation.

**Tests.**
- `lifecycle.rs::shutdown_waits_for_an_attempt_that_has_not_yielded`
  (`multi_thread`, about 5.5 s). `WedgedSecondOpen` moves to common: it
  takes the port and then blocks without yielding. A wire failure on a
  held session drives the supervisor into it. Assertions:
  - `shutdown` took at least 5 s, so the timeout branch and its `warn!`
    ran;
  - the wedged open returned before `shutdown` did;
  - the port is free when `shutdown` returns;
  - the handshake count is stable over a 200 ms window after return;
  - a fresh `SharedTransport` on the same factory starts with
    `refusals == 0`.

  Mutation: restore the parent abort → the fresh start is refused.
- These existing tests stay green unmodified, and their abort-on-cancel
  branch becomes deterministic under cancel-first:
  - `shutdown_is_not_undone_by_the_attempt_it_interrupts`
  - `shutdown_does_not_leave_a_reconnect_attempt_running`
  - `a_cold_start_stops_the_supervisor_before_opening`
  - `shutdown_cancels_supervisor_cleanly`
  - `a_hook_that_panics_mid_attempt_does_not_take_the_supervisor_with_it`

**Docs.**
- `shutdown` rustdoc: it may wait past 5 s, and why.
- The `release_any_held_conduit` comment.
- [star-adventurer-gti.md](../../services/star-adventurer-gti.md):
  - The shutdown sequence diagram is wrong today and gets fixed: the
    supervisor and the poll task are cancelled *before* the hook.
  - §Safety stop: a reload during a reconnect waits for the attempt.
- [dsd-fp2.md](../../services/dsd-fp2.md) §In-process reload: the "Await
  the server's own teardown" bullet.
- [service-lifecycle.md](../../skills/service-lifecycle.md) §Plugging
  into a server says "The five shared-transport services"; there are
  seven now. Its other two mentions of five stay. They describe the
  migration that closed #294, when there were five.

**Hardware.** Required before merge (see Validation).

### PR 5 — `fix(shared-transport): a missed last-client stop wakes the supervisor on its own signal` (W4b)

**Scope.**
- Add `stop_owed: Notify`.
- In `run_cleanup_locked`'s ServiceLifetime "stop not landed" branch,
  raise it in place of `reconnect_signal.notify_one()`.
- `UnlandedStateGuard` gets a reference to it and raises it in
  ServiceLifetime, after its increment and its flag stores.
- The supervisor gets a `stop_owed` arm that writes no flags.
- Update the comments that say `reconnect_signal` has other raisers.

**Tests.**
- `reconnect.rs::a_stop_paid_by_the_attempt_in_flight_does_not_cycle_its_replacement`.
  This is the scratch after-drain test, inverted. It runs on
  `multi_thread`, with the call-2 `while_open` constructor gated by
  `block_in_place`. Over a ten-interval window it asserts:
  - `opens == 2`;
  - `available && !reconnecting`;
  - halts `[Ok, Err(closed), Ok]`.

  Mutation: raise `reconnect_signal` again → opens reaches 3.
- `a_refused_disconnect_stop_is_replayed_without_waiting_for_the_tick`
  (interval 3600 s, no attempt yet): the transport is available again
  within 2 s.

  Mutation: delete the `stop_owed` arm → timeout.
- `a_stop_refused_just_after_a_recovery_waits_out_only_the_rest_of_the_interval`
  (interval 1 s, real clock). A reconnect recovers first and stamps
  `last_attempt`; a client then connects, and its disconnect stop is
  refused. It asserts:
  - no replay before the interval since that attempt has run out;
  - the transport is available again within that interval plus a
    margin.

  This pins the floor that decision 4 keeps.
- `a_panicked_disconnect_hook_is_replayed_without_waiting_for_the_tick`
  (`last_disconnect_panicking_on(2)`): covers the guard's raise.

  Mutation: drop the guard's notify → timeout.
- `a_refused_disconnect_stop_is_recovered_by_the_supervisor` and
  `a_disconnect_stop_the_device_refuses_takes_the_transport_out_of_service`
  stay green; their comments now say the cleanup wakes `stop_owed`.

**Docs.**
[star-adventurer-gti.md](../../services/star-adventurer-gti.md) §Safety
stop across a reconnect: rewrite the paragraph on how the supervisor
comes back round.
- Every debt a last-client cleanup records wakes the supervisor to look
  again. That covers a stop that failed on the wire, one that was
  refused, one that landed on a closed conduit, and a panicked hook.
- A panicked hook no longer waits for the next link failure.
- A wake whose stop was already replayed is ignored.

**Hardware.** None.

**Interim caveat** (stated in the PR description, test-only): until
PR 8, a `reconnect_now` publish made off the supervisor can make it
spend the permit between its stores and its undo. The replay then waits
one interval, and the Session debt gate holds throughout.

### PR 6 — `fix(shared-transport): shutdown closes what the slot holds when it empties it` (W1-LEAK)

**Scope.** About ten lines in `shutdown`. It keeps the read-without-take
around the hook, so a conduit left behind by a panicking hook stays
findable. It then takes the slot and clones and closes what that cell
holds at that moment.

The PR also fixes the publish-guard comment, which says `shutdown`
"takes the slot" when it does not.

This is kept after PR 8 as defence in depth. A held port is the one
harm that outlives the instance and hurts its successor.

**Tests.**
- `shared.rs::tests::shutdown_closes_the_conduit_the_cell_holds_when_it_empties_the_slot`.
  The shutdown hook parks. While it is parked, the test writes a
  replacement `Connection` into the cell under the slot guard, then
  releases the hook. The replacement's next request fails with
  "transport closed". This is white-box, so it survives PR 8, unlike a
  version driven by `reconnect_now`.

  Mutation: close only the clone → the request succeeds.
- `a_panicking_shutdown_hook_does_not_keep_the_port_from_the_next_start`
  stays green unmodified (findability).

**Docs.** `shutdown` rustdoc.

**Hardware.** None.

### PR 7 — `build(shared-transport): reconnect_now is a test-util hook`

**Scope.**
- `Cargo.toml`: the feature and the self dev-dependency, with comments.
- `Cargo.lock`: gains one line, the self-edge.
- `MODULE.bazel.lock`: refreshed with `scripts/repin-bazel-lock.sh`
  ([#1391](https://github.com/rusty-photon/rusty-photon/pull/1391)). The
  diff is two hash lines, and the `cr` hub is unchanged.
- `BUILD.bazel`: the `_test_util` twin, and `test-util` on every test
  target.
- `shared.rs`: three `#[cfg(feature = "test-util")]` attributes, on
  `reconnect_now`, `ManualReconnectGuard` and its `Drop`.

`supervisor_live` stays ungated: it is write-only but raises no
warning, and PR 8 deletes it.

Any edit to a member `Cargo.toml` stales `MODULE.bazel.lock`, a comment
included, because crate_universe hashes every member manifest. Since
#1391 the pre-commit hook refuses the commit if the repin is missing.
Also run `bazel build` with `-Dwarnings` on the package before
pushing.

**Tests.** No behaviour changes, so there are no new tests. The
evidence goes in the PR description:
- Negative controls, made as temporary edits and reverted:
  - a non-testonly library depending on the twin is a Bazel analysis
    error;
  - the `reconnect` target pointed at the production library fails
    with E0599.
- `cargo build -v -p star-adventurer-gti` compiles the crate with
  `feature="default"` only.
- `cargo test -p rusty-photon-shared-transport` runs all 140 tests.
- `cargo hack --feature-powerset clippy --all-targets` is clean.

**Docs.**
- [workspace.md](../../workspace.md), shared-transport row: one clause
  saying `reconnect_now` is a `test-util` hook absent from production
  builds.
- [testing.md](../../skills/testing.md) §6: a short convention note:
  - a test-only API is a `test-util` feature;
  - it reaches the crate's own tests through a self dev-dependency;
  - Bazel builds a `testonly` `_test_util` twin, and every test target
    is compiled with the feature;
  - why not `required-features`: it skips tests silently.
- `reconnect_now` rustdoc: test-util only, and never to be called from
  a hook, a `while_open` body or a task they spawn.
- `error.rs:55` and the field docs that imply a production caller are
  reworded. That includes the `reconnect_signal` doc, which today says
  `reconnect_now` fires it, and it does not.

**Hardware.** None. The production configuration only loses code.

### PR 8 — `refactor(shared-transport): one owner for every reconnect attempt` (W1, W2, W3, Lazy)

**Scope.**
- Add `kicks` to `Supervisor`.
- Rewrite the gated `reconnect_now(&self)` as a request to the
  supervisor. It refuses, touching nothing, when there is no
  supervisor: in LazyAcquire, before `start`, after `shutdown`. It
  sends under the lock and keeps no sender clone. Its error when the
  supervisor stops is distinct.
- The supervisor's kick handling, including skipping closed replies.
- `wait_out_cadence` reduced to one read and made preemptible.
- `attempt_reconnect(lifecycle)` with C1, C2 and C3.
- `cancel_while_open` is unchanged (decision 6).
- Deletions:
  - `ManualReconnectGuard`, `supervisor_live` and the dead re-read;
  - `reconnect_now`'s own flag stores, publish and clear;
  - `Session`'s LazyAcquire "reopens once every session is released"
    arm, and `is_service_lifetime`, both unreachable by construction.
- The `attempt_reconnect_lock` doc is narrowed.

No `!` on the title: after PR 7 the only API whose behaviour changes is
behind `test-util`. The LazyAcquire behaviour removed here was
reachable only through `reconnect_now`.

**New tests.**
- `a_shutdown_during_a_manual_reconnect_ends_it_before_the_hook`
  (scratch W1, inverted). With the hook parked:
  - `reconnect_now` has returned the "supervisor stopped" error;
  - one poll task was ever started;
  - the port is not held.

  After release:
  - `shutdown` returns `Ok`;
  - no polls over a 200 ms window;
  - `acquire` says "shut down";
  - a fresh instance starts with no refusals.
- `a_manual_recovery_cannot_outlive_the_shutdown_it_raced` (scratch W2,
  inverted):
  - `Err`, and both flags false;
  - `acquire` says "shut down";
  - `start()` cold-starts and serves;
  - every handed-out conduit was dropped.
- `a_queued_reconnect_does_not_advertise_the_conduit_it_is_about_to_close`
  (W3b, inverted): while the second kick's handshake is parked,
  `is_reconnecting && !is_available`, and a session gets
  `Reconnecting`.
- `a_queued_reconnect_that_fails_quietly_leaves_the_retry_promised`:
  the second open is refused, `is_reconnecting` stays set, and the
  tick recovers.
- `a_manual_reconnect_reports_the_attempts_own_error`: a failed
  handshake comes back as `SessionError::Codec`, so PR 2's first test
  keeps meaning what it says.
- Refusal tests:
  - `a_manual_reconnect_before_start_is_refused` (opens 0, flags
    untouched);
  - `a_lazy_transport_refuses_a_manual_reconnect_and_keeps_serving`
    (the held session pings, opens 1, no stray poll task);
  - `a_manual_reconnect_after_an_abandoned_shutdown_is_refused` (no
    handshake; `acquire` says "shut down").
- Commit points:
  - **C1** `a_reconnect_cancelled_before_its_close_leaves_the_conduit_to_the_teardown`:
    opens stays 1, and the shutdown hook's request reached the wire.
    Mutation: drop C1 → opens 2.
  - **C2**: extend PR 4's W5 test with `handshakes == 1`. Mutation:
    drop C2 → 2.
  - **C3** `a_cancelled_lifecycle_never_installs_the_attempts_conduit`:
    `while_open` spawns 1, `Err`, port not held. Mutation: drop the
    token term → spawns 2.
- Kicks:
  - `a_kick_whose_caller_left_before_it_started_is_skipped`: opens
    grows by exactly 1 over three intervals.
  - `a_kick_during_the_cadence_wait_is_the_only_attempt`.
  - `a_kicked_attempt_that_panics_is_reported_and_retried`.
- Group mutation: re-inline the attempt into `reconnect_now` → the W1,
  W2 and W3 tests go red.
- The cancel arm's route to a stranded reply, which PR 0 counts
  (decision 6):
  `reconnect.rs::a_teardown_that_interrupts_an_attempt_leaves_the_hook_its_own_reply`.
  It uses PR 0's `LineTransport` and a shutdown hook that sends `BYE`
  and records what it reads back. PR 3's `PokedPoll` gains `poke()`,
  which does not wait. Steps:
  1. `start()`, `park_next_read`, `poke`, `wait_inside_read`.
  2. Spawn `reconnect_now()`. Its attempt's `cancel_while_open` joins
     the parked poll.
  3. Spawn `shutdown()` and await the kick: `Err` with "supervisor
     stopped". This is the precondition: the cancel arm aborted the
     child inside its `cancel_while_open`, and `AbortDetachedGuard`
     aborted the poll mid-request.
  4. `shutdown` returns `Ok`.

  Asserts: the hook read `Ok(BYE)`.

  Mutation: disarm PR 0's guard → the hook reads `POLL`.

**Rewritten.**
- `reconnect_now_before_start_returns_slot_empty_error` is replaced by
  the refusal test above.
- `a_recovery_during_the_cadence_wait_cancels_the_attempt_it_was_waiting_for`
  becomes the cadence-wait kick test.
- `a_manual_reconnect_that_unwinds_with_no_supervisor_clears_the_retry`
  becomes the abandoned-shutdown refusal.
- `an_abandoned_teardown_does_not_leave_the_poll_task_running` is driven
  through `AbortDetachedGuard` with `timeout(50 ms, st.shutdown())` and
  a stubborn poll task.
- `a_reconnect_that_loses_its_slot_closes_the_replacement` is renamed
  and recommented: the attempt is now dropped inside `open`, with the
  distinct "supervisor stopped" text.
- `a_reconnect_after_shutdown_does_not_wedge_the_transport`: comment
  only.

**Deleted (subject gone).**
- `a_lazy_acquire_after_a_failed_reconnect_is_usable`
- `a_failed_lazy_reconnect_does_not_strand_a_live_session`
- `a_manual_reconnect_that_panics_does_not_strand_a_lazy_session`
- `a_lazy_reconnect_pays_an_owed_stop_before_publishing`

The LazyAcquire debt stays pinned by
`a_lazy_open_discharges_a_stop_the_previous_conduit_could_not_carry` and
`a_lazy_open_after_a_panicking_cleanup_releases_the_conduit_it_left_open`.

About 50 other `reconnect_now` call sites in ServiceLifetime tests keep
passing, because the kick returns after `publish_recovery`. Only their
comments change.

**Docs.**
- A new module-doc section, "Reconnect ownership and the lifecycle
  token". It covers:
  - the invariants above;
  - the two conditions that would bring a counter back;
  - the uniform "the opener closes what it abandons" sentence.
- Rustdoc for `reconnect_now`, `attempt_reconnect`,
  `commit_replacement` and `acquire`; the field docs; and the
  `PostPublishGuard` and `AbortDetachedGuard` examples.
- `cancel_while_open`'s rustdoc: it has five callers, not "the three".
- `session.rs` around the request path, and the `Hooks` doc. Hooks and
  `while_open` bodies must not call `acquire`, `start` or `shutdown` on
  their own transport.
- [workspace.md](../../workspace.md), shared-transport row: one clause on
  single ownership.

**Hardware.** Required before merge (see Validation). This PR changes
the supervisor loop that all seven services run.

## Validation

PRs 4 and 8 change what every service's reload does while a reconnect
is in flight. The standing rule is never to ship a connect-path change
on mock evidence. Before each of them merges, the legs below must
show that the teardown really landed inside an attempt. A reload that
happens to fall between attempts proves nothing about the change.

**What makes a run count.** Run with debug logging for the service and
the transport crate. A run counts only if the old instance's log shows
three things, in this order:
1. the attempt's start;
2. the shutdown's start;
3. the attempt's end: its outcome, or its being ended by the cancel.

Today the transport logs only an attempt's outcome ("transport
reconnected successfully", "transport reconnect attempt failed; will
retry"). PR 4 adds three `debug!` events: one where `attempt_reconnect`
starts, one where `shutdown` starts, and one for an attempt the cancel
ended.

A run without that sequence is discarded, not passed. For each counted
run, record:
- the phase it landed in: opening, handshaking or replaying, read from
  the lines between the attempt's start and the shutdown's;
- the log excerpt.

**How to land the teardown inside an attempt.** Timing it by hand does
not work: an attempt lasts well under a second, and one starts every
5 s. Drive the reload from the log instead. A watcher tails the old
instance's debug log and fires `config.apply` with a field that needs a
reload, the moment the attempt-start line appears. A fixed delay in the
trigger moves the landing later into the attempt.
- With the link down, each attempt spends about 370 ms in the serial
  open retry ladder.
- With the link back, it runs open, handshake and replay.

Neither phase can be held open on hardware without a test hook in
production code, which this plan does not add. The log sequence is
what makes a run evidence.

Each leg needs at least three counted runs. On pier1, at least one of
them must land in opening and one in handshaking or replaying. If a
phase has not been landed in 20 triggered tries, the PR says so and
names the mock test that covers that phase instead.

- **pier1 (Linux), `star-adventurer-gti`.** This is the one service
  whose hooks put commands on the wire.
  1. With a client connected, pull the mount's USB cable so the
     supervisor is retrying.
  2. Opening phase: with the cable still out, arm the trigger and let
     it fire on the next attempt. The rebuilt instance's eager
     `start()` in `build()` then fails, because the device is absent.
     The reload loop propagates that error, so the process exits
     non-zero. That is the expected end of this phase, not a failure.
     Once the log proves the overlap, reconnect the cable. systemd then
     restarts the service (`Restart=on-failure`, `RestartSec=5`).
  3. Handshake and replay phases: reconnect the cable, arm the trigger,
     and let it fire on the next attempt.
  4. Repeat until the counts above are met.

  Pass, for every counted run:
  - the old instance logs nothing after its `shutdown` returned;
  - the next open is not refused, which shows the old instance left no
    port held. In the handshake and replay phases that is the reload's
    rebuilt instance. In the opening phase it is the restarted process;
  - the reload returns (handshake and replay phases only);
  - a client connects to the instance that is now serving;
  - the mount reports stopped.

  For PR 8, also record each counted run's old-instance `safety stop
  complete` verdict. `NotAsserted` is not a finding in itself: a
  terminal stop on a conduit the attempt has already closed is
  `NotAsserted` by design (§Deliberately left). It is a finding only
  when the log shows no replacement open began before the shutdown
  hook. A teardown rarely lands inside the attempt's poll join, which
  lasts about one tick on a healthy link. So the evidence that an
  aborted poll leaves the hook its own reply is the mock tests: PR 0's
  `while_open` test, and PR 8's cancel-arm test.
- **rig2 (Windows), `dsd-fp2`.** rig2 carries no GTi, so the Windows
  leg runs the shared-transport device rig2 does have: the FP2 panel,
  on COM4. It follows the same procedure and the same counting. This
  is the leg that exercises the Windows handle release. `dsd-fp2` also
  starts its transport eagerly in `build()`. So in the opening phase,
  once the log proves the overlap, re-enable the device and start the
  service again explicitly. Do not rely on the SCM's recovery
  settings.

  Nobody is at that pier, so the link is dropped remotely. Before
  PR 4's leg starts, confirm the method on rig2 and record it in the
  PR:
  - **Default: disable and re-enable the FP2's device instance over
    ssh,** with `pnputil /disable-device <instance-id>` and
    `pnputil /enable-device <instance-id>` from an elevated shell.
    This removes the COM port while the service holds it, and needs no
    extra hardware. Record the instance ID.
  - **Alternative: switch the UPBv2 USB port the FP2 hangs off,**
    through Pegasus Unity's REST API, if it hangs off one. Record the
    port and the command.

  If rig2 allows neither, run the Windows leg on site instead. Use the
  dev box's `win11` KVM guest, with the PPBA passed through and
  `ppba-driver` running, and drop the link by detaching the USB device
  from the guest live.

Record the outcome in the PR description. `docs/validation/` holds
ConformU records, and these runs are not ConformU runs.

PRs 0, 1, 2, 3, 5, 6 and 7 rest on mock evidence. None of them sends
anything new or changes what the connect path sends. PR 0 only reads
and discards frames a device already sent.

## Follow-ups and what is deliberately left

### Issues to file

- **`Connection::request_timed` cancel-safety** was proposed here as an
  issue. On review (2026-10-04) it moved into the plan as PR 0
  (decision 7).
- **Filed on 2026-10-04**, both found during this plan's review and
  outside its scope:
  - [#1398](https://github.com/rusty-photon/rusty-photon/issues/1398):
    a `Session::close` dropped while it waits on `acquire_lock` has
    already taken its handle, so neither it nor `Drop` lowers the
    count. The last-client stop then never runs again on that
    instance.
  - [#1399](https://github.com/rusty-photon/rusty-photon/issues/1399):
    TLS serving stops accepting at shutdown but does not drain open
    connections, unlike plain HTTP, so `shutdown` can overlap a request
    still in flight.
- **Not filed** (decided on review, 2026-10-04): a synchronous commit
  lock around `publish_recovery`'s check-and-stores and the cleanup's
  record (the `qhy-camera` pattern). It would close the W6 transient,
  which nothing acts on.
- **Issue hygiene on #1243:** done on 2026-10-04
  ([comment](https://github.com/rusty-photon/rusty-photon/issues/1243#issuecomment-5982697182)).

### Deliberately left

- **The W6 transient.** No *final* flag state contradicts the debt.
  `is_available()` can still read true for a few instructions inside
  `publish_recovery`. The debt, which `Session` and (after PR 3)
  `WhileOpen` both consult, is the authority, so this is benign.
- **A stray reply that PR 0 cannot count or place.** PR 0 counts what
  an abandoned request owes (decision 7). On a conduit that delivers in
  order, that closes every route that abandons a request:
  - **R1**, the 5 s bound's abort of a poll that has not returned. GTi
    checks its token only between ticks, and a slow link can keep one
    tick going past 5 s.
  - **R3**, an HTTP handler dropped when its client goes away. The
    pinned ascom-alpaca fork spawns only the Platform 7 `connect` and
    `disconnect` calls, and every other device call runs inline in its
    handler. This is from reading the code; it has not been reproduced.
  - **R4**, an attempt aborted in the middle of its replay, after C3,
    which left a stop's `=\r` ack on the published conduit.
  - An attempt aborted inside its own `cancel_while_open`, whose
    `AbortDetachedGuard` aborts the poll task mid-request.

  Three kinds of stray are not handled:
  - **R2, a late reply after a receive timeout.** A timeout owes
    nothing (decision 7). The serial and UDP transports time a read out
    without clearing input. The timeout is a wire failure, so the
    supervisor takes the transport out of service at once, refusing
    sessions with `Reconnecting`, and then cycles the conduit. Until it
    does, a poll, a 1→0 hook or a shutdown can read the late frame. C1
    deliberately leaves that conduit live for the shutdown hook. The
    late frame can be an ack to a client's motion command.
  - **A frame no request asked for:** a duplicated UDP datagram, or
    qhy-focuser's unsolicited position frames. qhy's `matches` on
    `cmd_id` absorbs the latter.
  - **An owed UDP reply that arrives after the next request's own.**
    The count is right but the order is not, so that one request takes
    the owed reply, and the conduit is back in step after it. Decision 7
    covers when this can happen and what it does to GTi's safety stop:
    it fails safe.

  Open points 3 (R4) and 4 (`biased;` selects in the poll loops, to
  narrow R1) were dropped on review on 2026-10-04. PR 0 counts the
  reply both routes strand. On UDP a reorder can still misattribute
  that one reply, which fails safe at the stop (decision 7).

  On the GTi, what an uncounted stray is decides the outcome:
  - **A stale payload frame.** Every poll reply carries a payload, and
    `Response::decode` rejects any payload on a stop's ack. The stop
    that reads the frame fails to decode. Every later stop reads the
    ack before its own, so the verdict is `NotAsserted`. All three stop
    frames still go out, and the mount acts on them. The log's "may
    still be moving" is then a false alarm. A false `Asserted` cannot
    happen: a displaced reply means the stale frame was read, and
    reading it failed the verdict. A partial frame fails framing the
    same way.
  - **A stale ack (R2, or a duplicated datagram).** It shifts the
    verdict by one frame.
    The verdict then covers the replies to `:L1` and `:L2`, and `:K1`'s
    own reply is never read. A false `Asserted` needs `:K1` alone to be
    refused after both `:L` stops were acknowledged.

  A scratch test on the GTi mock confirmed the payload case. A wrapping
  transport handed back one stale frame first, compared against a
  control run:
  - a stale `:j` reply, a stale `:f` reply and a partial tail (`80\r`)
    each gave `NotAsserted`;
  - all three stop frames were written every time;
  - the control gave `Asserted`.

  PR 1 pins the payload case.

  A conduit carrying a stray at teardown is closed right after the
  hook, so the offset goes no further, unless the leftover outlives the
  close. The mock's reply queue does outlive it: there, the next open's
  `:e1` read the leftover `=\r` and was refused as the wrong device.
  That failure is loud, not a silent offset. Whether a real serial port
  keeps a reply across a close and reopen is not verified. UDP cannot,
  because GTi binds a fresh ephemeral port on each open.
- **An owed reply that never comes.** The next request discards its
  own reply in its place and times out, which raises a reconnect
  (decision 7). The replacement conduit starts with nothing owed. If
  that request is GTi's safety stop, `:L1` still goes out at once,
  `:L2` goes out one read timeout late, and the verdict is
  `NotAsserted`, so the debt replays the stop. It needs an abandoned
  request and the loss of its reply together.
- **An abandoned (dropped) teardown future.** Not reachable in
  production: `BoundServer` and the `build()` rollback always await
  `shutdown`. After PR 8, a cancelled supervisor that was detached this
  way finishes on its own. Its child refuses at C1–C3, and
  `attempt_reconnect_lock` serialises it against the next child. A port
  collision with a cold open on the same instance is still possible,
  and is not addressed.
  - Two corners predate PR 8 and are unchanged by it. Each needs a
    multi-thread race of microseconds against the detached supervisor's
    abort:
    - The stale child can register its poll after the next cold
      start's quiesce, and that start's publish then overwrites it,
      detaching it. Respawning inside C3 would close it, and was
      rejected on review (§Rejected alternatives).
    - Past C3, the stale child's `commit_replacement` can cancel the
      *next* lifecycle's poll task. Closing that would need
      `commit_replacement` to cancel only its own token.
- **A GTi terminal stop that lands on a closed conduit.** C1 narrows
  this: the attempt no longer closes once the token is cancelled. It
  can still happen when the close was already under way.
  - On a reload, the next cold start's unconditional assertion answers
    for it.
  - On a final SIGTERM nothing does.

  The GTi doc states this.
- **A cadence floor for manual kicks.** This is moot while
  `reconnect_now` is test-only. It becomes a requirement of whichever
  change un-gates the hook for an operator CLI.
- **The log level of a refused poll tick in the five services whose
  stop hook does nothing.** They log at `warn!`, where rule 9 would say
  `debug!`. A refusal is practically unreachable for them, so this is
  left unless review asks for it.

## Open points for review

1. **The Windows hardware leg runs `dsd-fp2` on rig2**, because rig2
   has no GTi. The link is dropped with `pnputil`, or a UPBv2 port if
   the FP2 hangs off one. Which one is confirmed on rig2 before PR 4's
   leg. The on-site fallback is the `win11` guest with the PPBA
   (§Validation). This is a fact to check on rig2, not a decision.

Settled on review (2026-10-03):
- bug A records the owed stop before the respawn;
- `IN_ATTEMPT` is not added;
- PR 2's constructor-panic commit ships, and PR 2 moves ahead of bug A;
- rule 10's trigger is any change to `Cargo.lock` or any workspace
  `Cargo.toml` (the root's or a member's), with
  `scripts/repin-bazel-lock.sh` and a pre-commit check
  ([#1391](https://github.com/rusty-photon/rusty-photon/pull/1391));
- "never abort a poll task mid-request" was made part of PR 8 (decision
  6 as first merged). It was undone on 2026-10-04, below, and PR 8 no
  longer does it.

Settled on review (2026-10-04):
- `Connection::request_timed` cancel-safety lands first, as PR 0, by
  owing and skipping; a receive timeout owes nothing (decision 7);
- decision 6 is undone: after PR 0 an abort mid-request strands
  nothing, so `cancel_while_open` and `AbortDetachedGuard` stay;
- open point 2, respawning the poll task inside C3, is rejected
  (§Rejected alternatives);
- open point 3, R4, is closed by PR 0, and needs neither a cancel arm
  that waits nor a second GTi pin. On UDP a reorder can still
  misattribute the one reply, which fails safe at the stop;
- open point 4, `biased;` selects in the poll loops, is dropped: it
  only narrowed R1, whose stranded reply PR 0 counts;
- open point 5, a `stop_owed` wake that skips the cadence floor, is
  rejected (§Rejected alternatives);
- #1398 and #1399 are filed; the W6 commit lock is not
  (§Issues to file).
