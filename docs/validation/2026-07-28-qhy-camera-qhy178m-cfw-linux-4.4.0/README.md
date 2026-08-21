# qhy-camera on Linux — QHY178M + CFW, 2026-07-28 (abort-contract fix)

Recorded Linux ConformU run against the same physical QHY178M and CFW as the
[earlier Linux](../2026-07-28-qhy-camera-qhy178m-cfw-linux/README.md) and
[Windows](../2026-07-28-qhy-camera-qhy178m-cfw-windows/README.md) records, taken
after the fix that stops the driver cancelling an exposure under a live readout.

This run exists for two reasons: it is the hardware evidence for that fix, and it
brings Linux onto **ConformU 4.4.0**, matching the Windows record (the earlier
Linux record was taken on 4.3.0, the newest release available at the time).

## What was tested

| | |
|---|---|
| Commit | [`e7ce4a0e`](https://github.com/rusty-photon/rusty-photon/commit/e7ce4a0e) — branch `fix/qhy-abort-readout-contract-755`, i.e. `origin/main` plus the abort/readout contract fix. The binary tested was built from `0f6aaa2e`, the same change before the branch was rebased onto main; `services/qhy-camera/` and `crates/qhyccd-rs/src/` are byte-identical between the two |
| Service | `qhy-camera`, **real-SDK** build (default features, no `QHYCCD_SKIP_NATIVE_LINK`) |
| Build | `cargo build --release -p qhy-camera`; rustc 1.96.0 (ac68faa20 2026-05-25) |
| SDK | QHYCCD SDK **26.06.04** — `/usr/local/lib/libqhyccd.so` → `libqhyccd.so.26.6.4.16`, sha256 `f51b92f9189fae7707e98ad334cf52d3c1493a6485f33394b39a18a3f4d5c738` |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.1.4-204.fc44 |
| Camera | QHY178M, 3056×2048, mono, `MaxADU` 65535 — SDK id `QHY178M-222b16468c5966524` |
| FilterWheel | The CFW on that camera's port, 7 slots, same physical `OpenQHYCCD` handle — `CFW-QHY178M-222b16468c5966524` |
| ConformU | **4.4.0** build 52526.0ad7f21, run against `http://127.0.0.1:11121/api/v1/camera/0` and `.../filterwheel/0` |

## Verdicts

Both devices, both suites, clean:

| Device | `alpacaprotocol` | `conformance` |
|---|---|---|
| Camera | 0 errors, 0 issues, 16 information messages — [log](alpacaprotocol-camera.log) | *"no errors, warnings or issues found"*, all 70 timed members within target — [log](conformance-camera.log), [results](conformance-camera-results.json) |
| FilterWheel | *"no errors, issues or information alerts"* — [log](alpacaprotocol-filterwheel.log) | *"no errors, warnings or issues found"*, all 33 timed members within target — [log](conformance-filterwheel.log), [results](conformance-filterwheel-results.json) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
are **0** in both results files.

The camera's 16 informational items are the same set as both earlier records —
the protocol suite's casing probes against `ImageArray` / `LastExposureDuration`
/ `LastExposureStartTime` before any exposure exists, answered with in-protocol
ASCOM errors over HTTP 200. `UniqueID`s are unchanged across all three runs.

## Abort-path exercise

The conformance suite covers `AbortExposure`, but the fix this run validates is
specifically about *when* the SDK cancel is issued relative to the readout, so
the following were run against the same binary beyond ConformU's own coverage:

- **5 × full `conformance`** on the camera — all clean, 70 timed members each.
- **10 × abort mid-integration, each followed immediately by a real frame.**
  Abort returned in ~17 ms, the camera returned to `Idle`, and the next frame
  completed every time.
- **20 × abort raced straight into the readout window** (1 ms exposure, abort
  issued with no delay). Each abort took a consistent 1.205–1.219 s — the driver
  waiting out the ~12.5 MB full-frame readout before issuing the SDK cancel,
  which is the behaviour the fix exists to produce. A real frame still completed
  afterwards.

`Disconnect` completed in 0.88 s. No `still inside the SDK` warnings and no
mid-exposure SDK errors appeared in the service log across any of the above.

None of these reproduced the wedge tracked in
[issue #755](https://github.com/rusty-photon/rusty-photon/issues/755). That is
encouraging rather than conclusive: the wedge was intermittent, and only extended
real-session use will confirm it is gone.
