# OmniSim (ASCOM Alpaca Simulators) Reference

OmniSim is the [ASCOM Alpaca Simulators](https://github.com/ASCOMInitiative/ASCOM.Alpaca.Simulators) — a multi-device ASP.NET Core app that exposes simulated ASCOM hardware (telescope, camera, focuser, filter wheel, etc.) over the standard Alpaca HTTP API. We use it as the device under test in our BDD suites (`crates/bdd-infra/src/rp_harness/omnisim.rs`).

The simulator is *faithful* to real hardware behavior in many ways that aren't immediately obvious from the ASCOM spec, and those behaviors have caused us several days of debugging. This doc captures what we've learned.

## Single-instance enforcement — and our `--multi-instance` escape hatch

Upstream OmniSim acquires a hardcoded global named mutex on startup:

```csharp
// ASCOM.Alpaca.Simulators/Program.cs
private static Guid ApplicationGUID = new Guid("{1389A00E-006F-4117-8930-EAFCAA7DC397}");
string mutexId = string.Format("Global\\{{{0}}}", ApplicationGUID);
using (var mutex = new Mutex(false, mutexId, out bool createdNew))
{
    hasHandle = mutex.WaitOne(10, false);
    if (hasHandle == false) {
        // forward args to first instance via named pipe, then exit
        throw new TimeoutException("Timeout waiting for exclusive access");
    }
    ...
}
```

**Upstream implications:**

- Only one OmniSim per machine. The GUID is hardcoded — no upstream CLI
  flag, env var, or config to override it. On Linux/macOS the mutex is
  backed by a file under `/tmp/.dotnet/shm/global/` (that's why an isolated
  `/tmp` — a container, or a mount namespace — defeats it); on Windows it's
  a kernel object.
- A second instance does *not* run independently. It connects to the first
  instance's named pipe (`{2249563F-E844-4264-8956-73AC7A44BEA0}`, also
  hardcoded), forwards its CLI args, and exits.

If you see `Timeout waiting for exclusive access` in OmniSim's stderr,
another instance is already running.

