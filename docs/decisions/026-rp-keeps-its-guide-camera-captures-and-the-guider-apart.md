# ADR-026: rp keeps its guide camera captures and the guider apart

## Status

Accepted (2026-11-15, by the owner). Supersedes in part
[ADR-024](024-native-guider-as-a-standalone-alpaca-client-service.md):
decision 6 whole, the clause of decision 5 "It does so only while `rp`
holds no lease on the camera (decision 6)", and "the camera lease" in
decision 7's list of what the contract gains. The rest of ADR-024
stands.

## Context

ADR-024 makes the guide camera an ordinary camera: its rusty-photon
driver serves it, it sits in `rp`'s roster, and the guider is an Alpaca
client of it. `rp` exposes on the same camera at times: a focus
provider's training run on the guiding train, a plate solve through
it, the dusk document's test frame. Both clients reach the camera
through the one driver that serves it, so the vendor SDK has one user
and nothing conflicts there, as it did when PHD2 opened the camera
through an SDK of its own.

What the two clients share is Alpaca's device state. A camera has one
exposure in flight, one settings cache, one image slot and one
`AbortExposure` for all its clients, and the standard has no session
that would tell them apart. A capture of `rp`'s that overlaps a run of
the guider's loses its exposure to the driver's refusal of a second
`StartExposure`, or its binning and region to the guider's writes, and
an abort sent by one ends the other's exposure. So `rp`'s captures
through the guide camera and the guider's runs must not overlap, and
something outside the standard has to keep them apart.

ADR-024 decision 6 settled that with a lease the guider serves: while
`rp` holds it the guider refuses to expose, and the guider grants it
only when idle. Designed out in full, the lease is three endpoints,
two error codes and a field of the guider's device report; a token, a
time to live, a renewal every 20 s and a release on every path in
`rp`; and a train lease that ends when the guider restarts and drops
the camera lease it held.

The owner weighed three ways to keep the two apart on 2026-11-15,
while `rp`'s side of the guider's contract was being built (the plan's
stage G6):

1. **The guider serves a lease**, as ADR-024 decided. It binds every
   client of the guider's API, since the guider itself refuses to
   expose while the lease is held. It costs the lease's endpoints and
   its state in the guider, and in `rp` the token, the renewals, the
   releases and the train lease's end on a lost camera lease.
2. **The guider is the guide camera's only user, and `rp` reads its
   frames.** `rp` takes no exposure through the guide camera; a
   training run or a test frame takes its frames from the guider. Two
   clients never expose on the camera, so nothing has to keep them
   apart. It costs a frame path in the guider's API, and a capture
   through the guide camera becomes a request to the guider, which
   would have to take the exposure, binning and gain a capture sets.
3. **`rp` keeps them apart on its own.** `rp` holds its own captures
   through the guide camera and the starts of guiding it sends off
   each other, in its memory, and exposes only after it has read the
   guider idle. The guider serves nothing for it.

The owner chose the third: it is the simplest of the three, and it
keeps the guider's API smaller. He accepted what it does not cover: a
client of the guider's API other than `rp` (a person's `curl`, a
script, a second `rp`) can still start the guider while `rp` exposes,
and a restart of `rp` forgets what it held.

## Decision

1. **The guider serves no camera lease.** Its API has no lease
   endpoints, no `camera_leased` or `lease_not_found` code and no
   `leased` field in `GET /api/v1/devices`, and it refuses no request
   because a lease is held. It has the guide camera to itself while it
   exposes. On the camera it uses the exposure members and writes the
   frame settings they need, as ADR-024 decision 5 says, whenever a
   client has asked it to loop, calibrate or guide.

