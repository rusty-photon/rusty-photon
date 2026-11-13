# Values that carry their time — one type for a cached reading, one rule for every other time

## Goal

Much of what the workspace handles carries the time it happened: a
frame read off a serial line, a sensor sample, a mount position, an
exposure's start. Six serial drivers order the writes to their cache by
that time. Each keeps a `ReadStamp` field beside every cached value and
each store asks the stamp before it assigns, so that a poll's frame read
just before a command cannot land after the command's readback and put
the old value back
([the store-order fix](https://github.com/rusty-photon/rusty-photon/pull/1720)).
The check is right at every store today, and only review keeps it
there: a new store that assigns without asking the stamp compiles, and
passes every test that does not construct the race.

[The issue behind this plan](https://github.com/rusty-photon/rusty-photon/issues/1727)
asks whether one type should hold a value and its stamp so that an
undated store cannot be written, where else in the workspace a value
carries its time, and what the rule is. This plan answers with one
type for the one job where two fields must move together, one rule
that says which clock every other time on a value uses, a census with a
verdict for each site, and five pull requests.

The outcome of this plan:

- In the five drivers whose caches hold one value per stamp, the value
  is private to `Stamped<T>` and `offer` is the only way in: a store
  that skips the stamp does not compile.
- [workspace.md § Values that carry their time](../workspace.md#values-that-carry-their-time)
  states the rule for a cached reading with the type that keeps it, and
  says that a reading's age is measured on the monotonic clock.
- `SensorMean` reads no clock. A sample is dated by the instant its
  frame was read off the wire, age and the averaging window are measured
  on the monotonic clock, and the crate's tests pass instants and never
  sleep.
- The reply-holding test transport that constructs a store-order race
  exists once, not once per driver.

The plan fixes no defect. It removes a way to write one, and it takes
the wall clock out of a reading's age and its averaging window, where a
step of it changes what the three power boxes' drivers report.

## What a time on a value is for

The census below finds five jobs. Only the first is the type's.

| Job | Clock | Carried as | Examples |
|-----|-------|------------|----------|
| **Order** two writers of one cache: the value read last stays | Monotonic, `tokio::time::Instant` | A `ReadStamp` beside the value; `AxisSnapshot::sampled_at` | The six serial drivers; `star-adventurer-gti` |
| **Age**: how old the reading is | Monotonic, the instant that orders it | The same instant | `AxisSnapshot::projected_to`; `SensorMean`, which uses the wall clock today |
| **Record**: tell a person, a file or a client when | Wall clock, `DateTime<Utc>` or `SystemTime` | A field written where the record is made | ASCOM `LastExposureStartTime`; `sentinel`'s epoch milliseconds; `rp`'s `captured_at` |
| **Deadline**: when something is due, or since when it runs | Monotonic; the census names those on the wall clock | An `Instant` with one owner | Move and capture deadlines, lease expiries, `next_restart_at`, `cooldown_until` |
| **Number**: the source or the driver counts its events | None | A sequence number or a generation counter | `last_seq`, `Last-Event-ID`, the QHY wheel's `epoch` |

## The pattern

### The type

```rust
/// A cached value and the instant it was read. The value is private:
/// `offer` is the only way in, so a store cannot skip the stamp.
#[derive(Debug, Clone)]
pub struct Stamped<T> {
    value: Option<T>,
    stamp: ReadStamp,
}

impl<T> Stamped<T> {
    /// Stores `value`, read (or set by a command answered) at `read_at`,
    /// unless the held value was read strictly later. Says whether it
    /// was stored, so that what follows a store runs only after one.
    #[must_use = "a refused value was not stored"]
    pub fn offer(&mut self, value: T, read_at: Instant) -> bool;
    pub const fn get(&self) -> Option<&T>;
    pub const fn read_at(&self) -> Option<Instant>;
}

impl<T: Copy> Stamped<T> {
    pub const fn value(&self) -> Option<T>;
}
```

`Default` is the empty cache and asks nothing of `T`. No constructor
and no setter takes a value without an instant, in tests either: a test
builds a cache by offering.

### The rule, as answers to the issue's eight questions

1. **One type or several?** One type, for ordering, and it is not
   generic over a clock. Nothing in the workspace orders by the wall
   clock and nothing persists a monotonic instant, so a clock parameter
   would join two things that never meet. A record stays a
   `DateTime<Utc>` or `SystemTime` field; a deadline stays an `Instant`.
   Where a monotonic instant has to be shown it is converted where the
   record is made: `sentinel` turns `next_restart_at` into epoch
   milliseconds as it publishes a service's health, and `phd2-guider`'s
   `CaptureClaim` holds both, `expires` to decide and `deadline` for
   messages.
2. **Which instant dates a reading?** The protocol's choice, made where
   the value is offered and explained in the driver's module doc. The
   request-and-reply serial drivers pass `WireTiming::received_at`.
   `star-adventurer-gti` dates a position by `sent_at`, because the
   firmware latches the step count as the command arrives and the reply
   can reach a busy host long after. A value a command sets is dated by
   the command's answer. The type takes the instant as an argument and
   does not record which kind it is: two caches dated differently never
   meet, so a marker would stop nothing.
3. **Monotonic or wall clock?** Order and age use the monotonic clock;
   a step of the wall clock (an NTP correction on a rig that booted
   without a good clock) must not reorder two reads or age a sample.
   The wall clock is for a time that leaves the process, and the
   conversion happens at that boundary. `SensorMean` is the one site
   that measures a reading's age on the wall clock today, and S4 fixes
   it. Some deadlines and running times are on the wall clock too; the
   census names them, and the plan leaves each as it is.
4. **Granularity.** The choice of `T`. A status frame is one `T`;
   `dsd-fp2`'s five reads are five; `qhy-focuser`'s two temperatures
   and voltage, read by one command, become one. A reading that can be
   absent is `T = Option<_>`: `dsd-fp2`'s heater temperature is "never
   read" as an empty cache and "read, no thermistor" as a stored `None`.
5. **Barriers.** Outside the type. An answer that advances a stamp
   without a value (`pa-scops-oag`'s move echo and halt answer) and an
   instant before which a sample's rate is not believed
   (`star-adventurer-gti`'s `rate_barrier`) say what a command
   invalidates, which is the protocol's. `ReadStamp` stays public for
   the first; the second stays in `AxisSnapshot`.
6. **Ties and test clocks.** A read at the held instant is stored,
   everywhere. Requests take turns on the wire, so two reads at one
   instant only happen on a paused test clock, where they are stored in
   turn.
7. **Where it lives.** In `rusty-photon-shared-transport`, beside
   `ReadStamp` and `WireTiming`: every user of the type already depends
   on the crate, and a crate of its own would hold forty lines. `rp`,
   `session-runner` and `sentinel` need no such type, since their times
   are records, deadlines and numbers. `rusty-photon-rolling-stats`
   needs none either once S4 passes it instants.
8. **Testing support.** Yes: one reply-holding transport decorator, the
   means every driver uses to construct a store-order race. S5.

## Census

Taken 2026-11-12 on `main` at `0b300d77`, by searching `services/`,
`crates/` and `tools/` for every field that holds an `Instant`, a
`SystemTime`, a `DateTime<Utc>`, an epoch count, a sequence number or a
generation counter, and reading the writers of each. `ReadStamp` is used
by the six serial drivers' `manager.rs` and nowhere else.

### Order

| Site | What it holds | Verdict |
|------|---------------|---------|
| `dsd-fp2` `manager.rs` | `motor_running`, `cover_raw`, `light_on`, `brightness`, `heater_temp_c`, each with its own `ReadStamp`, stored through five `store_*` methods | **Migrate, S1.** Five `Stamped<T>` |
| `ppba-driver` `manager.rs` | `status` and `power_stats`, each with a stamp; `apply_status` feeds the three means when the frame is admitted | **Migrate, S2.** Two |
| `upbv2-driver` `manager.rs` | `status`, `power`, `boot_state`, each with a stamp; the means as in `ppba-driver` | **Migrate, S2.** Three |
| `gemini-powerbox-adv3-driver` `manager.rs` | `status` with a stamp; the means likewise. `firmware` is read once, by the handshake | **Migrate, S2.** One. `firmware` has one writer and stays |
| `qhy-focuser` `manager.rs` | `position` with a stamp; `outer_temp`, `chip_temp` and `voltage` under one stamp. `target_position` and `is_moving` are what the driver believes of a move it sent | **Migrate, S3.** Two: the position, and the three values read together. The move's two fields are not readings and stay |
| `pa-scops-oag` `manager.rs` | One `status_read` dates `position` and `is_moving`; the move echo and the halt answer advance it without storing a value; `moves_in_flight` keeps a report from clearing a move not yet answered | **Leave as is.** A stamp that moves without a value is a barrier (rule 5). Its field docs say so |
| `star-adventurer-gti` `manager.rs` | `AxisSnapshot::sampled_at` per axis, merged by `merge_axis_if_newer`; `rate_barrier`; `projected_to` | **Leave as is** (decision 11) |
| `qhy-camera` `camera.rs` | Caches written under `cache_commit_lock`, held across the read and the store | **Not applicable.** A lock orders the stores; a stamp is for a value stored outside the lock that ordered the reads |
| `phd2-guider` `connection.rs` | `app_state`, written by the connection's reader task in socket order | **Not applicable.** One writer |
| `pa-falcon-rotator`, `zwo-focuser`, `zwo-camera`, `svbony-camera`, `sky-survey-camera` | No poll loop: a value is read from the device or the SDK when a client asks | **Not applicable** |

Thirteen value-and-stamp pairs in five drivers become `Stamped<T>`.

### Age

| Site | What it holds | Verdict |
|------|---------------|---------|
| `rusty-photon-rolling-stats` `SensorMean` | Each sample with `SystemTime::now()` taken as it is stored; the window and `time_since_last_update` read the wall clock again. Fed by `ppba-driver`, `upbv2-driver` and `gemini-powerbox-adv3-driver` | **Migrate, S4** |
| `star-adventurer-gti` `AxisSnapshot::projected_to` | Carries a position forward from `sampled_at` to now, on the monotonic clock | **Leave as is.** It follows the rule |
| `sentinel` `monitor.rs` `StateChange::timestamp` | A struct no code constructs | **Not applicable.** See [Findings outside the steps](#findings-outside-the-steps) |

### Record

| Site | What it holds | Verdict |
|------|---------------|---------|
| `qhy-camera`, `zwo-camera`, `svbony-camera`, `sky-survey-camera`, `phd2-guider`'s camera | The last exposure's start as `SystemTime`, for ASCOM `LastExposureStartTime` | **Leave as is** |
| `rp` `mcp/internals.rs`, `events.rs`, `safety.rs`, the planner | `captured_at`, `exposure_started_at`, `started_at`, `since` and the planner's times as `DateTime<Utc>`. An `ended` event's `elapsed_ms` is the wall-clock time since its `started_at` | **Leave as is** |
| `session-runner` `runs.rs`; `polar-align` `workflow.rs` | `started_at` and `ended_at`; `measured_at` and `updated_at` | **Leave as is** |
| `sentinel` `state.rs` | `last_poll_epoch_ms`, `last_change_epoch_ms`, `last_probe_epoch_ms`, `next_restart_epoch_ms` | **Leave as is.** The last is converted from an `Instant` as it is published |
| `rp-targets` `WriteStamp` | `updated_at` and `updated_by`, supplied by the caller: who last wrote a target, and when | **Leave as is.** A record of a write, not an order between writers |

### Deadline

| Site | What it holds | Verdict |
|------|---------------|---------|
| `rp` (`mcp/rig_state.rs`, `mcp/internals.rs`, `cooling.rs`, `guiding_watch.rs`, `train_lease.rs`), `phd2-guider` (`service/capture.rs`, `service/lease.rs`, `CaptureClaim::expires`), `star-adventurer-gti` (`mount_device/pulse.rs`, `mount_device/watchers.rs`, `power.rs`), `qhy-camera` `filterwheel.rs` (`sent_at`, `last_status_read`), `sentinel` `health.rs` (`next_restart_at`), `session-runner` (`grace_deadline`), `planetarium-bridge`, `rp-auth`'s memo, `rusty-photon-tls`, `rusty-photon-process`, shared-transport's `last_attempt` | An `Instant` something is due at or began at, with one owner | **Not applicable.** No reading, no second writer |
| `zwo-camera` `camera.rs` `Pulse::until` | A guide pulse's end as `SystemTime`, which `IsPulseGuiding` compares with the wall clock | **Leave as is.** A deadline, not a cached reading, and the plan changes no deadline |
| `zwo-camera` and `svbony-camera` `camera.rs`, `phd2-guider` `service/camera.rs` | `PercentCompleted` measures an exposure's running time on the wall clock, from the `SystemTime` start kept for `LastExposureStartTime` | **Leave as is.** A running time, not a cached reading |
| `phd2-guider` `service/capture.rs` restart marker | `deadline_unix_ms`, a capture's deadline as wall-clock milliseconds, in a file the next process reads | **Leave as is.** It outlives the process (rule 3) |
| `session-runner` `engine/exec.rs` trigger cooldown | The time since a trigger last fired, from the `last_fired` the blackboard records as a wall-clock time | **Leave as is.** It survives a resume (rule 3) |

### Number

| Site | What it holds | Verdict |
|------|---------------|---------|
| `sentinel` `watchdog.rs` `last_seq`; `rp` `routes.rs` `last_event_id`; `rp` `auto_focus`'s `watermark`; `session-runner` `events.rs` `last_seq`; `ui-htmx` `sse_proxy.rs` | The source's own event numbers | **Not applicable.** The number orders the events; no clock is read |
| `qhy-camera` `filterwheel.rs` `epoch` | The driver's count of the moves it sent and the connects it made; a status read taken under an older count says nothing of the move under way | **Leave as is.** A generation counter says which move a read belongs to, which an instant cannot |

## Decisions

Decisions 1–3 and 13 are the owner's, taken 2026-11-12: 1–3 before
the plan was written, 13 on the plan's proposal. Decisions 4–12 are the
plan's proposals; merging the plan settles them.

1. **A plan and a section of workspace.md, no ADR.** The pattern is a
   convention with a small type, and no contested alternative needs an
   ADR. The rule extends the paragraph on the serial drivers' caches in
   workspace.md § Serial-based services.
2. **`SensorMean` is a step of this plan** (S4): samples dated by the
   frame's read instant, the crate still `std`-only, its tests no
   longer sleeping.
3. **The reply-holding test transport is shared, in a pull request of
   its own, after the migrations** (S5).
4. **One type, for ordering, with no clock parameter.** Rule 1.
5. **It lives in `rusty-photon-shared-transport`.** Rule 7. No new
   crate for the type.
6. **The value is private and `offer` is the only way in.** No
   `From<T>`, no `set`, no constructor from a value, and no test-only
   back door: the point of the type is that an undated store cannot be
   written, and a back door for tests is one for production.
   `offer` is `#[must_use]`, because what follows a store (feeding a
   mean, completing a move) must not follow a refusal.
7. **The dating instant is the caller's argument, and the protocol
   picks it.** Rule 2.
8. **Granularity is the choice of `T`.** Rule 4.
9. **Barriers stay outside the type, and `ReadStamp` stays public.**
   Rule 5. `pa-scops-oag` keeps its bare stamp.
10. **No `age()`, no `map`, no serialisation until a caller needs
    one.** `read_at()` is enough for the tests that assert a stamp, and
    S4 gets its instants from `apply_status`'s own argument.
11. **`star-adventurer-gti` keeps `merge_newer`.** It applies the same
    rule, tied to two things the type does not hold: the barrier that
    clears a sample's rate, and the projection, whose result is a
    snapshot dated by no read. An `AxisSnapshot` is also a value in
    transit, built by a poll and merged into the cache later, where a
    `Stamped<T>` is the cache itself.
12. **`SensorMean` takes its instants as arguments, as
    `std::time::Instant`.** No clock trait and no tokio dependency: the
    crate's design doc commits it to depending on nothing, an async
    runtime included, and `tokio::time::Instant::into_std` is the whole
    conversion.
13. **The shared test transport lives in a test-support crate of its
    own, `transport-doubles`, not behind shared-transport's `test-util`
    feature.** The question first put to the owner named the feature as
    the home; the plan proposed the crate, and the owner chose it. The
    build files change the cost. Under Bazel the feature is a second
    library, `rusty-photon-shared-transport_test_util`, with the crate
    name of the first, and a driver's unit test is built with
    `crate = ":<driver>_lib"`, whose dependencies the test inherits. The
    test cannot add the feature's twin beside the plain crate its
    library already links: the comments in `services/rp/BUILD.bazel`
    and `services/zwo-camera/BUILD.bazel` record that this ends in
    `E0464 multiple candidates`, since one graph holding two crates of
    one name cannot name either. Each of the six drivers would need a
    test-only twin of its own library, as `rp` has for the plate
    solver's mock. Under Cargo each driver would turn the feature on in
    a dev-dependency, which compiles `reconnect_now` into every driver's
    test build, where [testing.md §6.13](../skills/testing.md) has the
    feature on for the crate's own tests and no dependent's. A crate
    that depends only on the public `TransportFactory`, `FrameTransport`
    and `TransportError` costs each driver one dev-dependency line and
    one Bazel dependency, and is `testonly` in Bazel. `bdd-infra` is the
    precedent for an unprefixed test-infrastructure crate.

## Steps

Each step is one pull request, opened and babysat under AGENTS.md
rule 14. S1 comes first and S5 last; S2 and S3 follow S1 in either
order; S4 follows S2, since both change the three power boxes'
`apply_status`.

What S1–S3 must not change: a byte on the wire, an ASCOM answer, a
feature file. Each driver's store-order tests hold a reply, let the
other writer store first and then assert the cache. They keep their
scenarios and the values they expect; the one edit a step makes to them
is how an assertion reads the cache, through `get` or `value` where it
reads a field today, and the compiler names every such line. That they
pass is the evidence that behaviour did not move, and each step's pull
request records that they fail with the order check taken out of
`offer`. Tenet 3 is checked in each review: the steps change how a value
already read is stored, and nothing a connect, a handshake or a poll
puts on the wire.

### S1 — `Stamped<T>`, and `dsd-fp2`

- `rusty-photon-shared-transport`: the type beside `ReadStamp`,
  exported from the crate root. Unit tests: an empty cache has no value
  and no instant; the first offer is stored; an earlier one is refused
  and leaves the value and the instant as they were; one at the held
  instant is stored; a later one replaces both.
- `dsd-fp2`: the five value fields become `Stamped<T>`, the five stamp
  fields go, and each `store_*` method is one `offer` with its
  `debug!` on refusal. `heater_temp_c` is `Stamped<Option<f64>>`.
  It goes first because it has one stamp per field and nothing tied to
  a store: the smallest consumer that proves the type.
- Documents: the shared-transport row of workspace.md's crate table and
  § Values that carry their time name `Stamped<T>`; the crate's design
  doc too, if `docs/crates/rusty-photon-shared-transport.md` exists by
  then.

### S2 — the three power boxes

- `ppba-driver` (two), `upbv2-driver` (three) and
  `gemini-powerbox-adv3-driver` (one). In each, `apply_status` feeds
  the three means only when `offer` stored the frame.
- For each driver, a test fails when the means are fed on a refused
  frame; where none does, S2 adds it. The check is a mutation: feed the
  means unconditionally and run the suite.
- Documents: `docs/services/gemini-powerbox-adv3-driver.md` § The cache
  keeps the frame read last names the type. The other two design docs
  describe the behaviour without naming `ReadStamp` and stay as they
  are.

### S3 — `qhy-focuser`

- `position` becomes `Stamped<i64>`; a move completes (`is_moving` and
  `target_position` cleared when the position equals the target) only
  when the offer is stored, as today.
- `outer_temp`, `chip_temp` and `voltage` become one `Stamped` of the
  protocol's `TemperatureResponse`, which already holds the three.
- The same mutation check as S2, on the move's completion.
- `pa-scops-oag` is read again and left alone.

### S4 — `SensorMean` reads no clock

- `add_sample(value, at)`, `get_mean(now)`,
  `time_since_last_update(now)`, `set_window(window, now)` and
  `time_since_freshest_update(means, now)` take `std::time::Instant`s.
  Nothing in the crate calls a clock.
- The three drivers pass the frame's read instant as `at`, the one
  `apply_status` already receives, and `tokio::time::Instant::now()` as
  `now`, both through `into_std`. `TimeSinceLastUpdate` then counts
  from the read off the wire, and follows a paused test clock.
- Samples arrive in order, because only an admitted frame adds one.
  The crate still says what it does with one that does not, and tests
  it: the proposal is to keep the sample and date it as the newest, so
  the window stays ordered and eviction from the front stays right.
- The crate's tests pass instants and stop sleeping: eight `sleep`
  calls in `lib.rs` and `observing_conditions.rs` go, and "at least the
  time slept" assertions become equalities.
- Documents: `docs/crates/rusty-photon-rolling-stats.md` (§ A sample
  stamped in the future reports zero elapsed, and Testing); workspace.md
  § Values that carry their time gains a sentence on age, measured on
  the monotonic clock from the instant that dates the reading.

### S5 — one reply-holding test transport

- A decorator over any `TransportFactory`: it wraps the transport the
  inner factory opens, holds the reply to the next outgoing frame that
  starts with given bytes (or the next reply whatever it answers, which
  `gemini-powerbox-adv3-driver` needs because its hub sends frames
  unasked), tells the test when the reply is held, under a deadline,
  and releases it. Generic over the inner factory, so a test still
  reaches its driver's mock.
- The six copies of `HoldingFactory`, `ReplyHold` and `HoldingTransport`
  in the drivers' `manager.rs` test modules go, 67 to 80 lines each; a
  driver keeps at most a one-line adapter from its `Command` to bytes.
- Home: a new crate, `transport-doubles` (decision 13), a
  dev-dependency of the six drivers and `testonly` in Bazel.
- Documents: the crate's row in workspace.md's crate table, and a short
  section in testing.md on constructing a store-order race with it.

## Risks and what bounds them

- **A migration moves a side effect outside the stored branch.** The
  mutation checks in S2 and S3, and the store-order tests that pass
  unchanged.
- **A monotonic clock may not count a system suspend.** The standard
  library says of `Instant` that "it is also not specified whether
  system suspends count as elapsed time or not". After S4, a suspend
  could leave `TimeSinceLastUpdate` short by its length until the next
  poll, a few seconds later. The poll loop itself runs on the same
  clock today, and the rigs do not suspend during a session. Against
  it: a wall-clock step no longer empties the averaging window or pins
  a sample in it.
- **A copy arms in a way the shared decorator lacks.** Three copies arm
  with a `Command` and two with bytes, and `gemini`'s holds any reply;
  S5's decorator has the two modes these come to, a frame's leading
  bytes and any reply. S5 moves the drivers one commit at a time, so a
  copy that does not fit stops at its own commit, and the decorator
  gains what that copy needs. S5 is not done while a copy remains.

No rig validation is owed: S1–S3 and S5 change nothing a device sees,
and S4 changes the clock behind one reported duration.

## Findings outside the steps

Not filed, and not part of any step; each is the owner's call.

- `sentinel`'s `StateChange`, whose `timestamp` the issue lists as an
  age, is declared in `monitor.rs` and constructed nowhere.
- A second test decorator is copied six times: `InjectableFactory`,
  which fails the next send, in `gemini-powerbox-adv3-driver`,
  `pa-falcon-rotator`, `pa-scops-oag`, `ppba-driver`, `upbv2-driver`
  and `qhy-focuser`. `transport-doubles` would hold it too.

## Definition of done

- No driver holds a value field and a `ReadStamp` field for it side by
  side; `ReadStamp` appears outside its crate only in `pa-scops-oag`.
- workspace.md § Values that carry their time names `Stamped<T>` and
  says that a reading's age is measured on the monotonic clock.
- `rusty-photon-rolling-stats` names no `SystemTime`, and its tests no
  `sleep`.
- `struct ReplyHold` appears once in the tree, in `transport-doubles`.
- Every row of the status table names its merged pull request, the
  issue is closed, and the plan is archived per
  [archiving-plans.md](../skills/archiving-plans.md).

## Out of scope

- Any change to what a store does once it is admitted. S1–S3 move the
  check into the type and leave each driver's side effects as they are.
- `star-adventurer-gti`'s snapshot (decision 11) and `pa-scops-oag`'s
  barrier (decision 9).
- Records, deadlines and numbers: every site the census gives "leave as
  is" or "not applicable".
- A clock trait, or an injectable clock for the workspace. `SensorMean`
  gets instants as arguments; nothing else asks for one.
- The two findings above.

## Implementation status

| Step | Status | Pull request |
|------|--------|--------------|
| Plan | Open | — |
| S1 — `Stamped<T>`, and `dsd-fp2` | Not started | — |
| S2 — the three power boxes | Not started | — |
| S3 — `qhy-focuser` | Not started | — |
| S4 — `SensorMean` reads no clock | Not started | — |
| S5 — one reply-holding test transport | Not started | — |