**Our fork lifts this** (issue #467): `ivonnyssen/ASCOM.Alpaca.Simulators`
release `v0.5.0-467.1` adds a `--multi-instance` flag that skips the mutex
and the named-pipe server for that process, and `v0.5.0-467.2` adds the
`OMNISIM_SETTINGS_DIR` environment variable, which re-roots the profile
store per instance on **every** platform. Both are needed for concurrent
instances: the settings write-back is unsynchronised, and the default
store location is not redirectable per-process on Windows
(`%USERPROFILE%\.ASCOM` via SHGetKnownFolderPath) or macOS
(`~/Library/Application Support` via NSSearchPath — .NET honors
`XDG_CONFIG_HOME` on other Unixes only; a shared store on macOS CI leaked
one suite's persisted telescope site into another's scenarios). Without
the flag and variable, behavior is exactly upstream's.
`bdd_infra::rp_harness::OmniSimHandle` always spawns with both, plus a
dynamically chosen `--urls` port, giving every BDD test process (including
every Bazel shard of `rp:bdd`) a private simulator. The `@serial` feature
tag still serializes scenarios *within* one test process, which share that
process's instance.

## Binding port

OmniSim is an ASP.NET Core app. Default port is **32323**. Override via either:

- CLI: `ascom.alpaca.simulators --urls "http://127.0.0.1:33333"`
- Env: `ASPNETCORE_URLS=http://127.0.0.1:33333`

Under upstream's single-instance guard a port override is moot if another
OmniSim is already running (the mutex blocks before the bind); with our
fork's `--multi-instance` it's how concurrent instances coexist.

## State persistence

OmniSim writes per-device settings to XDG config:

```
~/.config/ascom/alpaca/ascom-alpaca-simulator/
├── camera/v1/instance-0.xml
├── covercalibrator/v1/instance-0.xml
├── dome/v1/instance-0.xml
├── filterwheel/v1/instance-0.xml
├── focuser/v1/instance-0.xml
├── observingconditions/v1/instance-0.xml
├── rotator/v1/instance-0.xml
├── safetymonitor/v1/instance-0.xml
├── server/v1/instance-0.xml
├── switch/v1/instance-0.xml
└── telescope/v1/instance-0.xml
```

Upstream, the path is **not configurable**: on non-macOS Unix, .NET respects `XDG_CONFIG_HOME`, so re-rooting that env var redirects the state dir (nothing inside OmniSim documents this); on Windows the base is `%USERPROFILE%\.ASCOM` and on macOS `~/Library/Application Support/ascom` (ASCOM.Tools `XMLProfile` via `GetFolderPath`), neither redirectable by any env var. Our fork's `OMNISIM_SETTINGS_DIR` (`v0.5.0-467.2+`) re-roots the store on every platform, keeping XMLProfile's own layout under the given root. `bdd_infra::rp_harness` sets it to a private `bdd-infra-omnisim-<pid>-<n>/` dir (PID for cross-process uniqueness, a per-process spawn counter for concurrent spawns within one test binary) seeded from `crates/bdd-infra/omnisim-config/`, so concurrent instances never share a writable profile store (and test runs never touch your real profile dirs).

State is loaded on startup and persisted on shutdown. State persists across OmniSim restarts unless explicitly reset (see CLI flags / `/simulator/v1/.../reset` below).

## CLI flags

From `readme.md`:

| Flag | Effect |
|---|---|
| `--reset` | Resets all settings for the drivers and server (clears persisted state on startup) |
| `--reset-auth` | Resets authentication, allowing access without password |
| `--local-address` | Print the URL of the running instance (when invoked as a second instance) |
| `--show-browser` | Open the web UI in a browser (when invoked as a second instance) |
| `--urls <url>` | ASP.NET Core convention; override the bind URL |
| `--multi-instance` | **Fork-only** (`v0.5.0-467.1+`): skip the single-instance mutex + named-pipe server so several OmniSims can run concurrently (pair with distinct `--urls` and settings stores) |
| `OMNISIM_SETTINGS_DIR` (env var) | **Fork-only** (`v0.5.0-467.2+`): re-root the profile store to the given directory (XMLProfile layout underneath), on every platform |

`--reset` only takes effect at startup. To reset state in a *running* instance, use the OmniSim-only HTTP API.

## OmniSim-only HTTP API: `/simulator/v1/`

In addition to the standard ASCOM Alpaca API at `/api/v1/{device}/{n}/...`, OmniSim exposes a private namespace at `/simulator/v1/{device}/{n}/...` that is **not** part of the Alpaca spec. Source: [`SimulatorController.cs`](https://github.com/ASCOMInitiative/ASCOM.Alpaca.Simulators/blob/main/ASCOM.Alpaca.Simulators/Controllers/SimulatorController.cs).

The two operations available per device class are:

### `PUT /simulator/v1/{device}/{n}/reset`

Clears the device's persisted profile (`ClearProfile`) and re-initializes (`Init`). Equivalent to deleting the relevant `instance-N.xml` and starting over. Use this when you want to wipe stored *settings* (e.g., observer location, capability overrides).

### `PUT /simulator/v1/{device}/{n}/restart`

Reloads the device with its current persisted settings (`DriverManager.LoadX(0)`). Equivalent to "OmniSim has just started". Settings are preserved; runtime state (mount position, slew state, AtPark flag, tracking state) goes back to defaults. **This is what we use in BDD `before(scenario)` hooks** — it's fast (an HTTP PUT, ~ms), idempotent, and doesn't touch persisted settings.

Per-class endpoints (all support both `/reset` and `/restart`):

- `/simulator/v1/camera/{n}/...`
- `/simulator/v1/covercalibrator/{n}/...`
- `/simulator/v1/dome/{n}/...`
- `/simulator/v1/filterwheel/{n}/...`
- `/simulator/v1/focuser/{n}/...`
- `/simulator/v1/observingconditions/{n}/...`
- `/simulator/v1/rotator/{n}/...`
- `/simulator/v1/safetymonitor/{n}/...`
- `/simulator/v1/switch/{n}/...`
- `/simulator/v1/telescope/{n}/...`

Response format mirrors the standard Alpaca pattern (`ClientTransactionID`, `ServerTransactionID`, `ErrorNumber`, `ErrorMessage`).

## Behaviors that mirror real hardware

These are not bugs. They reflect how real ASCOM mounts (especially GEMs) behave, and OmniSim faithfully simulates them. They have all bitten us at some point.

### Telescope

- **`Slewing` (`IsSlewing`) is over-conservative.** It returns `true` if *any* of these are true: `SlewState != SlewNone`, an internal `slewing` flag, or `rateMoveAxes.LengthSquared != 0`. A prior `MoveAxis` call that left non-zero rate state will keep `Slewing` true even after a subsequent slew completes. **Conclusion:** poll `AtPark` (the canonical post-park signal) rather than `Slewing` to detect park completion. See `services/rp/src/mcp.rs::do_park_blocking`.

- **`AtPark` is the single source of truth for park completion.** It transitions to `true` in exactly one code path — the `SlewType.SlewPark` completion handler in OmniSim's slew loop.

- **Slew refuses to drive through forbidden zones.** OmniSim mirrors real-mount soft limits: the slew motion path-plans through reachable positions and will not traverse below-horizon or other forbidden ranges. This means a mount that has been *synced* into an impossible position (e.g., `SyncToCoordinates` to coords that map below the horizon at the current LST) cannot be parked from there — the park slew will start (`SlewState = SlewPark`, `Slewing = true`) but never advance, so `AtPark` never flips. Recovery requires `/simulator/v1/telescope/0/restart` (or a programmatic sync to a reachable position with tracking on first).

- **`SyncToCoordinates` requires `Tracking == true`.** Returns `InvalidOperationException` ("SyncToCoordinates is not allowed when tracking is False") otherwise. This is OmniSim-imposed; the standard ASCOM spec doesn't strictly require it, but real GEMs commonly do. Tests that call `sync_mount` against OmniSim must enable tracking first.

- **`Park()` clears `Tracking` on success** per ASCOM spec. Don't write code that depends on tracking surviving across a park.

- **`Unpark()` is instant and idempotent.** Just sets `AtPark = false`, no slew. Calling unpark when already unparked is a no-op, never an error.

- **Default startup position** is approximately altitude 38.9°, azimuth 165° (configurable via the setup UI). Above horizon for any reasonable observer/time, so a park slew from defaults always succeeds.

### Focuser

- **Default position is 25000, not 0.** OmniSim's focuser starts up
  (and re-initializes after `/simulator/v1/focuser/0/restart`) at
  position **25000** out of `MaxStep=50000`. New tests that pick a
  "neat" target like 5000 will appear to hang because of the next
  point.
- **Slew rate is ~400 steps/sec.** A `Move()` request blocks until
  `IsMoving == false`; the simulator advances the position at a
  fixed rate that works out to roughly 400 steps/sec on Linux. A
  20,000-step move (default → 5000) takes ~51 s wall-clock. Verified
  against OmniSim v0.5.0 (`PUT /api/v1/focuser/0/move` from the
  default position).
- **Implication for BDD scenarios:** with the per-scenario reset
  wired into `before(scenario)` (issue #149), the focuser is set
  back to 25000 before every scenario. Test target positions in
  `services/rp/tests/features/{focuser,auto_focus}.feature` are
  anchored near 25000 (typically `25000`, `24500`, `25500`, plus
  bounds like `24900..25100`) so each `move_focuser` call slews ≤
  ~500 steps and completes in ~1 s. Without that anchoring the rp
  BDD suite picks up ~5 minutes of pure focuser slew wait.
- **Don't fight the default — anchor near it.** The absolute
  position value is rarely what the test cares about; what matters
  is that the position changes in response to a tool call, lies
  within the configured bounds, and round-trips through
  `get_focuser_position`. Picking targets near 25000 satisfies all
  of that without paying the slew cost. Both feature files carry an
  inline comment to that effect.

### Filter wheel

- **Default position is 0.** Restart resets to slot 0. Slew between
  adjacent slots is ~500 ms; full traversal (slot 0 → 4) is ~2 s.
  Cheap enough that resetting between scenarios is free.

### Cover calibrator

- **Default state: cover closed (`CoverState=1`), calibrator off
  (`CalibratorState=1`).** The cover then transitions through
  `Moving (2)` → `Open (3)` over the configured `Cover Opening
  Time` (upstream default **5.0 s**); the calibrator transitions
  through `NotReady (4)` → `Ready (3)` over `Calibrator
  Stabilisation Time` (default **2.0 s**). Both kept asynchronous
  whenever those values are `> 0.5 s`; OmniSim flips to a
  synchronous-blocking response when the configured time is
  `≤ 0.5 s` (`SYNCHRONOUS_BEHAVIOUR_LIMIT`, see
  `CoverCalibratorSimulator.cs:19`).
- **Override via XML profile, not via HTTP.** OmniSim does not
  expose a setter for these timings — `SimulatorController.cs`
  only has `/reset`, `/restart`, and a read-only `/xmlprofile` —
  so we ship test-friendly defaults as a checked-in profile under
  `crates/bdd-infra/omnisim-config/ascom/alpaca/ascom-alpaca-simulator/covercalibrator/v1/instance-0.xml`.
  Before spawning the simulator, `bdd-infra` recursively copies
  that tree to `$CARGO_TARGET_DIR/bdd-infra-omnisim/` (with the
  prior copy wiped first to avoid leakage between runs) and
  points OmniSim at the target-tree copy via `XDG_CONFIG_HOME`.
  The simulator emits UniqueIDs and persists full default
  profiles for every other device on its first startup; those
  writes land in the build directory, so the checked-in source
  stays clean across normal test runs and `cargo clean` reaps
  the persisted state.
- **Test defaults.** Both timings are pinned to `0.6 s` — fast
  enough that BDD scenarios don't sit through 5-second cover
  slews, but still `> 0.5 s` so OmniSim keeps the asynchronous
  state-machine path alive (rp's polling code still gets
  exercised). Edit the XML to dial them in differently for local
  experiments.
- **Pair the XML override with rp's `poll_interval`.** rp polls
  cover/calibrator state every 3 s by default
  (`services/rp/src/config.rs::default_cover_calibrator_poll_interval`).
  Without overriding it, even a 0.6-second OmniSim transition
  ends up bounded by rp's 3-second poll.
  `bdd_infra::rp_harness::CoverCalibratorConfig` exposes a
  `poll_interval: Option<Duration>` field
  (`crates/bdd-infra/src/rp_harness/config.rs`) that the
  builder serialises into the rp config JSON; the actual
  100 ms override is set at the BDD step call sites that
  construct `CoverCalibratorConfig`
  (`services/rp/tests/bdd/steps/cover_calibrator_steps.rs`
  and
  `services/calibrator-flats/tests/bdd/steps/flat_calibration_steps.rs`).
  Production rp deployments keep the upstream 3-second default.

### Camera

- **Reset is instantaneous.** Restart re-instantiates the device
  but doesn't run any slow setup. The CCD parameters return to
  defaults; `Connected` goes back to false. rp re-issues
  `set_connected(true)` on its next startup, so callers see a
  short-lived disconnect window during the per-scenario reset and
  serialization (`@serial`) is required to keep concurrent
  scenarios from observing it.

## Cucumber-rs `@serial` tag

The `@serial` tag is natively recognized by cucumber-rs (`cucumber-0.22.1/src/runner/basic.rs:429`):

```rust
let which_scenario: WhichScenarioFn = |feature, rule, scenario| {
    scenario.tags.iter()
        .chain(rule.iter().flat_map(|r| &r.tags))
        .chain(&feature.tags)
        .find(|tag| *tag == "serial")
        .map_or(ScenarioType::Concurrent, |_| ScenarioType::Serial)
};
```

Scenarios with `@serial` (on scenario, rule, or feature level) run sequentially. Untagged scenarios run concurrent (up to 64 by default). All BDD feature files in this repo that touch OmniSim are tagged `@serial` because OmniSim itself is single-instance.

## Integration in this repo

- `crates/bdd-infra/src/rp_harness/omnisim.rs` — process management.
  `OmniSimHandle::start` is a singleton-spawning shim;
  `OmniSimHandle::reset_telescope` / `reset_camera` /
  `reset_filter_wheel` / `reset_focuser` /
  `reset_cover_calibrator` each wrap the matching
  `/simulator/v1/{class}/0/restart` endpoint, and
  `reset_all_devices` issues them sequentially. Every restart PUT
  additionally takes a process-wide mutex, so at most one restart is
  in flight per test process regardless of how many scenario hooks
  run concurrently — OmniSim's restart handler mutates
  unsynchronised static state, and concurrent restarts have both
  corrupted its device list (#171) and deadlocked the simulator
  outright (#431, the end-of-run burst of non-`@serial` scenarios
  on the Pi nightly).

- `services/rp/tests/bdd.rs` and
  `services/calibrator-flats/tests/bdd.rs` — the cucumber
  `before(scenario)` hook calls `reset_all_devices` to give every
  scenario a clean default state.

- All BDD feature files that touch OmniSim are tagged `@serial`
  (file-level) so the per-scenario reset can't disconnect a device
  while another scenario is mid-flight.

## References

- Source: [ASCOMInitiative/ASCOM.Alpaca.Simulators](https://github.com/ASCOMInitiative/ASCOM.Alpaca.Simulators)
- Single-instance check: [`Program.cs`](https://github.com/ASCOMInitiative/ASCOM.Alpaca.Simulators/blob/main/ASCOM.Alpaca.Simulators/Program.cs)
- Telescope hardware (slew/park internals): [`TelescopeSimulator/TelescopeHardware.cs`](https://github.com/ASCOMInitiative/ASCOM.Alpaca.Simulators/blob/main/TelescopeSimulator/TelescopeHardware.cs)
- OmniSim-only API: [`SimulatorController.cs`](https://github.com/ASCOMInitiative/ASCOM.Alpaca.Simulators/blob/main/ASCOM.Alpaca.Simulators/Controllers/SimulatorController.cs)
- Standard Alpaca API: see [`docs/references/ascom-alpaca.md`](ascom-alpaca.md)
