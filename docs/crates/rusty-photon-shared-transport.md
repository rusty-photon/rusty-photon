# `rusty-photon-shared-transport` Crate Design

One wire, many clients. The crate owns everything about a driver's link
to its device that is not the device's protocol: opening the conduit,
sharing it between the ASCOM devices and the poll task that talk over
it, putting one exchange on the wire at a time, keeping the device
stopped while no client is attached, and recovering a link that drops.
A driver supplies a codec, a way to open the conduit and four hooks,
and keeps its own cached state; the handshake and its identity check,
what the no-client state is, the poll body and the mapping of errors to
ASCOM are the driver's.

This is a workspace library, not a service, and it knows nothing of
ASCOM. Every serial and UDP Alpaca driver in the workspace is built on
it ([Adopters](#adopters)).

```text
 ServerBuilder::build ─start()─► Arc<SharedTransport<C>>
                                   │ slot ─► cell ─► Arc<Connection<C>>
                                   │                   │ command lock
 ASCOM device ─acquire()─► Session<C> ──request()──────┤
 poll task ◄────────────── WhileOpen<C> ─request()─────┤
 supervisor ── on a wire failure: close, reopen,       ▼
               handshake, swap the cell's connection   FrameTransport
```

## Adopting the crate

### What a driver supplies

- **A `Codec`** ([Codec contract](#codec-contract)): typed `Command`
  and `Response`, an `Error`, `encode` and `decode` over whole frames,
  and the overrides its protocol needs.
- **A `TransportFactory`**, whose `open()` returns a fresh
  `Box<dyn FrameTransport>` on every open — a lazy first `acquire`, a
  cold `start` and every reconnect attempt. A serial factory calls
  [`open_serial_port`](#opening-a-serial-port) and wraps the stream in
  `SerialFrameTransport::new(stream, terminator, max_frame_size)` with
  its read and write timeouts. A UDP factory binds and `connect()`s a
  socket by its own rules (the GTi binds inside the mount's subnet) and
  wraps it in `UdpFrameTransport`.
- **`Hooks`** ([Hooks](#hooks)). `Hooks::noop()`, also the `Default`,
  does nothing and asserts trivially.
- **A manager** wrapping `Arc<SharedTransport<C>>`
  (`SharedTransport::new(factory, codec, hooks)`) and the cache its
  hooks fill. `ServerBuilder::build()` calls
  `manager.transport().start()` before the HTTP listener binds, so a
  missing or wrong device fails the process before it advertises
  anything, and calls `shutdown()` if anything after the start fails.
  `BoundServer::start()` calls `shutdown()` once the HTTP server has
  stopped, on a shutdown and a reload alike, so the port and the
  supervisor are released before the service rebinds.

A request fails with `SessionError<E>`: `Transport` with a
`TransportError` (`Open`, `Io`, `Timeout` with the configured timeout,
`Eof` for a closed peer, `Framing`, or the retryable `Reconnecting`),
`Codec(E)`, or `SkipExhausted(n)` after `n` frames that did not match.
The lifecycle's own refusals are `Io` errors whose messages say what
happened. The handshake returns the codec's error type, so a codec's
error carries transport failures too: a handshake request that fails on
the wire reaches the caller of `start()` or `acquire()` as
`SessionError::Codec`. A driver flattens both into its own error at the
manager's boundary, the `TransportError` arm being what
`rusty-photon-driver`'s `driver_error!` macro generates.

### The device side

One ASCOM device holds one `Session`, in a slot
`Arc<RwLock<Option<Session<C>>>>`. Two devices on one wire —
`ppba-driver`'s Switch and ObservingConditions — each hold their own,
and the last of them to release runs the last-disconnect hook.

`set_connected` holds the slot's write lock across the whole
check-and-modify: read the slot, `acquire()` or `close()`, store.
Without it, two concurrent `Connected = true` calls both see `None` and
both acquire; one session lands in the slot and the other is dropped,
released only by `Drop`'s detached fallback. The slot is the connected
state: `Connected` answers `slot.is_some() && is_available()`, and the
device keeps no flag beside it that could disagree.

A disconnect closes the session with `close().await` rather than
dropping it, so the last-disconnect hook has run before the ASCOM call
returns, and fallible work after `acquire()` closes the session the
same way before it returns its error, as the GTi's mount does. A task
that must outlive its device's connection holds a session of its own —
the GTi's slew, park and pulse-guide watchers do — and the
last-disconnect hook waits for the last of them.

### Adopters

`dsd-fp2`, `gemini-powerbox-adv3-driver`, `pa-falcon-rotator`,
`pa-scops-oag`, `ppba-driver`, `qhy-focuser`, `star-adventurer-gti`
and `upbv2-driver`. All eight call `start()` from
`ServerBuilder::build()`, so every production transport runs in
`ServiceLifetime`. `star-adventurer-gti` is the one with a UDP
transport, and the one whose last-disconnect hook puts anything on the
wire. `rusty-photon-driver` depends on the crate for `TransportError`,
and `transport-doubles` for its test doubles ([Testing](#testing)).

## Lifecycles

`SharedTransport` holds the refcount of live sessions, the slot with
the current connection cell, the poll task, the supervisor and the
mode, under two lifecycles.

**`LazyAcquire`** is what a transport gets when nothing calls `start`,
as in a driver's unit tests. The conduit follows the clients.

- The 0→1 `acquire` releases what a previous lifecycle left behind
  ([Giving the port back](#giving-the-port-back)), opens the conduit,
  runs the handshake, pays any [safety debt](#the-safety-debt), builds
  the poll future and publishes the slot. A failure or panic before the
  publish rolls the refcount back, so the next `acquire` opens again.
- A later `acquire` bumps the refcount and clones the slot's cell.
- The last release marks the transport unavailable, cancels the poll
  task, runs `on_last_disconnect`, closes the conduit and empties the
  slot.
- `shutdown()` does nothing and `Hooks::shutdown` never runs. There is
  no supervisor: a conduit that fails stays in the slot, its requests
  failing, until every session is released and the next 0→1 opens a
  fresh one.

**`ServiceLifetime`** is what `start()` opts into: the conduit opens at
`start()` and closes at `shutdown()`, whatever the clients do. The
device is validated at boot, the port lives as long as the driver, the
handshake is paid once, the poll runs whether or not a client asks, and
a lost link is the supervisor's to recover, not the next client's.

- A cold `start()` releases what was left behind, the supervisor
  included, opens the conduit, runs the handshake,
  [asserts the no-client state](#the-startup-gate), builds the poll
  future, publishes the slot with the refcount at zero, and spawns the
  poll task and then the supervisor. An error returns with nothing
  published, and a guard clears the flags so a failed start never reads
  as serving or as being retried.
- `start()` on a transport a client has already opened lazily promotes
  it in place: it spawns the supervisor, then sets the mode, with no
  I/O and no assertion, since a client may be driving the device. On a
  started transport it returns `Ok` at once.
- `acquire` bumps the refcount and clones the cell; it runs no
  handshake. During a reconnect it hands out a session whose requests
  answer the retryable `Reconnecting`, not a shutdown error. After
  `shutdown()` it refuses with "transport has been shut down", whatever
  the refcount.
- The last release runs `on_last_disconnect` and nothing else: the poll
  task, the supervisor and the port stay up for the next client.
- `shutdown()` marks the transport unavailable, cancels and joins the
  supervisor, marks it unavailable again (an attempt that finished
  inside the join may have advertised a recovery), cancels the poll
  task, runs `Hooks::shutdown` on the conduit the slot names and closes
  it, then empties the slot and closes what its cell holds at that
  moment. It clears the reconnecting flag, writes off the safety debt
  and resets the cadence clock. The mode stays set, so a later
  `acquire` refuses and a later `start()` cold-starts.

`shutdown()` does not close live sessions; their requests fail from
then on. It assumes the HTTP server has stopped and drained, as every
driver's has, since the serve future drops the router, its devices and
their sessions first. A request already past its checks would
otherwise wait on the command lock, which `Hooks::shutdown` releases
between its commands, and could reach the device after the final stop.

The mode is an `AtomicBool`, set only by `start()` and never cleared.
`start`, `shutdown`, `acquire` and the last release read it under
`acquire_lock`, which spans the whole 0→1 and the whole 1→0, so one
transition sees one mode. The reconnect attempt cannot take that lock —
`shutdown()` holds it while joining the supervisor that runs the
attempt — and reads the mode once, at its replay decision.

`Session::close().await` releases inline. Dropping a session spawns
the release on the current runtime, and with none leaves the refcount
and the cleanup undone, with a warning. A session reads its connection
through the cell, so a reconnect, which swaps the connection inside the
cell, is invisible to it; a cold `start()` publishes a new cell, and a
session from before it fails as closed until reacquired.

## Hooks

Each hook is a boxed closure. `handshake`, `on_last_disconnect` and
`shutdown` take a `&Connection<C>` and share the command lock with
every session, which is why `Connection` is public.

**`handshake`** runs on every open, before any session can reach the
conduit: the lazy 0→1, the cold start and every reconnect attempt, and
not on a `ServiceLifetime` `acquire` or a promotion. Its error fails
the open with nothing published. It may treat a probe as optional and
ignore its error: every open drops the wake that error raised
([The reconnect signal](#the-reconnect-signal)), and a reconnect counts
wire failures from the end of the handshake.

**`on_last_disconnect`** asserts the no-client state, what must hold on
the device while nothing is attached: for the GTi both axes halted and
tracking stopped; every other adopter's hook issues nothing. It runs on
every 1→0 (in `LazyAcquire` after the poll task is cancelled; in
`ServiceLifetime` with the poll running and the port staying open), on
every cold `start()`, on a reconnect's replacement conduit, and on a
lazy 0→1 that pays a debt ([The no-client state](#the-no-client-state)).

- **Stop-class only**: halt an axis, stop tracking, abort an exposure;
  never a park, a slew, a cover or lamp, a power or dew toggle. It runs
  on start and reconnect paths, where
  [tenet 3](../workspace.md#project-tenets) permits stopping and
  nothing else.
- **It returns a verdict**, `StateAssertion::Asserted` or
  `NotAsserted`: does the state now hold on the device? The runtime
  sees a command that never reached the wire, not one the device
  answered without the state holding — a refusal, or a reply a typed
  decode above the codec rejects, as with the GTi's raw-frame codec.
  The verdict carries no reason: a refusal, an undecodable reply, the
  wrong device and a command never sent all fold to `NotAsserted`, and
  the hook logs the concrete error. `StateAssertion::and` folds several
  commands into one answer.
- **Best-effort about its own errors**: a failed request is logged in
  the hook, never propagated, and does not stop the cleanup. A panic
  unwinds through the cleanup; a guard records the stop as not landed
  ([The safety debt](#the-safety-debt)) but cannot close the conduit,
  which stays held until the next open releases it.
- **It does not clear what the handshake set up.** In `ServiceLifetime`
  the next `acquire` runs no handshake, so a cache emptied here stays
  empty until a reconnect refills it. Such cleanup goes in `shutdown`.

**`shutdown`** runs once per `start()`/`shutdown()` cycle, in
`ServiceLifetime` only, with the supervisor joined and the poll task
cancelled so neither races it. Its outcome is not read: a stop it does
not land is answered by the next cold start. A panic in it leaves the
conduit in the slot, where the next open closes it.

**`while_open`** is the poll task. Its future is built before the
publish, so a panic building it leaves nothing published, and spawned
after. Its lifetime is the transport's, not a session's: it runs across
client disconnects in `ServiceLifetime`, and is cancelled before the
conduit it polls closes — at the lazy 1→0, at `shutdown()`, at each
reconnect attempt (and spawned again on the replacement) and before an
open over a conduit left behind. Cancelling joins it for up to 5 s,
then aborts it; a teardown abandoned mid-join aborts the task it took.

- `WhileOpen::request` goes straight to the connection, past the
  reconnecting and debt checks a session's request makes, and a
  replacement's poll task is spawned before the safety replay. A poll
  body therefore only inquires, as every poll in the workspace does.
- Its liveness is its own: a body that returns or panics just stops,
  and the transport goes on reporting itself available. Treating a dead
  poll task as a transport failure would cycle the port for a bug the
  respawned task repeats, and on Windows that cycle meets the
  `Access is denied` of a port reopened too soon. A stale cache is the
  lesser failure; a hook that must survive its own failures handles
  them in the body and exposes its staleness.

## The command lock

`Connection` owns the boxed `FrameTransport` behind a mutex, the
command lock. Every request on a conduit — sessions, the poll task, the
hooks — holds it for its whole exchange, so one exchange is on the wire
at a time and the device has one command outstanding.

An exchange that has the lock runs to its reply whether or not its
caller still waits: the caller takes the lock, and the exchange runs on
a task of its own that reads the reply and discards it if the caller
has gone, as when an HTTP client hangs up. The protocols here carry no
tag a later reader could match a reply by, so an exchange dropped with
its caller would leave its reply to answer the next request, and every
reply after it would be one late until a read timed out or the conduit
reopened. The cost is that the next request waits one round trip, or
one read timeout. A request still queued for the lock sends nothing
when its caller goes; a panic in the exchange reaches the caller.

Under the lock an exchange encodes the command (so a codec's state is
not changed by a request still queueing), discards buffered input if
the codec asks, writes the frame, and reads frames until one decodes
and matches or the skip budget runs out.

### Codec contract

- **The transport splits frames; the codec writes terminators.** The
  `FrameTransport` splits the stream into frames — up to a terminator on
  serial, one datagram on UDP. `encode` returns the whole frame, any
  terminator included (`\r` for Sky-Watcher, `}` for `qhy-focuser`'s
  JSON), written verbatim; `decode` receives what `recv_frame`
  returned, terminator included.
- **`matches` and `max_skip`** (defaults: true, and 0) skip unsolicited
  frames: up to `max_skip` frames that do not match are read past, and
  then the request fails with `SkipExhausted`. `qhy-focuser` matches a
  reply's index to its command and skips up to 5 position frames a
  moving focuser pushes.
- **`discard_before_send`** (default false) runs
  `FrameTransport::discard_input` just before the command's frame goes
  out, for a device that streams frames unasked; otherwise a frame that
  landed first reads as the reply, and after a write shows the state
  before it. A frame starting after the discard can still precede the
  reply, and the skip budget covers it only where `matches` tells it
  apart, so a driver confirms a readback that does not show its write.
  On the Gemini hub, which streams a status frame every 3 s, a frame
  starting some 65 ms after a set was measured showing the state before
  it ([its design doc](../services/gemini-powerbox-adv3-driver.md)). A
  failed discard is a wire failure.
- **`skip_undecodable`** (default false) skips, under the same budget,
  a frame that fails to decode while a command the codec names waits.
  By default such a frame is the device's garbled answer and the
  request fails at once, rather than waiting out a timeout for a reply
  that has come. A codec names a command better left to its timeout:
  the Gemini hub's identity query, the first request after the port
  opens and one whose timeout draws a retry. Each skip is a `debug!`
  event with the frame's bytes.
- **`Clone`**: each conduit gets its own codec, shared by its
  exchanges.

### Bounded I/O

Every `send_frame`, `recv_frame` and `discard_input` must complete or
fail in bounded time, whatever the device does. Closing a conduit takes
the command lock, so an operation that can block forever blocks the
reconnect and the shutdown that need the port; timing out the lock
instead would leave the handle open, the thing being released. Both
transports here wrap each send and receive in a `tokio::time::timeout`
(`DEFAULT_IO_TIMEOUT`, 5 s, unless the factory sets one), and the
serial discard never waits. A close waits out one whole request: the
write timeout plus `max_skip + 1` read timeouts.

Each frame is a `trace!` event under
`RUST_LOG=rusty_photon_shared_transport=trace`, with bytes outside
printable ASCII (and `\`) escaped as `\xNN` so a frame cannot drive a
terminal, capped at 256 bytes, unredacted.

## The no-client state

The state `on_last_disconnect` asserts must hold whenever nothing is
attached, and a device does not forget what it was doing when the
driver does. Three gates keep a client from commanding a device whose
stop is not confirmed.

### The startup gate

A cold `start()` runs `on_last_disconnect` on the conduit it has just
opened and handshaken, before publishing it — every time, not only when
a stop is recorded as owed. A reload builds a new `SharedTransport`, so
the instance that would remember an unlanded stop is gone; a process
restart keeps nothing; and the device goes on doing what it was doing.
A start that assumed it idle could hand its first client a mount in
motion. Nothing is published and the refcount is zero, so no client can
have commanded anything the assertion undoes. A promotion does not
assert: its conduit is live and may have a client on it.

A failed assertion fails the start and closes the conduit, freeing the
port for the next try. A command that did not complete on the wire
moved the connection's failure counter: "the last-disconnect state did
not land on the fresh conduit". A `NotAsserted` verdict with no wire
failure is the hook's word alone: "… was not asserted on the fresh
conduit and no request failed on the wire; see the hook's own log for
the cause". The first says the link failed under the hook; the second
names no cause, since the verdict carries none.

The assertion records the stop as owed before the hook runs and as paid
once it lands. A first cold start that fails leaves the transport lazy,
and without that record a caller that handled the error and called
`acquire()` would find nothing owed and get a session on a conduit
whose state was never asserted.

### The safety debt

The debt is a pair of counters, incurred and paid, outstanding while
they differ. It is incurred by a last-disconnect hook whose commands
failed on the wire or whose verdict was `NotAsserted`, in either mode;
by a guard when that hook panics; and by every assertion on a fresh
conduit, before its hook runs. An assertion that lands pays the
incurred count it covered. A flag would not do: a payer cannot hold the
cleanup out while it replays, and a cleanup whose stop fails meanwhile
moves the incurred count past what the replay covers, so its debt
survives the payment, where a flag would be cleared and lose it.

An outstanding debt is replayed on the next conduit whether or not a
client has attached — by a reconnect, the next lazy 0→1, and a cold
start in any case. It is about the device, not about who is connected,
and a client that arrived after the failed stop cannot have commanded
anything, because requests are refused while it stands. `shutdown()`
writes it off: carried into a later `start()`, which publishes a fresh
conduit and clears the reconnecting flag, it would fire a stale halt at
the first reconnect, under a live client. A stop lost at shutdown is
answered by the next cold start's assertion.

In `ServiceLifetime`, a last release whose stop did not land also takes
the transport out of service at once (reconnecting set, available
cleared) and wakes the supervisor to replay it. The wake matters for a
stop the device refused: that request completed, so nothing else raised
one. `LazyAcquire` closes the conduit there anyway, and a reconnecting
flag would strand later requests on a retry no supervisor makes.

### The request gate

`Session::request` refuses with `TransportError::Reconnecting` while
the reconnecting flag is set or a debt is outstanding, reading the debt
itself. The flags publish the debt and can briefly disagree with it: a
1→0 landing as a recovery is advertised records its debt and takes the
transport out of service, and the advertisement, which cannot take
`acquire_lock`, can write healthy flags over that. The request checks
both again after taking the conduit from the cell, so one let through
before a reconnect cannot go out on the replacement that reconnect is
about to assert the stop on. A transition beginning after that can let
a request out
on the conduit it is closing: a command on the way into a teardown,
followed by a replay on the replacement. An unavailable transport not
reconnecting answers "transport has been shut down" in
`ServiceLifetime`, and in `LazyAcquire`, where a failed reconnect
closed it, "transport is closed; it reopens once every session is
released".

### The publish gate

A successful reconnect is advertised — available set, then reconnecting
cleared, so a racing request sees `Reconnecting` and never the terminal
error — only if no debt is outstanding, and the debt is read again after
the flags are written; if it moved, the advertisement is withdrawn in
reverse order and the supervisor comes back to replay. The re-read is
sound because a cleanup increments the debt before it touches either
flag, all `SeqCst`, so any flag write the advertisement overwrote is
one whose increment the re-read sees.

## Reconnect

The supervisor owns recovery. It exists in `ServiceLifetime` only,
from `start()` to `shutdown()`, and waits on its cancellation, the
[reconnect signal](#the-reconnect-signal) and a tick at the reconnect
interval. A signal sets reconnecting and clears available, so clients
are refused at once. On any wake with reconnecting set, it waits until
an interval has passed since the last attempt from either entry point,
checks again (a manual attempt may have recovered the link), and runs
one attempt; an outage's first attempt runs at once unless one ran
less than an interval ago.

The interval is `DEFAULT_RECONNECT_INTERVAL`, 5 s, which no driver
changes: a brief unplug recovers in one or two attempts, and a device
left off is not reopened needlessly. `set_reconnect_interval` raises
anything under `MIN_RECONNECT_INTERVAL`, 50 ms, to it with a warning:
zero would make a dead device a busy loop against the hardware. The
floor holds however the wake came, because an attempt whose own
requests fail raises the signal from inside itself, and without it the
next attempt would cycle the port as fast as it opens.

Each attempt runs as a task, so a panic in a hook it awaits is a failed
attempt, not the end of the one task that retries. An outage is logged
once however long it lasts: a warning at its first failed attempt, with
the retry interval; `debug!` for each later one; and
`info!(failed_attempts, "transport link recovered")` when an attempt
succeeds after that warning. A link that recovers on its first attempt
was never warned about and logs its recovery at `debug!`. A device off
for the day fails over ten thousand attempts in sixteen hours at 5 s,
and staying down is not news.

### An attempt

Attempts are serialised. One attempt:

1. stamps the cadence clock and notes whether the refcount is zero,
   which settles the no-client replay: a client arriving later cannot
   command anything until the attempt returns;
2. cancels the poll task and closes the conduit being replaced
   ([Giving the port back](#giving-the-port-back));
3. opens a fresh conduit and handshakes it in isolation, while sessions
   are refused;
4. builds the poll future, catching a panic, which fails the attempt
   and closes the replacement;
5. swaps the replacement into the cell under the slot lock, only if
   that cell is still the slot's — after a teardown emptied the slot,
   the replacement is closed and the attempt fails;
6. spawns the poll task on it;
7. replays `on_last_disconnect` if a debt is outstanding, in either
   mode, or in `ServiceLifetime` if the refcount was zero at step 1;
8. commits, or fails, closing the replacement and cancelling its poll
   task, if a request failed on the wire since the handshake, the
   replay said `NotAsserted`, or a debt is outstanding once the replay
   is paid — a 1→0 can land anywhere in the attempt and run its hook
   on this conduit.

The supervisor then advertises the recovery through
[the publish gate](#the-publish-gate). The respawned poll task is
outside the checks: its first request may land either side of them,
and a failure there raises the signal like any other.

### Giving the port back

An attempt closes the old conduit before opening the new one. A Windows
COM port refuses a second open while the process holds the first
handle, so opening first would make the open that should release the
old handle the one that fails, attempt after attempt. Closing first
loses nothing: the transport is already out of service.

`Connection::close` closes explicitly rather than waiting for the last
`Arc<Connection>` to drop: the holders at that moment — live sessions,
an aborted poll task — are not something a reconnect or shutdown can
bound, and on Windows the handle outlives the drop besides
([Opening a serial port](#opening-a-serial-port)). It takes the command
lock, so an exchange in flight finishes first, and drops the stream
under it, so a second closer cannot see the conduit gone and reopen the
port while the first is still dropping it. Later requests on that
connection fail as closed. A cold start and a lazy 0→1 apply the rule
to what an earlier lifecycle left: before they open, they take and join
the supervisor, cancel the poll task, reset the cadence clock and close
any conduit in the slot. A conduit nothing else holds yet, one whose
open fails before the publish, may be dropped instead.

### The reconnect signal

Each `SharedTransport` has one `Notify`, attached to every connection
it builds. A request whose send, receive or discard fails on the wire
counts a wire failure and fires it; one that found the conduit closed
counts one without firing, a closed conduit being where a teardown
leaves things; a codec error or an exhausted skip budget does neither,
because the device answered and a reconnect cannot fix a protocol
mismatch. A `ServiceLifetime` 1→0 whose stop did not land fires it too.
In `LazyAcquire` nothing listens.

`Notify` keeps one permit, so a wake raised before the supervisor waits
still counts, and every open drains it: before a cold open, where
anything pending is an earlier lifecycle's, and after every handshake,
where a probe the handshake ignored would otherwise wake a supervisor
to tear down the conduit it just accepted. The drain relies on
`Notified::enable` consuming a stored permit, which a unit test pins.

### `reconnect_now`

`SharedTransport::reconnect_now` runs one attempt and returns its
result. It is a test hook behind the `test-util` feature, which
production builds do not compile: nothing joins the attempt it starts,
so a teardown cannot wait it out, and the attempt can publish a
conduit, respawn a poll task or advertise the transport after
`shutdown()` has torn the lifecycle down. The crate's tests reach it
through a dev-dependency on the crate itself, and Bazel builds them
against a `testonly` twin of the library with the feature on
([testing.md](../skills/testing.md) §6.13, "A Test-Only API Is a
`test-util` Feature").
A failed manual attempt with no supervisor clears the reconnecting
flag, leaving a closed conduit for the next 0→1 to reopen.

### What a teardown does not reach

`shutdown()` gives the supervisor 5 s and then aborts it, which does not
stop an attempt task it was awaiting. An attempt wedged in `open()` or
a hook outlives the teardown and can hold the port the next lifecycle
wants. If it publishes before `shutdown()` empties the slot,
`shutdown()` closes the replacement; after, it finds the slot empty and
closes its own. Reaching it would need its handle in lifecycle state,
and only an attempt that does not yield for the whole join gets here,
such as a synchronous open that hangs.

## Opening a serial port

`open_serial_port(port, baud_rate)` is the one place a serial port is
opened, and every driver's serial factory calls it. It opens
`tokio_serial::new(port, baud_rate)` in async mode with serialport's
defaults: 8 data bits, no parity, one stop bit, no flow control, DTR
left alone, and exclusive on Unix. A device that needs a modem line
gets it from its factory on the opened stream, as `dsd-fp2` raises DTR.
The builder sets no timeout: the transport's per-operation timeouts are
the deadline, and a port-level timer too would give two answers to
which fired. An open error becomes `TransportError::Open` carrying the
`tokio_serial::Error` itself, so the cause survives as its source.

The open is retried on a ladder of 20, 50, 100 and 200 ms — five
attempts, under half a second of waiting — each failure a `debug!` that
names no cause, since the ladder runs on every open error. It exists
for Windows, where reopening a port this process has just closed (the
supervisor after a USB glitch, a reload rebuilding the service) races
the close: `tokio-serial` drives a COM port through mio's named-pipe
type, which holds the handle in a refcounted cell, and dropping the
stream cancels the pending overlapped read but keeps a reference for
the cancellation's completion packet. `CloseHandle` runs only once the
reactor processes it, so a reopen in the same breath finds the port
held: `Access is denied`. On rig2 a reload of `upbv2-driver` was
refused this way 200 µs after its previous iteration had released the
port
([issue 1222](https://github.com/rusty-photon/rusty-photon/issues/1222)).
No API waits for the handle to go, and each sleep parks the runtime,
which lets the reactor run, so the first delay normally does the work.
A port another process holds fails with the OS's error half a second
later. Unix closes at drop.

### Writing without a flush

`SerialFrameTransport::send_frame` writes with `write_all` and does not
flush. On Unix `tokio-serial`'s flush is a `tcdrain(2)` that blocks
until the UART has sent the bytes, synchronously inside one poll, where
`tokio::time::timeout` cannot preempt it, so a USB-CDC port whose
firmware never services its endpoints — a bus-powered adapter in front
of an unpowered device — would wedge the write forever. Without the
flush, a device that never answers is caught by the read timeout.

`recv_frame` reads up to and including the terminator and fails with
`Framing` as soon as a frame would pass `max_frame_size` without one.
`discard_input` drops what the `BufReader` holds, then reads the stream
dry without waiting — each read polled once, the first `Pending` ending
it — for at most 16 frame-sized reads, so a device streaming faster
than the drain cannot hold it there.

### UDP

`UdpFrameTransport` wraps a `connect()`ed `UdpSocket`: one datagram per
`send_frame` and per `recv_frame`, so a protocol that requires one frame
per packet keeps it by construction. A frame over `max_frame_size` is
refused before sending, and a short send is an error.

## Stamped values

A caller stores what its request brought back after its turn on the
wire ends, outside the lock, so two stores can land in the opposite
order to their reads: a poll slow to run again stores the frame it read
just before a write after the write's readback, and puts the old value
back. `ReadStamp::admit(read_at)` accepts a value unless the held one
was read strictly later, and only then moves the stamp; a read at the
held instant is accepted, so reads on a paused test clock store in
turn.

`Stamped<T>` is a value and its stamp in one: the value is private and
`offer(value, read_at)`, which says whether it stored, is the only way
in. `T = Option<_>` keeps "never read" apart from "read, and absent"; a
bare `ReadStamp` is for answers that move the stamp with no value.
Stamps are monotonic `tokio::time::Instant`s, and exchanges on one
connection never overlap, so a stamp from the exchange is in wire
order. Which driver keeps what, and which instant dates a value:
[workspace.md § Values that carry their time](../workspace.md#values-that-carry-their-time).

## Wire timing

`request_timed`, on `Session`, `WhileOpen` and `Connection`, returns a
`WireTiming` with the response: three `tokio::time::Instant`s, all
taken inside the command lock, so time queued behind other callers is
in none of them.

- `sent_at`, just before the frame is handed to `send_frame`, after its
  trace event. A busy host can delay the write but not move the stamp
  from it: whatever comes between makes it early, never late.
- `written_at`, just after `send_frame` returns. `send_gap()` is
  normally microseconds; more says the task was held up on its way to
  the write.
- `received_at`, just after the `recv_frame` of the answering frame,
  not of a frame skipped before it. The reply reached the host earlier
  by however long the host took to notice it.

The device handled the command within `[sent_at, received_at]`
(`round_trip()`), provided the answer is this command's reply; a
protocol with untagged replies can be handed one left from an earlier
exchange, and then `round_trip()` is shorter than the link allows.
Where in the interval the device acts is the protocol's, and a driver
dating a value the device latched picks the instant. Host load lands on
the reply's side, so a device that latches on receipt is dated better
by `sent_at`. The request-and-reply serial drivers date by
`received_at`; the GTi dates a position by `sent_at`, because with
ConformU running on the same Raspberry Pi, replies on its poll task
were measured noticed 40–61 ms late, median 42 ms
([star-adventurer-gti.md](../services/star-adventurer-gti.md#encoder-samples-and-the-read-instant)).
The crate's timing tests use a 41 ms reply delay for that reason.

## Module layout

| File | Holds |
|------|-------|
| `src/lib.rs` | Re-exports; `BoxFuture`, so the crate needs no `futures` |
| `src/shared.rs` | `SharedTransport`: both lifecycles, the safety debt, the supervisor and the attempt; `DEFAULT_RECONNECT_INTERVAL`, `MIN_RECONNECT_INTERVAL` |
| `src/session.rs` | `Session`, `WhileOpen`, `Hooks`, `StateAssertion`, the connection cell |
| `src/connection.rs` | `Connection`: the command lock, the exchange, the wire-failure count and signal, wire tracing; `WireTiming` |
| `src/codec.rs` | `Codec` |
| `src/transport.rs` | `FrameTransport`, `TransportFactory`, `SerialFrameTransport`, `UdpFrameTransport`, `open_serial_port`, `DEFAULT_IO_TIMEOUT` |
| `src/read_stamp.rs` | `ReadStamp`, `Stamped` |
| `src/error.rs` | `TransportError`, `SessionError` |

## Testing

Unit tests sit with what they pin: in `shared.rs` the request gate
against flags set by hand to a state no caller can schedule, the
publish gate, the outage counts and `Notify`'s permit semantics; in
`connection.rs` the exchange outliving its caller, the instants, the
discard and the undecodable skip; in `transport.rs` framing, the absent
flush, timeouts and the retry ladder.

| File in `tests/` | Pins |
|------|------|
| `race.rs` | Concurrent first acquires open and handshake once; a session queued behind another's exchange is timed from its own |
| `rollback.rs` | A failed 0→1 — factory, handshake, a panic in either, a panicking poll constructor — leaves the refcount at zero, and the next acquire opens again |
| `idempotent.rs` | Sequential acquires share one open; a reacquire handshakes again; the `Drop` release; availability by phase |
| `while_open.rs` | The poll task's lifetime in `LazyAcquire`; the abort of one that ignores cancellation |
| `lifecycle.rs` | `start` and `shutdown`, the startup gate's two failures, promotion, the `ServiceLifetime` release, releasing the port with sessions alive, panicking hooks, the quiesce before a cold open, tolerated probes |
| `reconnect.rs` | The supervisor and `reconnect_now`: the cell swap, `Reconnecting`, recovery by signal and by tick, close-before-open, the no-client replay, the debt across reconnects and lazy opens, refused stops, the cadence floor, panics mid-attempt |

`tests/common/mod.rs` holds an echo codec and transport, a counting
`ProgrammableFactory`, and an `ExclusiveFactory` that refuses to open
while a transport it handed out is alive, as a Windows COM port does: a
test of close-before-open passes on the programmable one whether the
order is right or wrong. A driver's own tests use `transport-doubles`:
`HoldingFactory` for store-order races ([testing.md](../skills/testing.md)
§6.14), `FailingFactory` to fail the next frame sent.

## Dependencies

`async-trait`, `derive_more`, `thiserror`, `tokio`, `tokio-serial`,
`tokio-util` (the cancellation tokens) and `tracing`. The `test-util`
feature compiles `reconnect_now` and what only it uses.
