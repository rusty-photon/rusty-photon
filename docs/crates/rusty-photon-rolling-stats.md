# `rusty-photon-rolling-stats` Crate Design

Time-windowed statistics over timestamped samples. Today that is one type,
[`SensorMean`] — a rolling mean of `f64` readings over a configurable window.

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

## A sample stamped in the future reports zero elapsed

`time_since_last_update` returns `None` for exactly one condition: no samples
have ever been added. A backwards clock jump — an NTP correction, a VM resuming
from a snapshot — leaves the newest sample stamped ahead of now, and
`SystemTime::duration_since` fails on that. Reporting the failure as `None`
would put a *fresh* reading in the same bucket as "no data", which the ASCOM
drivers surface as `f64::MAX` seconds — the opposite of the truth. A sample
stamped in the future is as new as a sample can be, so it reports
`Duration::ZERO`.

## Consumers

`ppba-driver` and `upbv2-driver` each hold three (temperature, humidity,
dewpoint) and resize them from ASCOM's `AveragePeriod`. Mapping
`AveragePeriod = 0` onto a window is driver policy and lives there, not here —
both currently scale it to their poll cadence, but nothing in this crate
assumes that. See
[`docs/services/ppba-driver.md`](../services/ppba-driver.md) and
[`docs/services/upbv2-driver.md`](../services/upbv2-driver.md).

## Testing

Unit tests live with the code. The timing-dependent ones assert **floors**
after a known sleep rather than ceilings on elapsed time: load can only push an
elapsed time up, so a floor cannot flake, and it rejects a constant-zero
implementation that a tight ceiling would accept. The one ceiling that remains
is 60 s, which catches a timestamp that was never set (an epoch-based answer
reads as decades) without asserting anything about the machine.
