# `rusty-photon-rolling-stats` Crate Design

Rolling statistics, in two types and one module of policy:
[`SensorMean`], a rolling mean of `f64` readings over a configurable time
window; [`SquareSums`], a count and sum of squares that is never reset, for the
RMS over an interval the caller picks; and `observing_conditions`, the policy
the three ASCOM `ObservingConditions` drivers that hold such means share.

This is a workspace library, not a service. It is `std`-only and depends on
nothing: no async runtime, no serialiser, no device library. That is deliberate,
and it is the reason the type lives here rather than in a driver crate. A
rolling mean is wanted anywhere something is sampled on a cadence, and a
consumer should not inherit `ascom-alpaca` to get one; the same goes for a
guide error's RMS.

## Scope

- Hold `f64` samples, each dated by an instant the caller hands in, in a
  time-bounded window.
- Answer the mean of the samples **inside** that window.
- Answer how old the newest sample is.
- Resize the window, evicting what no longer fits.
- Keep a running count and sum of squares, and answer the RMS over the values
  folded between two readings of it.

Out of scope: persistence, interpolation, statistics beyond the mean and the
RMS (min/max and percentiles would be additions here, not new crates), any
knowledge of what the samples measure, and reading a clock.

## The caller hands in every instant

`SensorMean` reads no clock. `add_sample(value, at)` takes the instant that
dates the sample. `get_mean(now)`, `time_since_last_update(now)` and
`set_window(window, now)` take the instant the question is asked at, and so
does `time_since_freshest_update(means, now)`. Each is a
`std::time::Instant`, a reading of the monotonic clock, so a step of the
wall clock moves neither a sample's age nor its place in the window. Whether
a system suspend counts as elapsed time is the platform's: the standard
library leaves it unspecified for `Instant`, so an age can come out short by
the length of a suspend until the next sample arrives.