2. **`rp` keeps its captures and the guider's runs apart with the guide
   camera guard.** The guard exists when a guider is configured, on the
   guiding train's camera, the guide camera. With no guider configured
   there is no guard, and nothing reads the guider for a capture or a
   train lease.
   - **Its holders** are a capture through the guide camera, from
     before anything is written to the camera until its image is
     downloaded, or for a cancelled capture until the camera has
     stopped exposing; a guiding train's train lease, from step 2 of
     its acquire until the lease ends, every ending included; and a
     start of guiding, `start_guiding` or the rotate-while-guiding
     ladder's start, for the length of its request to the guider, the
     stop that undoes a cancelled start included.
   - **`rp`'s own users of the camera hold it together**: captures and
     a guiding train's lease already take turns on the camera's capture
     slot and the lease's reservation. A start holds it alone.
   - **Nobody waits on the guard.** Whichever finds it held by the
     other kind is refused at once, naming the holder: a start that
     finds a capture or a train lease, named by its id; a capture or a
     lease that finds a start in flight.
   - **The guider idle.** Having taken the guard, a capture or a
     guiding train's lease reads the guider's stats and is refused
     unless the state is `idle`, naming the state. A read that fails
     refuses it, naming the error. Every capture through the guide
     camera reads, one under a train lease included. The read is the
     only request the guard sends the guider. Within `rp` this leaves
     no window: a start cannot land between the read and the
     exposure.
   - **What takes no guard.** `dither`, `pause_guiding`,
     `resume_guiding` and the ladder's re-selection and resume act on a
     run or a loop, which an idle guider does not have;
     `calibration/clear` exposes nothing; and a stop is a stop-class
     command.
   - **The check of the guider's devices runs before a start only.**
     That the guider's camera is the guiding train's and its mount
     `rp`'s is checked before every start, so a run `rp` started is on
     the guide camera. A capture and a train lease check no devices.

   [rp.md § The guide camera guard](../services/rp.md#the-guide-camera-guard)
   specifies it, with the train lease's step that takes it.

3. **A train lease's step 2 is the guard.** A guiding train's lease
   takes the guide camera guard at the place in the acquire where it
   took the camera lease, and for the same reason: the step refuses at
   once, so it costs little to learn. An imaging train's lease takes no
   guard. The acquire's result has no `camera_lease` field, and a train
   lease neither renews nor releases anything at the guider, nor ends
   because the guider restarted.

## Consequences

- **The guider's API is smaller**, and so is `rp`'s side of it. The
  guider keeps no lease state, and `rp` keeps no token, renewal loop or
  release path for one. Its guard is a lock in `rp`'s memory and one
  read of the guider's stats.
- **A restart of the guider ends no train lease.** A guider starts idle
  and exposes only when a client asks
  ([tenet 3](../workspace.md#project-tenets), no actuation on connect),
  so its restart takes nothing from a capture or a training run that
  `rp` has under way.
- **Accepted: another client can start the guider while `rp`
  exposes.** The guard binds `rp`'s own calls. A start that reaches the
  guider from elsewhere during a capture of `rp`'s meets the camera as
  two clients of any camera do: the driver refuses the second
  `StartExposure`, which the guider counts as a failed frame, and the
  guider's settings writes can reach `rp`'s frame, which then does not
  match its document. Every capture through the guide camera reads the
  guider's state before it exposes, so a training run stops at its
  next capture and puts back as on any failure.
- **Accepted: a restart of `rp` forgets the guard.** A capture the
  restart cut short can leave the camera exposing to the end of its
  frame, and a start sent in that time meets the exposure in the
  driver.
- **The guider can be used without `rp`** as before, and a person who
  drives it by hand while `rp` captures through the guide camera keeps
  the two apart by hand.

## References

- [ADR-024](024-native-guider-as-a-standalone-alpaca-client-service.md)
  — the native guider, of which this replaces decision 6.
- [rp.md § The guide camera guard](../services/rp.md#the-guide-camera-guard),
  [§ Train Lease](../services/rp.md#train-lease) — the guard and the
  train lease's step that takes it.
- [guider.md § Architecture](../services/guider.md#architecture) — the
  division of the guide camera and the mount between `rp` and the
  guider.
- [`docs/plans/native-guider.md`](../plans/native-guider.md) § 7 and
  decision 14 — the frame path, and the decision as the plan records
  it.
