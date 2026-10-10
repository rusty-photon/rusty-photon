# plate-solver

rp-managed service that wraps the ASTAP CLI and exposes a narrow HTTP
solve API to `rp`. Operator installs ASTAP separately (BYO per ADR-005);
`plate-solver` ships no ASTAP binary or index database.

See [`docs/services/plate-solver.md`](../../docs/services/plate-solver.md)
for the design contract and [`docs/plans/archive/plate-solver.md`](../../docs/plans/archive/plate-solver.md)
for implementation sequencing (archived 2026-05-15).

## Status

All eight phases of the [implementation plan](../../docs/plans/archive/plate-solver.md#phases)
have shipped: design doc, crate scaffolding + `AstapRunner` trait +
`.wcs` parser, BDD scenarios, HTTP server + supervision,
process-supervisor recipes, nightly cross-platform real-ASTAP smoke,
hint-plumbing verification, and LGPL §4/§6 review under BYO. The
service exposes `POST /api/v1/solve` and `GET /health`, supervises
each spawned `astap_cli` child with the documented graceful-signal
→ 2 s grace → force-kill escalation, and serializes overlapping
solves through a single-flight semaphore. ASTAP is invoked as an
operator-installed subprocess per request; no ASTAP binary or
index database is bundled.

## Operator install

ASTAP is operator-supplied. The
[`install-astap`](../../.github/actions/install-astap/action.yml) GitHub
action is the canonical install recipe — operators can follow the same
per-OS table by hand:

- Linux: download `astap_command-line_version_Linux_*.zip` from
  SourceForge, place `astap_cli` somewhere in `$PATH`.
- macOS: same, plus `xattr -d com.apple.quarantine astap_cli` on first run.
- Windows: download `astap_command-line_version_win64.zip`, extract.

The D05 star database (~100 MB) is required for typical 0.6°–6° FOV
imaging; download from the same SourceForge project.

Configure `plate-solver` to point at the install:

```json
{
  "server": {
    "port": 11131,
    "bind_address": "0.0.0.0"
  },
  "astap_binary_path": "/opt/astap/astap_cli",
  "astap_db_directory": "/opt/astap/d05",
  "default_solve_timeout": "30s",
  "max_solve_timeout": "120s"
}
```

The `server` block is the shared shape from
`crates/rusty-photon-server-config` — it also accepts optional `tls`
and `auth` blocks; absent both, the service speaks plain,
unauthenticated HTTP.

`astap_binary_path` and `astap_db_directory` are the only required
fields. Validation runs at startup; missing or unreadable values exit
the process non-zero so the operator's process supervisor surfaces the
misconfiguration rather than masking it with a silent retry.

## Running

```sh
plate-solver --config /etc/plate-solver/config.json
```

The wrapper prints `bound_addr=<host>:<port>` to stdout once it has
bound its listener. Test harnesses and wrapper scripts parse this to
discover the bound port when `port: 0` is configured; production
deployments should pin a fixed port so reverse proxies, firewall
rules, and rp's `plate_solver.url` config remain stable. Logs go to
stderr via `tracing-subscriber`; set `RUST_LOG=debug` for verbose
output.

Graceful shutdown: SIGTERM and SIGINT on Unix, Ctrl-C on Windows.

## Process-supervisor recipes

An OS-level process supervisor restarts the wrapper on **crash /
non-zero exit**. It pairs with Sentinel's health supervision (see
[Sentinel integration](#sentinel-integration) below): the supervisor
owns relaunch-on-exit, and its restart command (`systemctl restart …`,
`launchctl kickstart -k …`, `nssm restart …`) is exactly what Sentinel
shells out to when probes fail.

> **Hang recovery is *not* covered by these recipes alone.** systemd
> `Restart=on-failure`, launchd `KeepAlive`, and NSSM's auto-restart
> all fire on process exit, not on a hung-but-still-alive wrapper.
> A wedged wrapper that's still bound to its port will keep the
> supervisor happy. Hangs are covered by Sentinel's `/health`-polling
> supervision ([Sentinel integration](#sentinel-integration) below),
> or by any other external `/health` watchdog (Prometheus blackbox
> exporter, Nagios, a small cron-driven probe). The wrapper does not
> hang waiting for `astap_cli`: every request is bounded by a
> wall-clock deadline that escalates from a graceful signal to a
> force-kill of the child's whole process tree (SIGKILL to its process
> group / TerminateJobObject) after a 2-second grace.

### Linux / systemd

`/etc/systemd/system/plate-solver.service`:

```ini
[Unit]
Description=plate-solver
After=network.target

[Service]
ExecStart=/usr/local/bin/plate-solver --config /etc/plate-solver/config.json
Restart=on-failure
RestartSec=2
User=rp
Group=rp
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```

Restart command: `systemctl restart plate-solver`.
Tail logs:     `journalctl -u plate-solver -f`.

### macOS / launchd

This recipe uses a **per-user LaunchAgent**. For a system-wide
LaunchDaemon (root context, recommended for headless observatory
machines), put the plist in `/Library/LaunchDaemons` instead and
swap the log paths to `/var/log/plate-solver.{out,err}.log`.

`~/Library/LaunchAgents/com.rusty-photon.plate-solver.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>           <string>com.rusty-photon.plate-solver</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/plate-solver</string>
    <string>--config</string>
    <string>/etc/plate-solver/config.json</string>
  </array>
  <key>KeepAlive</key>       <true/>
  <key>StandardOutPath</key> <string>/Users/<USER>/Library/Logs/plate-solver.out.log</string>
  <key>StandardErrorPath</key><string>/Users/<USER>/Library/Logs/plate-solver.err.log</string>
</dict>
</plist>
```

(Replace `<USER>` with the actual home directory — plist values
don't expand `~` or `$HOME`.)

Restart command: `launchctl kickstart -k gui/$(id -u)/com.rusty-photon.plate-solver`.
Tail logs:        `tail -f ~/Library/Logs/plate-solver.err.log`.

### Windows / NSSM

[NSSM](https://nssm.cc/) wraps a Win32 binary as a Windows service.
By default NSSM doesn't redirect stdout / stderr, so the install
recipe also sets `AppStdout` / `AppStderr` and enables NSSM's
built-in log rotation:

```cmd
nssm install plate-solver "C:\Program Files\plate-solver\plate-solver.exe"
nssm set     plate-solver AppParameters "--config C:\ProgramData\plate-solver\config.json"
nssm set     plate-solver AppRestartDelay 2000
nssm set     plate-solver AppStdout "C:\ProgramData\plate-solver\stdout.log"
nssm set     plate-solver AppStderr "C:\ProgramData\plate-solver\stderr.log"
nssm set     plate-solver AppRotateFiles 1
nssm set     plate-solver AppRotateBytes 10485760
nssm start   plate-solver
```

Restart command: `nssm restart plate-solver`.
Tail logs (PowerShell): `Get-Content -Wait C:\ProgramData\plate-solver\stderr.log`.

## Sentinel integration

Sentinel supervises this wrapper through its
[service health supervision](../../docs/services/sentinel.md#service-health-supervision),
and on a packaged install it needs **no configuration at all**:
sentinel discovers the installed `rusty-photon-plate-solver` unit from
the service manager, derives the `GET /health` probe from this
service's own config file, and after three consecutive failures
(non-200, timeout, or connection refused) runs
`systemctl restart rusty-photon-plate-solver` — with doubling backoff
between attempts and a Pushover notification for every autonomous
restart. This covers both crashes and hangs; the OS process supervisor
(recipes above) remains the relaunch mechanism the restart drives.
Hand-run wrappers (`cargo run`, the ad-hoc recipes above under
non-packaged names) are not under the service manager and are
therefore not supervised.

Defense in depth around it:

- Hangs in the *child* `astap_cli` process are bounded by the
  wrapper's per-request deadline; the wrapper itself doesn't hang on
  a wedged solve.
- `rp`'s HTTP client to the wrapper has its own outer timeout
  (`plate_solver.timeout_secs` in rp config) — even if the wrapper's
  internal deadline regresses, rp does not hang on a `plate_solve`
  call.

## Coordinates and units

- `ra_hint` and `ra_center`: **decimal degrees, 0–360** (matches FITS
  `CRVAL1`). The wrapper converts `ra_hint` to ASTAP's expected
  decimal hours (`degrees / 15`) before spawning.
- `dec_hint` and `dec_center`: **decimal degrees, −90 to +90**. The
  wrapper converts `dec_hint` to ASTAP's south-pole-distance
  (`90 + dec`) before spawning.
- `pixel_scale_arcsec`: arcseconds per pixel.
- `rotation_deg`: degrees.
- `wcs_matrix`: nested object with `crpix1`/`crpix2` (FITS 1-based
  pixels) and `cd1_1`/`cd1_2`/`cd2_1`/`cd2_2` (degrees per pixel),
  read verbatim from the `.wcs` sidecar. `null` unless the sidecar
  carries all six keys — never synthesized from CDELT/CROTA2.

Full HTTP contract in [`docs/plans/archive/plate-solver.md`](../../docs/plans/archive/plate-solver.md)
§"HTTP contract".