What dates a sample is therefore the caller's choice. A driver hands in the
instant its frame came off the wire rather than the instant it stores the
reading, so the age it reports counts from the read
([`docs/workspace.md` § Values that carry their
time](../workspace.md#values-that-carry-their-time)). Which clock the
instants come from is the caller's too: a driver on tokio converts a
`tokio::time::Instant` with `into_std`, so its means follow a paused test
clock, and the crate depends on no async runtime to allow it. A clock trait
would add a dependency or a type parameter for what an argument already
does.

A sample is inside the window as of `now` while it is no older than the
window is long; one exactly a window old is inside. The comparison is made
on the sample's age, not against a cutoff of `now` less the window:
`Instant::checked_sub` gives that cutoff as an `Option`, whose `None` would
need an answer of its own, and the age always has one.

### A sample dated before the newest is dated as the newest

Samples are expected in the order of their instants, and the drivers hand
them in so: only a frame read after the cached one adds a sample.
`add_sample` still answers for one that is not. It keeps the sample and
dates it as the newest one held. The reading counts towards the mean, and
the buffer stays in the order of its instants, which eviction relies on: it
drops from the front and stops at the first sample inside the window.
Dropping the sample would lose a reading the caller did take. Keeping its
own date would put an older sample at the back, where
`time_since_last_update` reads the newest from, and behind a newer one that
eviction stops at.

## The window is applied on read

`add_sample` evicts what has aged out, and `get_mean` **also** filters by the
window before averaging. The second filter is the point of the crate, not a
redundancy.

Eviction alone is only correct while samples keep arriving. A sampler that
stops — a driver whose poll loop is erroring while its session stays open —
leaves a buffer of readings that all aged out with nothing arriving to evict
them. Averaging those and returning the result reports a reading from hours ago
as current. For an `ObservingConditions` dewpoint, that is the number a client
decides dew-heater duty from on a night when the real dewpoint has since been
crossed.

So `get_mean` returns `None` when nothing in the buffer is inside the window.
Callers already have a "no value yet" path — the ASCOM drivers map `None` to
`VALUE_NOT_SET` — and an honest absence is worth more than a confident stale
number. This is project tenet 2.

It stays a **read-side filter**: `get_mean` takes `&self` and the buffer is
unchanged until the next insert.

## A steady reading averages to itself

`get_mean` adds up each sample's offset from the oldest sample inside the
window and adds the mean offset to that sample: `x₀ + Σ(xᵢ − x₀) / n`. A
plain sum divided by the count rounds at every partial sum, so sixty samples
of 27.4 come to 27.400000000000023 and three to 27.399999999999995; at sixty
samples most one-decimal readings carry such a residue. The offsets of equal
readings are all zero, so a sensor that holds steady reads back exactly the
value it reports, which is what a client compares against its own display.

## `sample_count` counts the buffer, not the window

`sample_count` returns buffer occupancy, which between a sample aging out and
the next one arriving is *more* than `get_mean` averages. The asymmetry is
deliberate. It is what lets a test distinguish "eviction ran" from "the read
filter hid it"; a filtered accessor would return the post-eviction number
whether or not eviction existed at all.

## `time_since_last_update` reports emptiness, not staleness

`None` means the buffer holds nothing. There are two ways to get there:
nothing was ever added, or `set_window` shrank the window far enough to evict
everything it held.

Staleness is deliberately *not* one of them. A buffer whose samples have all
aged out still reports how old the newest one is, even though
`get_mean` has stopped answering. That pairing is the point: `get_mean` going
`None` is what a caller notices, and this is how it learns how stale the
reading it can no longer have actually is.

### A sample dated after `now` reports zero elapsed

A caller that reads its clock and then a buffer another task has added to
in between asks about a sample dated after its `now`. Answering `None`
would put a *fresh* reading in the same bucket as an empty buffer, which the
ASCOM drivers surface as `f64::MAX` seconds — the opposite of the truth. A
sample dated after `now` is as new as a sample can be, so it reports
`Duration::ZERO`, and `get_mean` counts it as inside the window.

## `SquareSums`: the RMS over an interval the caller picks

A window answers "what is it now". Some callers need the statistic over an
interval of their own instead, timed by something the sampler knows nothing
about: rp records the RMS guide error over one camera exposure from the guider
service's steps. A window cannot serve that once the interval outlasts it, and
the sampler cannot size one for intervals it never hears of.

`SquareSums` is cumulative instead. `add` folds a value into a count and a sum
of squares, and nothing is ever evicted or reset. A caller reads it at both ends
of its interval, and `since(&earlier)` subtracts the two readings into the
values folded in between, whose `rms` is the interval's. The readings can cross
a process boundary: `count` and `sum_of_squares` go on the wire, and
`from_parts` rebuilds a reading on the other side, refusing a sum that is
negative or not finite, which no accumulator reports.

- **A non-finite square is skipped, not counted.** One would leave the sum
  non-finite for good, every later interval with it, and JSON has no spelling
  for it.
- **`since` refuses a reading that is not earlier.** A count or a sum that went
  backwards means the two readings are not of one accumulator in order, so
  their difference means nothing. The sum only ever grows, so two genuine
  readings never differ by a negative amount. A caller that may read two
  accumulators, such as two runs of a service, needs its own way to tell them
  apart: the guider service sends an `epoch` beside its readings.
- **`rms` converts the count through `u32`.** A count past `u32::MAX` answers
  `None`; no caller comes near it.

## The `observing_conditions` policy module

`SensorMean` knows nothing about ASCOM, and the crate still depends on
nothing. What `observing_conditions` adds is the policy shared by the three
drivers that hold three means each, written once so that a change reaches all
three:

- **`AveragePeriod` onto a window.** `MAX_AVERAGING_PERIOD` (24 h, ASCOM's
  ceiling) and `average_period_hours()` (the `[0, 24]` range every check
  reads, which `contains` makes NaN-safe); `instantaneous_window(poll)`, the
  window that serves `AveragePeriod = 0`: `max(3 × poll interval, 10 s)`, the
  shortest window that still always holds the newest sample under healthy
  polling; and `effective_window(hours, poll)`, the window for any period —
  the period itself, **never shorter than the instantaneous window**. A
  window shorter than the poll cadence, applied on read, holds no sample
  for part of every poll cycle, so a client that set a period of a few
  seconds would read "no value" between polls while the sensor is healthy;
  the floor keeps the newest sample inside. The period the client set is
  recorded verbatim by the driver and read back as-is, so the floor changes
  what the means average over, not what `AveragePeriod` reports. A
  non-finite or oversized period lands on the instantaneous window rather
  than panicking.
- **The empty-name `TimeSinceLastUpdate`.** `time_since_freshest_update`
  answers over several means: the freshest sample's age at the instant it
  is handed, and `None` while every mean is empty, so the client reads "no
  data" rather than "updated just now". An empty mean does not hide a
  fresher one beside it.

The policy speaks in hours and `Duration`s; the ASCOM error codes stay with
the drivers.

## Consumers

`ppba-driver`, `upbv2-driver` and `gemini-powerbox-adv3-driver` each hold
three (temperature, humidity, dewpoint), size them through
`effective_window` from the configured `averaging_period` and from every
`SetAveragePeriod`, and answer the empty-name `TimeSinceLastUpdate` through
`time_since_freshest_update`. Each dates a sample by the read instant of the
status frame that carried it and asks at `tokio::time::Instant::now()`, both
through `into_std`.

`rp` rebuilds two readings of the guider's running totals with `from_parts`
and subtracts them into the guiding over an exposure. The `guider` service
keeps the totals as a `SquareSums` per axis of its guide error. See [`docs/services/guider.md` §
Statistics](../services/guider.md#statistics) and [`docs/services/rp.md`
§ Rig state](../services/rp.md#rig-state). See
[`docs/services/ppba-driver.md`](../services/ppba-driver.md),
[`docs/services/upbv2-driver.md`](../services/upbv2-driver.md) and
[`docs/services/gemini-powerbox-adv3-driver.md`](../services/gemini-powerbox-adv3-driver.md).

## Testing

Unit tests live with the code. None of them waits. A test of `SensorMean`
or of `time_since_freshest_update` reads the clock once, for an origin,
because an `Instant` has no other constructor; every other instant is that
one plus a `Duration`. The answers are therefore exact, and the tests assert
them as equalities: an age to the nanosecond, a sample exactly one window
old on the inside of the edge and one a nanosecond older on the outside.
Load on the machine has nothing to move.
