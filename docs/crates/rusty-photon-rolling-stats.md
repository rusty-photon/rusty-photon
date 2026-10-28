# `rusty-photon-rolling-stats` Crate Design

Time-windowed statistics over timestamped samples. Today that is one type,
[`SensorMean`] — a rolling mean of `f64` readings over a configurable window —
and one module of policy, `observing_conditions`, which the three ASCOM
`ObservingConditions` drivers that hold such means share.

This is a workspace library, not a service. It is `std`-only and depends on
nothing: no async runtime, no serialiser, no device library. That is deliberate,
and it is the reason the type lives here rather than in a driver crate. A
rolling mean is wanted anywhere something is sampled on a cadence, and a
consumer should not inherit `ascom-alpaca` to get one.

## Scope

- Hold timestamped `f64` samples in a time-bounded window.
- Answer the mean of the samples **inside** that window.
- Answer how long ago the newest sample arrived.
- Resize the window, evicting what no longer fits.

Out of scope: persistence, interpolation, any statistic other than the mean
(RMS, min/max and percentiles would be additions here, not new crates), and any
knowledge of what the samples measure.

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
aged out still reports how long ago the newest one arrived, even though
`get_mean` has stopped answering. That pairing is the point: `get_mean` going
`None` is what a caller notices, and this is how it learns how stale the
reading it can no longer have actually is.

### A sample stamped in the future reports zero elapsed

A backwards clock jump — an NTP correction, a VM resuming from a snapshot —
leaves the newest sample stamped ahead of now, and
`SystemTime::duration_since` fails on that. Reporting the failure as `None`
would put a *fresh* reading in the same bucket as an empty buffer, which the
ASCOM drivers surface as `f64::MAX` seconds — the opposite of the truth. A
sample stamped in the future is as new as a sample can be, so it reports
`Duration::ZERO`.

## The `observing_conditions` policy module

`SensorMean` knows nothing about ASCOM, and the crate still depends on
nothing. What `observing_conditions` adds is the policy the three drivers
that hold three means each had written out three times, and which a fix had
already reached in one copy only:

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
  answers over several means: the freshest sample's age, and `None` while
  every mean is empty, so the client reads "no data" rather than "updated
  just now". An empty mean does not hide a fresher one beside it.

The policy speaks in hours and `Duration`s; the ASCOM error codes stay with
the drivers.

## Consumers

`ppba-driver`, `upbv2-driver` and `gemini-powerbox-adv3-driver` each hold
three (temperature, humidity, dewpoint), size them through
`effective_window` from the configured `averaging_period` and from every
`SetAveragePeriod`, and answer the empty-name `TimeSinceLastUpdate` through
`time_since_freshest_update`. See
[`docs/services/ppba-driver.md`](../services/ppba-driver.md),
[`docs/services/upbv2-driver.md`](../services/upbv2-driver.md) and
[`docs/services/gemini-powerbox-adv3-driver.md`](../services/gemini-powerbox-adv3-driver.md).

## Testing

Unit tests live with the code. The timing-dependent ones assert **floors**
after a known sleep rather than ceilings on elapsed time: load can only push an
elapsed time up, so a floor cannot flake, and it rejects a constant-zero
implementation that a tight ceiling would accept. The one ceiling that remains
is 60 s, which catches a timestamp that was never set (an epoch-based answer
reads as decades) without asserting anything about the machine.
