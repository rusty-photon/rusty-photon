# svbony-camera on the field rig — SV605CC, 2026-10-01 (Gain and Offset armed by `StartExposure`)

Recorded ConformU run against the field rig's SV605CC, the same physical unit
as every earlier svbony record, taken on the change that caches `Gain` and
`Offset` at their setters and has every exposure arm them, the gain after the
exposure write that clears the SDK's auto-exposure gate
([GO1/GO2/GO5](../../services/svbony-camera.md#gain--offset--readout)).
Beside it, an A/B of what a set made **during** an exposure does to that
exposure's frame, against `main` and against the change.

As in the [2026-08-23 record](../2026-08-23-svbony-camera-sv605cc-rig-reconnect/README.md),
the packaged service was stopped for the run and a **source build** ran in
its place, as the `rusty-photon` user (the camera's USB node is group
`rusty-photon`, mode 0660) from a scratch working directory, bound to
`127.0.0.1` and reached from the dev box through an ssh tunnel — no LAN-open
port and no credential. The packaged service was started again afterwards.

## What was tested

| | |
|---|---|
| Commit | [`b519654e`](https://github.com/rusty-photon/rusty-photon/commit/b519654e) (branch `fix/camera-gain-offset-cache-1336`), on `main` at `c13e5cae` — built on the rig from a worktree at `c13e5cae` with the branch's diff applied (tree `1e01c7ac`, identical to the commit's) |
| A/B baseline | `main` at [`c13e5cae`](https://github.com/rusty-photon/rusty-photon/commit/c13e5cae), built the same way |
| Service | `svbony-camera`, default features, `cargo build -p svbony-camera` (dev profile) with `SVBONY_SDK_LIB_DIR=/usr/lib/rusty-photon`, run with that directory on `LD_LIBRARY_PATH`; rustc 1.98.1 (48a229cea 2026-09-01) |
| SDK | `libSVBCameraSDK.so` v1.13.4 `armv8` at `/usr/lib/rusty-photon`, placed by the packaged SDK helper — sha256 `d8c6c1848d4cc95de6594449f43ee339693dfe52a8341bc857a3fe183d16e0e3` |
| Platform | Raspberry Pi 5 (aarch64), Debian GNU/Linux 13 (trixie), kernel 6.18.34+rpt-rpi-2712 — the telescope field rig |
| Camera | SVBONY SV605CC, colour, 2976×3000, gain `[0, 600]`, offset `[0, 100]`, hardware serial `0123481353808C03EE2512150035`; seeded at connect at gain 0, offset 0 (the device defaults the connect restores) |
| ConformU | **4.5.0** build 53834.49ab847 on the dev box, against `http://127.0.0.1:11143/api/v1/camera/0` (the tunnel) |

## Verdicts

| Suite | Result |
|---|---|
| `alpacaprotocol` | *"no errors, issues or information alerts"* — [log](alpacaprotocol.log) |
| `conformance` | *"no errors, warnings or issues found"*; every member within its target response time — [log](conformance.log), [results](conformance-results.json) |

**Re-run on [`d2896cde`](https://github.com/rusty-photon/rusty-photon/commit/d2896cde)**,
the review fix that publishes the gain and offset last in the handshake and
keeps a handshake a disconnect overtook from filling them (GO4) — a change to
the connect path, so run on the camera rather than taken on the mock's word:
built and run the same way, both suites clean again, all four counts 0
([`alpacaprotocol`](alpacaprotocol-d2896cde.log),
[`conformance`](conformance-d2896cde.log),
[results](conformance-results-d2896cde.json)). The packaged service was
stopped for about 3 minutes for it.

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
are all **0**. ConformU's gain and offset checks read the seeded values, write
each minimum and maximum, and are refused `InvalidValue` one beyond each
bound, all answered from the cache; its exposures then arm the gain it left,
after their own exposure writes.

## A set during an exposure

Method: three 3 s frames per control, the rig's telescope dark. A at value
*a*; B started at *a*, with the value set to *b* 1.2 s into its exposure; C,
the next frame, at *b*. Means over every 97th pixel of the raw Bayer frame
(the median of a colour frame jumps between channels, so the mean is the
figure to read).

| | Offset 10 → 33 (gain 0), mean | Gain 0 → 300 (offset 0), mean |
|---|---|---|
| `main` — A | 529.5 | 5.5 |
| `main` — B (set in flight) | **1743.1** | **169.7** |
| `main` — C | 1743.0 | 169.2 |
| change — A | 529.5 | 6.8 |
| change — B (set in flight) | **530.9** | **6.7** |
| change — C | 1740.4 | 94.3 |

On `main` both land in the frame in flight, whole. On the change neither
does, and the next frame takes both.

The set itself also changed: on `main` a `PUT Gain` or `PUT Offset` made
during the exposure took 77–79 ms, waiting for the camera behind the
capture's `SVBGetVideoData` slices; on the change it took 7–8 ms, the cache
alone.

## What arming costs, and what it persists

Ten 0.01 s frames at unchanged settings. `StartExposure` returns before the
capture arms:

| | `StartExposure` call, median (max) | `StartExposure` → `ImageReady`, median |
|---|---|---|
| `main` | 6.2 ms (6.7) | 644 ms |
| change | 6.4 ms (7.2) | 635 ms |

With auto-save off (C1a), arming persists nothing: the SDK's
`U3SM900C-AST_Cfg_A.bin` and `_Cfg_SAVE.bin` in the working directory were
last written at the conformance suite's connect (18:26:07 local), when the
connect restores the defaults, and not by any of the exposures after it
(18:26:11–18:26:36).
