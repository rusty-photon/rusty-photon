# rp's OmniSim fork: the releases its BDD suites pinned, 2026-05 to 2026-11

A record, moved whole from `docs/services/rp.md` on 2026-11-19: the
account of the OmniSim fork releases the BDD suites pinned — the `467.x`
series, which gives every test process a private simulator, and the
`326.x` series of `TelescopeHardware` fixes for the `center_on_target`
slew hang, with the issues each answered. It stood under
[rp.md § Prerequisites](../../services/rp.md#prerequisites); that section
says which OmniSim a BDD run needs, and
[omnisim.md](../../references/omnisim.md) states what the fork changes and
why.

**CI pins a patched fork**, not upstream: `REPO` and `VERSION` in
`.github/actions/install-omnisim/omnisim.mjs` name
[`ivonnyssen/ASCOM.Alpaca.Simulators` `v0.5.0-467.3`](https://github.com/ivonnyssen/ASCOM.Alpaca.Simulators/releases/tag/v0.5.0-467.3).
The `467.x` releases (issue #467) add the `--multi-instance` flag (467.1,
skips upstream's machine-global single-instance mutex; 467.3, also skips
the Windows tray icon, whose identity is just as machine-global and which
instances starting together crash on — `docs/references/omnisim.md`
§ "The Windows tray icon is machine-global too") and the
`OMNISIM_SETTINGS_DIR` env var (467.2, re-roots the profile store per
instance on every platform — the default store is not redirectable on
Windows or macOS). The harness always spawns OmniSim with both, so **the
fork is a hard requirement for BDD runs now**, local ones included: every
test process (parallel suites, `rp:bdd` shards) gets a private simulator
on its own port with a private profile store. It also carries the
`326.x` series of `TelescopeHardware` fixes
for the `center_on_target` slew-state hang/flake (issues #326, #319):
326.1/.2 put the slew-engine writers and the
`IsSlewing`/RA/Dec/`AtPark`/`SlewState` readers under `hardwareLock`;
326.3 disposes the per-`Init()` slew timer (it leaked one live timer on
each per-scenario "restart to clean state" reset, accumulating tick sources
that raced the single static slew engine) and resets the slew state —
including the `slewing` flag — under `hardwareLock`. Those addressed the
locking/leak races but **not** the underlying wedge: 326.4 fixes it
(issue #319). The real cause is geometric — a GEM goto whose
shortest-path primary rotation crosses the hour-angle software limit
(`180 + hourAngleLimit`) gets undone by `CheckAxisLimits` every tick, so
the slew never finishes and `IsSlewing` stays `true` forever.
`center_on_target`'s sync-then-slew-to-near-the-same-coords triggers it
for the `RA < 180` off-target row at certain sidereal times (hence the
intermittent CI flake). 326.4 makes `DoSlew` take the limit-avoiding
rotation to the *same* target (pier side unchanged, so ConformU stays
clean) and adds a no-progress guard. Reverting `REPO` and `VERSION` to
upstream
[`v0.5.0`](https://github.com/ASCOMInitiative/ASCOM.Alpaca.Simulators/releases/tag/v0.5.0)
is no longer a one-line change: upstream has no `--multi-instance`, so
the `bdd-infra` spawn path (and the parallel/sharded BDD scheduling
built on it) would have to be reverted too. For local runs, use the
pinned fork — upstream `v0.5.0` lacks `--multi-instance` (every BDD
suite fails at OmniSim spawn) and still carries the #326 races and the
sidereal-time-gated #319 wedge.
